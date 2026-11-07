//! `topside serve`: the long-running half.
//!
//! Threads share the database:
//! - **updates** — long-polls Telegram: the operator's replies to gate
//!   messages become answer parts, the buttons close the gate;
//! - **outbox** — sends the queued events to Telegram, in order per project;
//! - **wakes** — delivers answers into T3 threads;
//! - **heartbeat** — tells `topside status` (and deploys) the service is alive,
//!   independent of how long a send takes.
//!
//! Every Telegram message topside sends is one message (no splitting), so an
//! event either happened or did not — a retry never leaves half of it behind.
//!
//! Only outbound connections: the machine needs no public port.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::builds;
use crate::chat_notes;
use crate::config::{Buttons, Ctx, TelegramConfig};
use crate::gates;
use crate::launcher;
use crate::store::{Build, Gate, OutboxItem, Question, Store};
use crate::t3;
use crate::telegram::{Bot, MAX_DOWNLOAD, MAX_TEXT, NetError, TgError};
use crate::util::{clip, clip_bytes, html, human_size, later, now, safe_name, tg_len};

/// Room under the 4096-unit limit (Telegram counts UTF-16 units).
pub const TEXT_LIMIT: usize = MAX_TEXT - 96;
const WAKE_ATTEMPTS: i64 = 8;
/// Local failures (an unreadable file, a project gone from the config) are given
/// this many tries. Telegram's transient errors and network errors are retried
/// for as long as it takes: a message is delayed, never lost.
const OUTBOX_ATTEMPTS: i64 = 30;
/// A request Telegram rejected as malformed fails the same way every time.
const PERMANENT_ATTEMPTS: i64 = 3;
/// An upload that times out this many times is replaced by a note with its path.
const UPLOAD_ATTEMPTS: u32 = 3;
/// Handling one update (e.g. downloading a video) is retried this many times.
const UPDATE_ATTEMPTS: u32 = 3;

/// Never panics: stderr is the journal's stream, which breaks when journald
/// restarts — a log line must not take a worker down.
fn log(msg: impl AsRef<str>) {
    use std::io::Write;
    let _ = writeln!(
        std::io::stderr().lock(),
        "{} {}",
        chrono::Local::now().format("%H:%M:%S"),
        msg.as_ref()
    );
}

fn open_store(ctx: &Ctx) -> Store {
    loop {
        match Store::open(&ctx.paths) {
            Ok(db) => return db,
            // A newer topside's database this one may not use: retrying would leave
            // the service up without a working thread; exit and be restarted.
            Err(e) if format!("{e:#}").contains("newer topside") => {
                log(format!("database: {e:#}; exiting to be restarted"));
                std::process::exit(70);
            }
            Err(e) => {
                log(format!("database: {e:#}; retry in 5 s"));
                std::thread::sleep(Duration::from_secs(5));
            }
        }
    }
}

pub fn serve(ctx: Ctx) -> Result<u8> {
    std::fs::create_dir_all(&ctx.paths.data_dir)?;
    let lock_path = ctx.paths.data_dir.join("serve.lock");
    let lock = std::fs::File::create(&lock_path)
        .with_context(|| format!("cannot create {}", lock_path.display()))?;
    if lock.try_lock().is_err() {
        bail!(
            "another `topside serve` is running ({})",
            lock_path.display()
        );
    }
    // Fail at start, not at the first message, when the database is unusable.
    let db = Store::open(&ctx.paths)?;
    let repaired = db.repair()?;
    if repaired > 0 {
        log(format!(
            "{repaired} row(s) written by an older topside fixed up"
        ));
    }
    let schema = db.schema_at_open();
    let released = db.release_stale_wakes()?;
    if released > 0 {
        log(format!(
            "{released} answer(s) were being delivered when the service stopped; sending again"
        ));
    }
    drop(db);

    let ctx = Arc::new(ctx);
    let bot = match ctx.telegram() {
        Some(tg) => {
            let bot = Bot::from_config(tg)?;
            if tg.allowed_users.is_empty() {
                log("telegram: allowed_users is empty — nobody can answer gates");
            }
            Some(Arc::new(bot))
        }
        None => {
            log("telegram is not configured: gates are answered with `topside gate answer` only");
            None
        }
    };
    {
        let ctx = ctx.clone();
        worker("heartbeat", move || heartbeat_loop(&ctx, schema))?;
    }
    {
        let ctx = ctx.clone();
        worker("wakes", move || wakes_loop(&ctx))?;
    }
    if let (Some(bot), Some(tg)) = (bot.clone(), ctx.telegram().cloned()) {
        let ctx = ctx.clone();
        worker("updates", move || updates_loop(&ctx, &bot, &tg))?;
    }
    log(format!(
        "topside serve {} started",
        env!("CARGO_PKG_VERSION")
    ));
    outbox_loop(&ctx, bot.as_deref());
}

// ---------------------------------------------------------------- workers

/// A worker thread the service cannot do without: if it ever stops (a panic),
/// the whole service exits, so systemd restarts it — a heartbeat that goes on
/// while answers are no longer taken would hide the failure.
fn worker(name: &str, f: impl FnOnce() + Send + 'static) -> Result<()> {
    let label = name.to_string();
    std::thread::Builder::new()
        .name(label.clone())
        .spawn(move || {
            // Nothing is used after a panic: the process exits (log() cannot panic).
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
            log(format!(
                "{label} thread {}; the service exits to be restarted",
                if outcome.is_err() {
                    "panicked"
                } else {
                    "stopped"
                }
            ));
            std::process::exit(70);
        })?;
    Ok(())
}

/// Also watches the schema: a newer topside (a CLI after a deploy switched the
/// binary, before the service restarted) that migrates the database underneath
/// makes this one exit, to be restarted as the new version.
fn heartbeat_loop(ctx: &Ctx, schema: i64) {
    let db = open_store(ctx);
    loop {
        if db.schema().is_ok_and(|now| now > schema) {
            log("the database was migrated by a newer topside; exiting to be restarted");
            std::process::exit(70);
        }
        let _ = db.set_meta("serve_heartbeat", &now());
        let _ = db.set_meta("serve_pid", &std::process::id().to_string());
        std::thread::sleep(Duration::from_secs(2));
    }
}

fn wakes_loop(ctx: &Ctx) {
    let db = open_store(ctx);
    loop {
        if let Err(e) = process_wakes(ctx, &db) {
            log(format!("wake: {e:#}"));
        }
        std::thread::sleep(Duration::from_secs(2));
    }
}

fn outbox_loop(ctx: &Ctx, bot: Option<&Bot>) -> ! {
    let db = open_store(ctx);
    loop {
        if let Err(e) = process_outbox(ctx, &db, bot) {
            log(format!("outbox: {e:#}"));
        }
        std::thread::sleep(Duration::from_secs(2));
    }
}

fn backoff(attempts: i64) -> i64 {
    (5i64 << attempts.clamp(0, 10)).min(600)
}

fn give_up(db: &Store, item: &OutboxItem, text: &str) -> Result<()> {
    log(format!("{} #{}: giving up: {text}", item.kind, item.id));
    let count: i64 = db
        .meta("outbox_gave_up_count")?
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let _ = db.set_meta("outbox_gave_up_count", &(count + 1).to_string());
    let _ = db.set_meta(
        "outbox_gave_up",
        &format!("{} {} #{}: {}", now(), item.kind, item.id, clip(text, 300)),
    );
    db.outbox_done(item.id, Some(&format!("gave up: {text}")))
}

/// Whether an outbox failure has had enough tries.
fn exhausted(e: &anyhow::Error, attempts: i64) -> bool {
    let tg = e.downcast_ref::<TgError>();
    if tg.is_some_and(TgError::permanent) {
        return attempts >= PERMANENT_ATTEMPTS;
    }
    let transient = tg.is_some() || e.downcast_ref::<NetError>().is_some();
    !transient && attempts >= OUTBOX_ATTEMPTS
}

fn process_outbox(ctx: &Ctx, db: &Store, bot: Option<&Bot>) -> Result<()> {
    // A lane (project) whose head failed waits; the other lanes go on.
    let mut blocked: HashSet<String> = HashSet::new();
    for item in db.due_outbox()? {
        if blocked.contains(&item.lane) {
            continue;
        }
        let Some(bot) = bot else {
            db.outbox_done(item.id, Some("telegram is not configured"))?;
            continue;
        };
        // Claimed first: if the database fails after a successful send, the
        // event comes back after the backoff, not on the next tick.
        db.outbox_claim(item.id, &later(backoff(item.attempts)))?;
        let attempts = item.attempts + 1;
        match handle_event(ctx, db, bot, &item) {
            Ok(()) => db.outbox_done(item.id, None)?,
            Err(e) => {
                let text = format!("{e:#}");
                if exhausted(&e, attempts) {
                    give_up(db, &item, &text)?;
                    continue; // the rest of the lane goes on
                }
                let delay = e
                    .downcast_ref::<TgError>()
                    .and_then(|t| t.retry_after)
                    .map(|s| s as i64 + 1)
                    .unwrap_or_else(|| backoff(item.attempts));
                log(format!(
                    "{} #{} ({}): {text}; retry in {delay} s",
                    item.kind, item.id, item.lane
                ));
                db.outbox_retry(item.id, &text, &later(delay))?;
                // Later events of this project wait: order matters more than speed in a topic.
                blocked.insert(item.lane.clone());
            }
        }
    }
    Ok(())
}

fn process_wakes(ctx: &Ctx, db: &Store) -> Result<()> {
    for gate in db.due_wakes()? {
        let generation = gate.wake_gen;
        let Some(thread) = gate.thread_id.clone() else {
            db.wake_failed(gate.id, generation, "no thread", None)?;
            continue;
        };
        // Taken (as a lease) before sending: a database that cannot record the
        // result must not make the same answer arrive again and again; an
        // outcome that was never recorded makes it due again when the lease ends.
        if !db.claim_wake(gate.id, generation)? {
            continue;
        }
        let message = gates::compose_answer(ctx, db, &gate)?;
        match t3::send(ctx, &thread, &message) {
            Ok(()) => {
                log(format!(
                    "gate #{}: answer delivered to thread {thread}",
                    gate.id
                ));
                db.wake_done(gate.id, generation)?;
                db.enqueue("wake_result", json!({"gate": gate.id, "ok": true}))?;
            }
            Err(e) => {
                let text = format!("{e:#}");
                let attempts = gate.wake_attempts + 1;
                if attempts >= WAKE_ATTEMPTS {
                    log(format!("gate #{}: wake failed for good: {text}", gate.id));
                    db.wake_failed(gate.id, generation, &text, None)?;
                    db.enqueue(
                        "wake_result",
                        json!({"gate": gate.id, "ok": false, "error": text}),
                    )?;
                } else {
                    let delay = (15i64 << attempts).min(900);
                    log(format!(
                        "gate #{}: wake failed: {text}; retry in {delay} s",
                        gate.id
                    ));
                    db.wake_failed(gate.id, generation, &text, Some(&later(delay)))?;
                }
            }
        }
    }
    Ok(())
}

fn gate_id(item: &OutboxItem) -> Result<i64> {
    item.payload["gate"]
        .as_i64()
        .with_context(|| format!("{}: no gate in payload", item.kind))
}

fn handle_event(ctx: &Ctx, db: &Store, bot: &Bot, item: &OutboxItem) -> Result<()> {
    let tg = ctx.telegram().context("telegram is not configured")?;
    match item.kind.as_str() {
        "gate_opened" => post_gate(ctx, db, bot, tg, &db.gate(gate_id(item)?)?),
        "gate_question" => {
            let gate = db.gate(gate_id(item)?)?;
            let n = item.payload["n"].as_i64().unwrap_or(0);
            let Some(q) = db.questions(gate.id)?.into_iter().find(|q| q.n == n) else {
                return Ok(());
            };
            // Already in the gate message (added before it was posted).
            if gate.tg_shown_question.is_some_and(|shown| n <= shown) {
                return Ok(());
            }
            reply_gate(
                ctx,
                db,
                bot,
                tg,
                &gate,
                &format!("❓ <b>Question {n}</b>: {}", html(&clip(&q.text, 1500))),
            )
        }
        "gate_note" => {
            let gate = db.gate(gate_id(item)?)?;
            let note = db.note(item.payload["note"].as_i64().unwrap_or(0))?;
            if gate.tg_shown_note.is_some_and(|shown| note.id <= shown) {
                return Ok(());
            }
            reply_gate(
                ctx,
                db,
                bot,
                tg,
                &gate,
                &format!("📝 {}", html(&clip(&note.text, 2000))),
            )
        }
        "gate_files" => {
            let gate = db.gate(gate_id(item)?)?;
            if gate.tg_anchor.is_none() {
                return Ok(()); // never shown: in-order delivery means it will not be
            }
            send_agent_files(ctx, db, bot, tg, &gate)
        }
        "gate_withdrawn" => {
            let gate = db.gate(gate_id(item)?)?;
            if gate.tg_anchor.is_none() {
                return Ok(()); // never shown: nothing to take back
            }
            drop_keyboard(bot, tg, &gate);
            let reason = gate.close_reason.clone().unwrap_or_default();
            let text = if reason.is_empty() {
                "🚫 The agent withdrew this gate: no answer needed.".to_string()
            } else {
                format!(
                    "🚫 The agent withdrew this gate: {}",
                    html(&clip(&reason, 1000))
                )
            };
            reply_gate(ctx, db, bot, tg, &gate, &text)
        }
        "gate_closed" => {
            let gate = db.gate(gate_id(item)?)?;
            if gate.tg_anchor.is_none() {
                return Ok(());
            }
            drop_keyboard(bot, tg, &gate);
            let from_cli = gate.close_reason.as_deref() == Some("cli");
            let mut text = String::new();
            if from_cli {
                text.push_str("📥 The answer was recorded from the command line.");
            }
            if gate.thread_id.is_none() {
                if !text.is_empty() {
                    text.push(' ');
                }
                text.push_str(&format!(
                    "📥 Answer recorded. No agent thread to wake: the answer waits in topside, <code>topside gate show {}</code>",
                    gate.id
                ));
            }
            if text.is_empty() {
                return Ok(()); // the wake-up result reports delivery
            }
            reply_gate(ctx, db, bot, tg, &gate, &text)
        }
        "wake_result" => {
            let gate = db.gate(gate_id(item)?)?;
            if gate.tg_anchor.is_none() {
                return Ok(());
            }
            let text = if item.payload["ok"].as_bool() == Some(true) {
                let parts = db.replies(gate.id)?.len();
                format!(
                    "📨 Answer delivered to the agent in T3 ({parts} {}).",
                    parts_word(parts)
                )
            } else {
                format!(
                    "⚠️ Could not deliver the answer to the agent's thread: {}\nThe answer is saved: <code>topside gate show {}</code>; \
                     to send it to another thread: <code>topside gate set-thread {} &lt;thread&gt;</code>.",
                    html(&clip(item.payload["error"].as_str().unwrap_or("?"), 800)),
                    gate.id,
                    gate.id
                )
            };
            reply_gate(ctx, db, bot, tg, &gate, &text)
        }
        "publish" => post_publish(
            ctx,
            db,
            bot,
            tg,
            item.payload["publish"].as_i64().unwrap_or(0),
        ),
        "gate_build_ready" => {
            let gate = db.gate(gate_id(item)?)?;
            if gate.build_announced || !gate.is_open() || gate.tg_anchor.is_none() {
                return Ok(());
            }
            let covering = builds::covering(ctx, db, &gate)?;
            if covering.is_empty() {
                return Ok(());
            }
            let names: Vec<String> = covering
                .iter()
                .take(6)
                .map(|b| {
                    format!(
                        "{}: <code>{}</code>",
                        html(&b.platform),
                        html(&clip(b.name(), 120))
                    )
                })
                .collect();
            let text = format!("📦 A build to check is ready: {}", names.join(", "));
            let launch = launch_button(ctx, &covering);
            let with_button = launch.clone().map(|b| json!({"inline_keyboard": [[b]]}));
            match reply_gate_with(ctx, db, bot, tg, &gate, &text, with_button) {
                Err(e) if launch.is_some() && button_refused(&e) => {
                    log(format!(
                        "gate #{}: launch button refused ({e:#}); sent without it",
                        gate.id
                    ));
                    reply_gate(ctx, db, bot, tg, &gate, &text)?;
                }
                other => other?,
            }
            db.set_build_announced(gate.id)
        }
        other => {
            log(format!("outbox: unknown event {other}, dropped"));
            Ok(())
        }
    }
}

fn parts_word(n: usize) -> &'static str {
    if n == 1 { "part" } else { "parts" }
}

fn require_posted(gate: &Gate) -> Result<i64> {
    gate.tg_anchor
        .with_context(|| format!("gate #{} is not posted yet", gate.id))
}

fn drop_keyboard(bot: &Bot, tg: &TelegramConfig, gate: &Gate) {
    if let Some(message) = gate.tg_keyboard
        && let Err(e) = bot.remove_keyboard(tg.chat_id, message)
    {
        log(format!("gate #{}: cannot remove buttons: {e:#}", gate.id));
    }
}

fn reply_gate(
    ctx: &Ctx,
    db: &Store,
    bot: &Bot,
    tg: &TelegramConfig,
    gate: &Gate,
    text: &str,
) -> Result<()> {
    reply_gate_with(ctx, db, bot, tg, gate, text, None)
}

fn reply_gate_with(
    ctx: &Ctx,
    db: &Store,
    bot: &Bot,
    tg: &TelegramConfig,
    gate: &Gate,
    text: &str,
    keyboard: Option<Value>,
) -> Result<()> {
    let Some(anchor) = gate.tg_anchor else {
        log(format!(
            "gate #{} was never shown in Telegram; skipped: {}",
            gate.id,
            clip(text, 120)
        ));
        return Ok(());
    };
    let id = bot.send_message(
        tg.chat_id,
        ctx.topic(&gate.project),
        text,
        Some(anchor),
        keyboard,
    )?;
    db.map_message(tg.chat_id, id, gate.id)?;
    Ok(())
}

/// The launch button for a macOS build, when a launcher is configured.
fn launch_button(ctx: &Ctx, builds: &[Build]) -> Option<Value> {
    let b = builds.iter().find(|b| b.platform == "macos")?;
    let file = b
        .file
        .strip_prefix(&format!("{}/", b.project))
        .unwrap_or(&b.file);
    let url = launcher::link(ctx, &b.project, file)?;
    Some(json!({"text": ctx.buttons().launch, "url": url}))
}

/// Telegram refused the message because of a button (a launch link it does not accept).
fn button_refused(e: &anyhow::Error) -> bool {
    e.downcast_ref::<TgError>()
        .is_some_and(TgError::about_buttons)
}

fn keyboard(buttons: &Buttons, gate: i64, launch: Option<Value>) -> Value {
    let mut rows = vec![json!([
        {"text": buttons.send, "callback_data": format!("w:send:{gate}")},
        {"text": buttons.accept, "callback_data": format!("w:ok:{gate}")},
    ])];
    if let Some(button) = launch {
        rows.push(json!([button]));
    }
    json!({"inline_keyboard": rows})
}

/// The body is kept at least this long while there is anything else to shorten.
const BODY_FLOOR: usize = 400;

/// The gate as one message, and how many of `notes` (the oldest first) it shows.
/// The CLI keeps a gate within the budget; should the result still not fit
/// (notes added before posting, text that grows when escaped), the newest notes
/// are left out — they follow as separate messages — then the body down to a
/// floor, the questions, and the rest of the body are shortened, on the raw
/// text before escaping, so no tag is ever cut.
pub fn render_gate(
    gate: &Gate,
    title: &str,
    questions: &[Question],
    notes: &[String],
    covering: &[Build],
    buttons: &Buttons,
) -> (String, usize) {
    let full_body = gate.body.chars().count();
    let mut body_chars = full_body;
    let mut notes_kept = notes.len();
    let mut question_chars = 600;
    loop {
        let mut lines = vec![
            format!("🚦 <b>Gate #{} · {}</b>", gate.id, html(&clip(title, 100))),
            format!("<b>{}</b>", html(&clip(&gate.title, 300))),
        ];
        if !gate.body.is_empty() {
            lines.push(String::new());
            if body_chars > 0 {
                lines.extend(clip(&gate.body, body_chars).lines().map(html));
            }
            if body_chars < full_body {
                lines.push(format!(
                    "(the full description: <code>topside gate show {}</code>)",
                    gate.id
                ));
            }
        }
        lines.push(String::new());
        lines.push("<b>Questions</b>".into());
        for q in questions {
            lines.push(format!("{}. {}", q.n, html(&clip(&q.text, question_chars))));
        }
        for note in &notes[..notes_kept] {
            lines.push(format!("📝 {}", html(note)));
        }
        let later_notes = notes.len() - notes_kept;
        if later_notes > 0 {
            lines.push(format!(
                "📝 {later_notes} more note(s) follow as separate messages"
            ));
        }
        lines.push(String::new());
        if let Some(sha) = &gate.commit_sha {
            if covering.is_empty() {
                lines.push(
                    "⏳ No build with this work yet: I will post here when one is out.".into(),
                );
            } else {
                for b in covering.iter().take(6) {
                    lines.push(format!(
                        "📦 {}: <code>{}</code>",
                        html(&b.platform),
                        html(&clip(b.name(), 120))
                    ));
                }
            }
            lines.push(format!(
                "🌿 <code>{}</code>{}",
                html(&sha.chars().take(7).collect::<String>()),
                gate.branch
                    .as_deref()
                    .map(|b| format!(" {}", html(&clip(b, 80))))
                    .unwrap_or_default()
            ));
        }
        lines.push(format!(
            "💬 Reply to this message with text, photos or videos (up to 20 MB), as many as you need. \
             Then press “{}”, or “{}” if all is well (anything you wrote goes to the agent as remarks).",
            html(&buttons.send),
            html(&buttons.accept)
        ));
        if gate.thread_id.is_none() {
            lines.push("No agent thread: the answer is kept in topside.".into());
        }
        let text = lines.join("\n");
        let len = tg_len(&text);
        if len <= TEXT_LIMIT {
            return (text, notes_kept);
        }
        let over = len - TEXT_LIMIT;
        if notes_kept > 0 {
            notes_kept -= 1;
        } else if body_chars > BODY_FLOOR {
            body_chars = body_chars
                .saturating_sub(over.max(body_chars / 5).max(50))
                .max(BODY_FLOOR);
        } else if question_chars > 10 {
            // Text that grows when escaped (& → &amp;) can outgrow the CLI budget.
            question_chars = (question_chars * 4 / 5).max(10);
        } else if body_chars > 0 {
            body_chars = body_chars.saturating_sub(over.max(50));
        } else {
            return (
                format!(
                    "🚦 <b>Gate #{}</b>: the text does not fit one message, see <code>topside gate show {}</code>",
                    gate.id, gate.id
                ),
                0,
            );
        }
    }
}

fn post_gate(ctx: &Ctx, db: &Store, bot: &Bot, tg: &TelegramConfig, gate: &Gate) -> Result<()> {
    if gate.tg_anchor.is_none() {
        if !gate.is_open() {
            return Ok(()); // closed before it was shown
        }
        let covering = builds::covering(ctx, db, gate)?;
        let note_rows = db.notes(gate.id)?;
        let questions = db.questions(gate.id)?;
        let notes: Vec<String> = note_rows.iter().map(|n| clip(&n.text, 1500)).collect();
        let (text, notes_kept) = render_gate(
            gate,
            &ctx.title(&gate.project),
            &questions,
            &notes,
            &covering,
            &ctx.buttons(),
        );
        // What this message shows: later questions and notes, and notes left out
        // of it (the newest ones), are sent apart by their own events.
        let shown_question = questions.iter().map(|q| q.n).max().unwrap_or(0);
        let shown_note = notes_kept
            .checked_sub(1)
            .map(|i| note_rows[i].id)
            .unwrap_or(0);
        let launch = launch_button(ctx, &covering);
        let topic = ctx.topic(&gate.project);
        let sent = bot.send_message(
            tg.chat_id,
            topic,
            &text,
            None,
            Some(keyboard(&ctx.buttons(), gate.id, launch.clone())),
        );
        let id = match sent {
            // A gate must not be lost over a decoration: post it without the launch button.
            Err(e) if launch.is_some() && button_refused(&e) => {
                log(format!(
                    "gate #{}: launch button refused ({e:#}); posted without it",
                    gate.id
                ));
                bot.send_message(
                    tg.chat_id,
                    topic,
                    &text,
                    None,
                    Some(keyboard(&ctx.buttons(), gate.id, None)),
                )?
            }
            other => other?,
        };
        db.map_message(tg.chat_id, id, gate.id)?;
        db.set_gate_messages(gate.id, id, Some(id), shown_question, shown_note)?;
        if !covering.is_empty() {
            db.set_build_announced(gate.id)?;
        }
        log(format!("gate #{} posted", gate.id));
        builds::refresh(ctx, db, &gate.project);
    }
    let gate = db.gate(gate.id)?;
    send_agent_files(ctx, db, bot, tg, &gate)
}

/// Failed uploads in this run of the service (a restart gives them their tries
/// again), per file and per gate. Per file, not per event: tries the event spent
/// on something else (posting the gate) do not count against an upload. Per gate
/// too: once a gate's uploads have failed `UPLOAD_ATTEMPTS` times (the link is
/// that slow), each further file gets one try, so N attachments cost about
/// N + 2 tries, not 3 × N, while the project's topic waits.
#[derive(Default)]
struct UploadFailures {
    files: HashMap<i64, u32>,
    gates: HashMap<i64, u32>,
}

static UPLOAD_FAILURES: Mutex<Option<UploadFailures>> = Mutex::new(None);

fn with_failures<T>(f: impl FnOnce(&mut UploadFailures) -> T) -> T {
    let mut guard = UPLOAD_FAILURES.lock().unwrap_or_else(|e| e.into_inner());
    f(guard.get_or_insert_with(UploadFailures::default))
}

/// The file has had all its tries already: its note goes without another upload.
fn upload_exhausted(file: i64) -> bool {
    with_failures(|f| f.files.get(&file).is_some_and(|n| *n >= UPLOAD_ATTEMPTS))
}

/// Count a failed upload; whether the file has had all its tries now.
fn upload_failed(gate: i64, file: i64) -> bool {
    with_failures(|f| {
        let on_gate = f.gates.entry(gate).or_insert(0);
        *on_gate += 1;
        let gate_slow = *on_gate > UPLOAD_ATTEMPTS;
        let n = f.files.entry(file).or_insert(0);
        *n += 1;
        if gate_slow {
            *n = (*n).max(UPLOAD_ATTEMPTS);
        }
        *n >= UPLOAD_ATTEMPTS
    })
}

/// The file is in Telegram, or its note is.
fn upload_done(file: i64) {
    with_failures(|f| f.files.remove(&file));
}

/// Even the note did not get through: the link is down, not slow for uploads, so
/// the failed uploads say nothing about the file — it gets its tries back.
fn link_down(gate: i64, file: i64) {
    with_failures(|f| {
        f.files.remove(&file);
        f.gates.remove(&gate);
    });
}

/// An upload went through: the link works, the gate's next files get all their tries.
fn upload_succeeded(gate: i64) {
    with_failures(|f| f.gates.remove(&gate));
}

fn send_agent_files(
    ctx: &Ctx,
    db: &Store,
    bot: &Bot,
    tg: &TelegramConfig,
    gate: &Gate,
) -> Result<()> {
    let anchor = require_posted(gate)?;
    let topic = ctx.topic(&gate.project);
    for file in db.files(gate.id, Some("agent"))? {
        if file.tg_message_id.is_some() {
            continue;
        }
        let path = std::path::Path::new(&file.path);
        let name = file.path.rsplit('/').next().unwrap_or("").to_string();
        let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        let say_where = |why: &str| {
            bot.send_message(
                tg.chat_id,
                topic,
                &format!(
                    "📎 The file {} cannot be sent to Telegram ({}); it is on the agent's machine: <code>{}</code>",
                    html(&clip(&name, 120)),
                    html(why),
                    html(&clip(&file.path, 600))
                ),
                Some(anchor),
                None,
            )
        };
        let give_up_upload = || {
            say_where("the upload did not finish in time").inspect_err(|e| {
                if e.downcast_ref::<NetError>().is_some() {
                    link_down(gate.id, file.id);
                }
            })
        };
        // Telegram refuses empty and oversized files, every time: say where it is instead.
        let id = if size == 0 {
            say_where("it is empty")?
        } else if size > crate::telegram::MAX_UPLOAD {
            say_where(&human_size(size))?
        } else if upload_exhausted(file.id) {
            give_up_upload()?
        } else {
            match bot.send_file(tg.chat_id, topic, path, None, Some(anchor)) {
                Ok(id) => {
                    upload_succeeded(gate.id);
                    id
                }
                // Refused for good (e.g. a photo of odd proportions): say where it is
                // and go on, so the files after it are not held up.
                Err(e) if e.downcast_ref::<TgError>().is_some_and(TgError::permanent) => {
                    say_where(&clip(&format!("{e:#}"), 200))?
                }
                // An upload that keeps timing out (a slow link) must not hold the
                // project's topic forever either.
                Err(e)
                    if e.downcast_ref::<NetError>().is_some()
                        && upload_failed(gate.id, file.id) =>
                {
                    log(format!(
                        "gate #{}: {} did not upload: {e:#}",
                        gate.id, file.path
                    ));
                    give_up_upload()?
                }
                Err(e) => return Err(e),
            }
        };
        upload_done(file.id);
        db.map_message(tg.chat_id, id, gate.id)?;
        db.set_file_message(file.id, id)?;
    }
    Ok(())
}

/// One message per publish. Gates it makes ready get their own events, so a
/// failure there never re-posts the announcement.
fn post_publish(
    ctx: &Ctx,
    db: &Store,
    bot: &Bot,
    tg: &TelegramConfig,
    publish_id: i64,
) -> Result<()> {
    let publish = db.publish(publish_id)?;
    let project = &publish.project;
    let topic = ctx.topic(project);
    let built = db.builds_of_publish(publish_id)?;
    let root_name = ctx
        .builds_root()
        .ok()
        .and_then(|r| r.file_name().map(|n| n.to_string_lossy().to_string()))
        .unwrap_or_else(|| "builds".into());
    // Open gates this build lets the operator check.
    let mut ready = Vec::new();
    for gate in db.gates(Some(project), Some("open"))? {
        let covering = builds::covering(ctx, db, &gate)?;
        if covering.iter().any(|b| b.publish_id == Some(publish_id)) {
            ready.push(gate);
        }
    }
    let sha7: String = publish.sha.chars().take(7).collect();
    let header = format!(
        "📦 <b>{}</b> · <code>{}</code>",
        html(&clip(&ctx.title(project), 100)),
        html(&sha7)
    );
    let mut changes_kept = publish.changes.len().min(15);
    let mut ready_kept = ready.len().min(10);
    let text = loop {
        let mut lines = vec![format!(
            "{header}{} — {}",
            publish
                .tag
                .as_deref()
                .map(|t| format!(" · {}", html(&clip(t, 60))))
                .unwrap_or_default(),
            html(&clip(&publish.subject, 300))
        )];
        for b in built.iter().take(8) {
            lines.push(format!(
                "{}: <code>{}</code> ({})",
                html(&b.platform),
                html(&clip(b.name(), 120)),
                human_size(b.size as u64)
            ));
        }
        lines.push(format!(
            "Syncthing folder: <code>{}/{}/</code>",
            html(&root_name),
            html(project)
        ));
        if changes_kept > 0 {
            lines.push(String::new());
            lines.push("<b>What is new</b>".into());
            for change in publish.changes.iter().take(changes_kept) {
                lines.push(format!("• {}", html(&clip(change, 200))));
            }
        }
        if publish.changes.len() > changes_kept {
            lines.push(format!(
                "… and {} more",
                publish.changes.len() - changes_kept
            ));
        }
        if ready_kept > 0 {
            lines.push(String::new());
            let list: Vec<String> = ready
                .iter()
                .take(ready_kept)
                .map(
                    |g| match g.tg_anchor.and_then(|m| ctx.message_url(project, m)) {
                        Some(url) => format!(
                            "<a href=\"{}\">#{}</a> {}",
                            html(&url),
                            g.id,
                            html(&clip(&g.title, 120))
                        ),
                        None => format!("#{} {}", g.id, html(&clip(&g.title, 120))),
                    },
                )
                .collect();
            let more = ready.len() - ready_kept;
            lines.push(format!(
                "🚦 Ready to check: {}{}",
                list.join("; "),
                if more > 0 {
                    format!(" and {more} more")
                } else {
                    String::new()
                }
            ));
        }
        let text = lines.join("\n");
        if tg_len(&text) <= TEXT_LIMIT {
            break text;
        }
        if changes_kept > 0 {
            changes_kept -= 1;
        } else if ready_kept > 0 {
            ready_kept -= 1;
        } else {
            break header; // built from parts, never cut
        }
    };
    let launch = launch_button(ctx, &built);
    let with_button = launch.clone().map(|b| json!({"inline_keyboard": [[b]]}));
    match bot.send_message(tg.chat_id, topic, &text, None, with_button) {
        Err(e) if launch.is_some() && button_refused(&e) => {
            log(format!(
                "publish #{publish_id}: launch button refused ({e:#}); sent without it"
            ));
            bot.send_message(tg.chat_id, topic, &text, None, None)?;
        }
        other => {
            other?;
        }
    }
    for gate in ready {
        if !gate.build_announced && gate.tg_anchor.is_some() {
            db.enqueue("gate_build_ready", json!({"gate": gate.id}))?;
        }
    }
    log(format!("publish #{publish_id} of {project} announced"));
    Ok(())
}

// ---------------------------------------------------------------- updates

fn updates_loop(ctx: &Ctx, bot: &Bot, tg: &TelegramConfig) {
    // Here, not at start: a slow or blocked network must not delay the worker.
    let mut bot_name: Option<String> = None;
    match bot.get_me() {
        Ok(me) => {
            bot_name = me["username"].as_str().map(str::to_string);
            log(format!(
                "telegram: @{} in chat {}",
                bot_name.as_deref().unwrap_or("?"),
                tg.chat_id
            ));
        }
        Err(e) => log(format!("telegram: {e:#} (will retry)")),
    }
    if let Err(e) = bot.set_commands(&[("gates", "Open gates of this project")]) {
        log(format!("telegram: setMyCommands: {e:#}"));
    }
    let db = open_store(ctx);
    let mut offset: i64 = db
        .meta("tg_offset")
        .ok()
        .flatten()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let mut groups: HashMap<String, i64> = HashMap::new();
    loop {
        if bot_name.is_none() {
            bot_name = bot
                .get_me()
                .ok()
                .and_then(|me| me["username"].as_str().map(str::to_string));
        }
        match bot.get_updates(offset, 30) {
            Ok(updates) => {
                for update in updates {
                    let Some(update_id) = update["update_id"].as_i64() else {
                        continue;
                    };
                    handle_with_retries(
                        ctx,
                        &db,
                        bot,
                        tg,
                        bot_name.as_deref(),
                        &update,
                        &mut groups,
                    );
                    offset = update_id + 1;
                    let _ = db.set_meta("tg_offset", &offset.to_string());
                }
                if groups.len() > 500 {
                    groups.clear();
                }
            }
            Err(e) => {
                let wait = e
                    .downcast_ref::<TgError>()
                    .and_then(|t| t.retry_after)
                    .unwrap_or(5);
                log(format!("getUpdates: {e:#}; retry in {wait} s"));
                std::thread::sleep(Duration::from_secs(wait));
            }
        }
    }
}

/// An update that failed (a download through a flaky proxy, a busy database) is
/// tried again; if it still fails, the operator is told, so nothing they sent or
/// pressed disappears silently. Handling is idempotent per Telegram message.
fn handle_with_retries(
    ctx: &Ctx,
    db: &Store,
    bot: &Bot,
    tg: &TelegramConfig,
    bot_name: Option<&str>,
    update: &Value,
    groups: &mut HashMap<String, i64>,
) {
    let update_id = update["update_id"].as_i64().unwrap_or(0);
    let mut last = None;
    for attempt in 1..=UPDATE_ATTEMPTS {
        match handle_update(ctx, db, bot, tg, bot_name, update, groups) {
            Ok(()) => return,
            Err(e) => {
                log(format!("update {update_id} (attempt {attempt}): {e:#}"));
                last = Some(e);
                if attempt < UPDATE_ATTEMPTS {
                    std::thread::sleep(Duration::from_secs(3 * attempt as u64));
                }
            }
        }
    }
    let Some(e) = last else { return };
    let cq = &update["callback_query"];
    if let Some(id) = cq["id"].as_str() {
        // A button that failed: say so on the button itself.
        let text = format!(
            "That did not work: {}. Press again.",
            clip(&format!("{e:#}"), 150)
        );
        let _ = bot.answer_callback(id, &text, true);
        return;
    }
    let msg = if update["message"].is_object() {
        &update["message"]
    } else {
        &update["edited_message"]
    };
    if let Some(message_id) = msg["message_id"].as_i64()
        && msg["chat"]["id"].as_i64() == Some(tg.chat_id)
        && allowed(tg, msg["from"]["id"].as_i64())
    {
        let text = format!(
            "⚠️ Could not take this message: {}. Please send it again.",
            html(&clip(&format!("{e:#}"), 400))
        );
        match bot.send_message(
            tg.chat_id,
            msg["message_thread_id"].as_i64(),
            &text,
            Some(message_id),
            None,
        ) {
            // A reply to this notice belongs to the same gate as the failed message:
            // the gate it was mapped to (handling maps it before the download), or
            // the one it replied to.
            Ok(notice) => {
                let gate = db
                    .gate_of_message(tg.chat_id, message_id)
                    .ok()
                    .flatten()
                    .or_else(|| {
                        msg["reply_to_message"]["message_id"]
                            .as_i64()
                            .and_then(|r| db.gate_of_message(tg.chat_id, r).ok().flatten())
                    });
                if let Some(gate) = gate {
                    let _ = db.map_message(tg.chat_id, notice, gate);
                }
            }
            Err(e) => log(format!(
                "update {update_id}: cannot tell the operator: {e:#}"
            )),
        }
    }
}

fn handle_update(
    ctx: &Ctx,
    db: &Store,
    bot: &Bot,
    tg: &TelegramConfig,
    bot_name: Option<&str>,
    update: &Value,
    groups: &mut HashMap<String, i64>,
) -> Result<()> {
    if update["callback_query"].is_object() {
        return handle_callback(ctx, db, bot, tg, &update["callback_query"]);
    }
    if update["message"].is_object() {
        return handle_message(ctx, db, bot, tg, bot_name, &update["message"], groups);
    }
    if update["edited_message"].is_object() {
        return handle_edit(db, bot, tg, &update["edited_message"]);
    }
    if update["my_chat_member"].is_object() {
        // Setup aid: where the bot was added, so a wrong chat_id is visible in `topside status`.
        let n = chat_notes::membership(&now(), &update["my_chat_member"]);
        let note = chat_notes::text(ctx, chat_notes::Kind::Membership, &n);
        log(format!("telegram: bot membership: {note}"));
        db.set_meta(chat_notes::MEMBERSHIP_KEY, &n.to_string())?;
    }
    Ok(())
}

/// The operator edited a message that is part of an answer: the answer follows.
fn handle_edit(db: &Store, bot: &Bot, tg: &TelegramConfig, msg: &Value) -> Result<()> {
    let chat = msg["chat"]["id"].as_i64().unwrap_or(0);
    if chat != tg.chat_id || !allowed(tg, msg["from"]["id"].as_i64()) {
        return Ok(());
    }
    let message_id = msg["message_id"].as_i64().unwrap_or(0);
    let Some(gate_id) = db.gate_of_message(chat, message_id)? else {
        return Ok(());
    };
    let gate = db.gate(gate_id)?;
    let caption = msg["text"]
        .as_str()
        .or_else(|| msg["caption"].as_str())
        .unwrap_or("")
        .trim();
    // A file the bot could not download is part of the text: it stays.
    let text = match media_of(msg) {
        Some((_, size, name)) if size > MAX_DOWNLOAD => {
            format!("{caption}\n{}", oversize_note(&name, size))
                .trim()
                .to_string()
        }
        _ => caption.to_string(),
    };
    if !gate.is_open() {
        if db.reply_of_message(gate_id, message_id)?.is_some() {
            bot.send_message(
                chat,
                msg["message_thread_id"].as_i64(),
                &format!(
                    "Gate #{gate_id} is already closed: this edit did not reach the agent; write to it in T3."
                ),
                Some(message_id),
                None,
            )?;
        }
        return Ok(());
    }
    if db.update_reply_text(gate_id, message_id, &text)? {
        let _ = bot.react(chat, message_id, "✍");
    }
    Ok(())
}

fn oversize_note(name: &str, size: u64) -> String {
    format!(
        "[the bot could not download the file {name} ({}): larger than 20 MB]",
        human_size(size)
    )
}

fn allowed(tg: &TelegramConfig, user: Option<i64>) -> bool {
    user.is_some_and(|u| tg.allowed_users.contains(&u))
}

/// What a message starting with "/" is to us.
#[derive(Debug, PartialEq)]
enum Command {
    /// `/gates`, or `/gates@<this bot>`.
    Gates,
    /// Not for us, or one of ours we do not have: neither a command nor an answer.
    Ignore,
}

/// Only our own command counts as one. Anything else starting with "/" — a path,
/// `/fps 30 is fine`, an unknown `/word` — is ordinary text and may be an answer.
/// A command addressed to a bot (`/x@bot`) is never an answer.
fn command_of(text: &str, bot_name: Option<&str>) -> Option<Command> {
    let word = text.split_whitespace().next()?;
    let rest = word.strip_prefix('/')?;
    let (name, to) = rest.split_once('@').unwrap_or((rest, ""));
    let plain = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    if !plain(name) || (!to.is_empty() && !plain(to)) {
        return None;
    }
    // Before getMe has answered the bot's name is unknown: an addressed /gates is taken as ours.
    let ours = to.is_empty() || bot_name.is_none_or(|b| b.eq_ignore_ascii_case(to));
    match (ours, name, to.is_empty()) {
        (true, "gates", _) => Some(Command::Gates),
        (_, _, false) => Some(Command::Ignore),
        _ => None,
    }
}

/// `(file_id, size, file name)` of the media in a message, if any.
fn media_of(msg: &Value) -> Option<(String, u64, String)> {
    let id = msg["message_id"].as_i64().unwrap_or(0);
    let pick = |v: &Value, default: String| -> Option<(String, u64, String)> {
        Some((
            v["file_id"].as_str()?.to_string(),
            v["file_size"].as_u64().unwrap_or(0),
            v["file_name"]
                .as_str()
                .map(str::to_string)
                .unwrap_or(default),
        ))
    };
    if let Some(sizes) = msg["photo"].as_array() {
        return pick(sizes.last()?, format!("photo-{id}.jpg"));
    }
    for (field, default) in [
        ("video", format!("video-{id}.mp4")),
        ("document", format!("file-{id}")),
        ("animation", format!("animation-{id}.mp4")),
        ("video_note", format!("video-note-{id}.mp4")),
        ("voice", format!("voice-{id}.ogg")),
        ("audio", format!("audio-{id}.mp3")),
    ] {
        if msg[field].is_object() {
            return pick(&msg[field], default);
        }
    }
    None
}

fn handle_message(
    ctx: &Ctx,
    db: &Store,
    bot: &Bot,
    tg: &TelegramConfig,
    bot_name: Option<&str>,
    msg: &Value,
    groups: &mut HashMap<String, i64>,
) -> Result<()> {
    let chat = msg["chat"]["id"].as_i64().unwrap_or(0);
    let command = msg["text"].as_str().and_then(|t| command_of(t, bot_name));
    if command == Some(Command::Gates) && allowed(tg, msg["from"]["id"].as_i64()) {
        // Setup aid: the operator's command shows which chat and topic ids to configure.
        let n = chat_notes::seen(&now(), msg);
        let note = chat_notes::text(ctx, chat_notes::Kind::Seen, &n);
        log(format!("telegram: command from the operator in {note}"));
        db.set_meta(chat_notes::SEEN_KEY, &n.to_string())?;
    }
    if chat != tg.chat_id || !allowed(tg, msg["from"]["id"].as_i64()) {
        return Ok(());
    }
    let message_id = msg["message_id"].as_i64().unwrap_or(0);
    let topic = msg["message_thread_id"].as_i64();
    let text = msg["text"]
        .as_str()
        .or_else(|| msg["caption"].as_str())
        .unwrap_or("")
        .trim()
        .to_string();

    match command {
        Some(Command::Gates) => return command_gates(ctx, db, bot, tg, topic, message_id),
        Some(Command::Ignore) => return Ok(()),
        None => {}
    }

    let replied = msg["reply_to_message"]["message_id"].as_i64();
    let mut gate_id = match replied {
        Some(r) => db.gate_of_message(chat, r)?,
        None => None,
    };
    let album = msg["media_group_id"].as_str().map(str::to_string);
    if gate_id.is_none() {
        gate_id = album.as_ref().and_then(|g| groups.get(g).copied());
    }
    // Not a reply: in a forum topic such a message still "replies" to the topic's
    // creation message. The bot (an admin) sees it; if the project has exactly one
    // open gate *that the operator can see*, the message is an answer to it —
    // attaching a photo on a phone easily loses the reply.
    let plain = replied.is_none() || msg["reply_to_message"]["forum_topic_created"].is_object();
    if gate_id.is_none()
        && plain
        && let Some(project) = ctx.project_by_topic(topic)
    {
        let shown: Vec<Gate> = db
            .gates(Some(&project), Some("open"))?
            .into_iter()
            .filter(|g| g.tg_anchor.is_some())
            .collect();
        match shown.as_slice() {
            [only] => gate_id = Some(only.id),
            [] => {}
            several => {
                // One hint per message (or album), not per photo.
                if album.as_ref().is_none_or(|a| !groups.contains_key(a)) {
                    let ids: Vec<String> = several
                        .iter()
                        .take(20)
                        .map(|g| format!("#{}", g.id))
                        .collect();
                    bot.send_message(
                        chat,
                        topic,
                        &format!(
                            "Several gates are open ({}): reply to the message of the one this is for.",
                            ids.join(", ")
                        ),
                        Some(message_id),
                        None,
                    )?;
                    // Marked only once the hint is out: a failed send is retried in full.
                    if let Some(a) = &album {
                        groups.insert(a.clone(), 0);
                    }
                }
                return Ok(());
            }
        }
    }
    let Some(gate_id) = gate_id.filter(|id| *id > 0) else {
        log(format!(
            "message {message_id} (topic {topic:?}, reply to {replied:?}) is not an answer to a gate; ignored"
        ));
        return Ok(());
    };
    if let Some(album) = album {
        groups.insert(album, gate_id);
    }
    let gate = db.gate(gate_id)?;
    if !gate.is_open() {
        let state = if gate.status == "withdrawn" {
            "was withdrawn"
        } else {
            "is already closed"
        };
        bot.send_message(
            chat,
            topic,
            &format!(
                "Gate #{} {state}: this message was not taken as an answer. Write to the agent in T3 or wait for a new gate.",
                gate.id
            ),
            Some(message_id),
            None,
        )?;
        return Ok(());
    }
    db.map_message(chat, message_id, gate.id)?;
    // Handled before (this is a retry after a later step failed): nothing to add.
    if db.reply_of_message(gate.id, message_id)?.is_some() {
        let _ = bot.react(chat, message_id, "👍");
        return Ok(());
    }

    match media_of(msg) {
        Some((_, size, name)) if size > MAX_DOWNLOAD => {
            // The warning first: recorded only once it is out, so a retry repeats it.
            let warning = bot.send_message(
                chat,
                topic,
                "⚠️ The file is larger than 20 MB: the bot cannot download it. Trim the recording, or put the file into Syncthing and reply to this message with its path.",
                Some(message_id),
                None,
            )?;
            db.map_message(chat, warning, gate.id)?;
            let note = oversize_note(&name, size);
            let added = db.add_reply(
                gate.id,
                "telegram",
                format!("{text}\n{note}").trim(),
                None,
                Some(message_id),
            )?;
            if added.is_none() {
                return closed_meanwhile(bot, chat, topic, message_id, gate.id);
            }
            return Ok(());
        }
        Some((file_id, _, name)) => {
            let dir = ctx.paths.gate_dir(gate.id).join("operator");
            std::fs::create_dir_all(&dir)?;
            // File systems count bytes (255 on ext4): the name is cut on a character boundary.
            let dest = dir.join(format!(
                "{message_id}-{}",
                clip_bytes(&safe_name(&name), 180)
            ));
            bot.download(&file_id, &dest)?;
            let file = db.add_file(
                gate.id,
                "operator",
                &dest.to_string_lossy(),
                crate::util::media_kind(&dest),
            )?;
            if db
                .add_reply(gate.id, "telegram", &text, Some(file), Some(message_id))?
                .is_none()
            {
                return closed_meanwhile(bot, chat, topic, message_id, gate.id);
            }
        }
        None if text.is_empty() => return Ok(()),
        None => {
            if db
                .add_reply(gate.id, "telegram", &text, None, Some(message_id))?
                .is_none()
            {
                return closed_meanwhile(bot, chat, topic, message_id, gate.id);
            }
        }
    }
    if let Err(e) = bot.react(chat, message_id, "👍") {
        log(format!("gate #{}: reaction: {e:#}", gate.id));
    }
    Ok(())
}

/// The gate closed while the message was being handled (a long download).
fn closed_meanwhile(
    bot: &Bot,
    chat: i64,
    topic: Option<i64>,
    message_id: i64,
    gate: i64,
) -> Result<()> {
    bot.send_message(
        chat,
        topic,
        &format!(
            "Gate #{gate} closed while I was taking this message: it was not taken as an answer. Write to the agent in T3."
        ),
        Some(message_id),
        None,
    )?;
    Ok(())
}

fn command_gates(
    ctx: &Ctx,
    db: &Store,
    bot: &Bot,
    tg: &TelegramConfig,
    topic: Option<i64>,
    message_id: i64,
) -> Result<()> {
    let project = ctx.project_by_topic(topic);
    let open = db.gates(project.as_deref(), Some("open"))?;
    let mut lines = Vec::new();
    if open.is_empty() {
        lines.push("No open gates.".to_string());
    }
    for g in open.iter().take(30) {
        let link = g
            .tg_anchor
            .and_then(|m| ctx.message_url(&g.project, m))
            .map(|u| format!("<a href=\"{}\">#{}</a>", html(&u), g.id))
            .unwrap_or_else(|| format!("#{}", g.id));
        let replies = db.replies(g.id)?.len();
        let progress = if replies > 0 {
            format!(" · answer so far: {replies} {}", parts_word(replies))
        } else {
            String::new()
        };
        lines.push(format!(
            "{link} {} · {}{progress}",
            html(&clip(&g.title, 100)),
            html(&g.project)
        ));
    }
    let mut more = open.len().saturating_sub(30);
    let text = loop {
        let mut text = lines.join("\n");
        if more > 0 {
            text.push_str(&format!("\n… and {more} more"));
        }
        if tg_len(&text) <= TEXT_LIMIT || lines.len() <= 1 {
            break text;
        }
        lines.pop();
        more += 1;
    };
    bot.send_message(tg.chat_id, topic, &text, Some(message_id), None)?;
    Ok(())
}

fn handle_callback(
    ctx: &Ctx,
    db: &Store,
    bot: &Bot,
    tg: &TelegramConfig,
    cq: &Value,
) -> Result<()> {
    let id = cq["id"].as_str().unwrap_or("");
    if cq["message"]["chat"]["id"].as_i64() != Some(tg.chat_id) {
        return Ok(());
    }
    if !allowed(tg, cq["from"]["id"].as_i64()) {
        bot.answer_callback(id, "Only the operator can answer gates.", false)?;
        return Ok(());
    }
    let data = cq["data"].as_str().unwrap_or("");
    let mut parts = data.split(':');
    let (Some("w"), Some(action), Some(gate_id)) = (parts.next(), parts.next(), parts.next())
    else {
        return Ok(());
    };
    let gate_id: i64 = gate_id.parse().unwrap_or(0);
    let gate = db.gate(gate_id)?;
    if !gate.is_open() {
        // The same press handled again after a failure, or a double tap: the answer is in.
        let just_now = gate.close_reason.as_deref() == Some("telegram")
            && gate
                .closed_at
                .as_deref()
                .and_then(crate::util::age_secs)
                .is_some_and(|age| age < 300);
        let text = if just_now {
            format!("The answer to gate #{gate_id} is already recorded.")
        } else {
            format!("Gate #{gate_id} is already closed.")
        };
        bot.answer_callback(id, &text, false)?;
        if let Some(message) = cq["message"]["message_id"].as_i64() {
            let _ = bot.remove_keyboard(tg.chat_id, message);
        }
        return Ok(());
    }
    match action {
        "send" => {
            if db.replies(gate_id)?.is_empty() {
                let hint = format!(
                    "First reply to the gate's message with text, photos or videos. Or press “{}”.",
                    ctx.buttons().accept
                );
                bot.answer_callback(id, &hint, true)?;
                return Ok(());
            }
            gates::finalize(ctx, db, &gate, "answered", "telegram")?;
            let _ = bot.answer_callback(id, "Answer recorded: passing it to the agent.", false);
        }
        "ok" => {
            gates::finalize(ctx, db, &gate, "accepted", "telegram")?;
            let _ = bot.answer_callback(id, "Accepted: passing it to the agent.", false);
        }
        _ => return Ok(()),
    }
    log(format!(
        "gate #{gate_id}: answered from telegram ({action})"
    ));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plurals() {
        assert_eq!(parts_word(1), "part");
        assert_eq!(parts_word(2), "parts");
        assert_eq!(parts_word(21), "parts");
    }

    #[test]
    fn media_picks_largest_photo() {
        let msg = json!({"message_id": 7, "photo": [
            {"file_id": "small", "file_size": 10},
            {"file_id": "big", "file_size": 100}
        ]});
        assert_eq!(
            media_of(&msg),
            Some(("big".into(), 100, "photo-7.jpg".into()))
        );
        let doc = json!({"message_id": 8, "document": {"file_id": "d", "file_size": 5, "file_name": "log.txt"}});
        assert_eq!(media_of(&doc), Some(("d".into(), 5, "log.txt".into())));
        assert_eq!(media_of(&json!({"message_id": 9, "text": "hi"})), None);
    }

    #[test]
    fn backoff_is_bounded() {
        assert_eq!(backoff(0), 5);
        assert_eq!(backoff(3), 40);
        assert_eq!(backoff(50), 600);
    }

    #[test]
    fn only_our_commands_are_commands() {
        let me = Some("example_gates_bot");
        assert_eq!(command_of("/gates", me), Some(Command::Gates));
        assert_eq!(
            command_of("/gates@example_gates_bot", me),
            Some(Command::Gates)
        );
        assert_eq!(
            command_of("/gates@Example_Gates_Bot", me),
            Some(Command::Gates)
        );
        assert_eq!(command_of("/gates@other_bot", me), Some(Command::Ignore));
        assert_eq!(
            command_of("/help@example_gates_bot", me),
            Some(Command::Ignore)
        );
        // Before getMe: an addressed /gates is ours.
        assert_eq!(
            command_of("/gates@example_gates_bot", None),
            Some(Command::Gates)
        );
        // Text that happens to start with "/" is an answer.
        assert_eq!(
            command_of("/home/me/builds/clip.mp4 is the recording", me),
            None
        );
        assert_eq!(command_of("/tmp has the log", me), None);
        assert_eq!(command_of("/fps 30 is fine", me), None);
        assert_eq!(command_of("/", me), None);
        assert_eq!(command_of("plain text", me), None);
    }

    #[test]
    fn network_and_setup_errors_are_never_given_up() {
        let net: anyhow::Error = NetError("io: timed out".into()).into();
        assert!(!exhausted(&net, 1000));
        let setup: anyhow::Error = TgError {
            code: Some(400),
            description: "sendMessage: Bad Request: chat not found".into(),
            retry_after: None,
        }
        .into();
        assert!(!exhausted(&setup, 1000));
        let bad: anyhow::Error = TgError {
            code: Some(400),
            description: "sendMessage: Bad Request: can't parse entities".into(),
            retry_after: None,
        }
        .into();
        assert!(!exhausted(&bad, 2));
        assert!(exhausted(&bad, 3));
        let local = anyhow::anyhow!("cannot read /x");
        assert!(!exhausted(&local, 29));
        assert!(exhausted(&local, 30));
    }

    fn sample_gate(body: &str, title: &str) -> Gate {
        let dir = tempfile::tempdir().unwrap();
        let mut db = Store::open_at(&dir.path().join("w.db")).unwrap();
        let id = db
            .insert_gate(&crate::store::NewGate {
                project: "p",
                title,
                body,
                questions: &[],
                thread_id: None,
                commit_sha: Some("0123456789"),
                branch: Some("feature/x"),
                worktree: None,
                created_by: None,
            })
            .unwrap();
        db.gate(id).unwrap()
    }

    fn q(n: i64, text: &str) -> Question {
        Question {
            n,
            text: text.into(),
            added_at: String::new(),
        }
    }

    #[test]
    fn a_gate_is_always_one_valid_message() {
        // Hostile sizes: a huge body full of characters that grow when escaped,
        // many long notes and questions.
        let body = "<&>\"".repeat(5000);
        let gate = sample_gate(&body, &"<b>".repeat(200));
        let questions: Vec<Question> = (1..=12).map(|n| q(n, &"&".repeat(400))).collect();
        let notes: Vec<String> = (0..20).map(|_| "<".repeat(1500)).collect();
        let (text, kept) = render_gate(
            &gate,
            "Project",
            &questions,
            &notes,
            &[],
            &Buttons::default(),
        );
        assert!(tg_len(&text) <= TEXT_LIMIT, "{}", tg_len(&text));
        assert_eq!(kept, 0, "the notes follow as separate messages");
        // Every tag opened is closed: nothing was cut inside markup.
        assert_eq!(text.matches("<b>").count(), text.matches("</b>").count());
        assert_eq!(
            text.matches("<code>").count(),
            text.matches("</code>").count()
        );
        assert!(
            text.contains("the full description"),
            "the cut is announced"
        );
    }

    #[test]
    fn notes_that_do_not_fit_are_the_newest() {
        let gate = sample_gate(&"Шаг. ".repeat(300), "Walls");
        let notes: Vec<String> = (0..3).map(|i| format!("{i}{}", "з".repeat(1400))).collect();
        let (text, kept) = render_gate(
            &gate,
            "Project",
            &[q(1, "Comfortable?")],
            &notes,
            &[],
            &Buttons::default(),
        );
        assert!(kept < 3 && kept > 0, "{kept}");
        assert!(
            text.contains(&format!("0{}", "з".repeat(100))),
            "the oldest note stays"
        );
        assert!(text.contains("follow as separate messages"));
    }

    #[test]
    fn emoji_count_twice() {
        // 🟢 is two UTF-16 units: a gate of emoji must still fit Telegram's count.
        let gate = sample_gate(&"🟢".repeat(2400), "Emoji");
        let (text, _) = render_gate(
            &gate,
            "Project",
            &[q(1, "?")],
            &[],
            &[],
            &Buttons::default(),
        );
        assert!(tg_len(&text) <= TEXT_LIMIT, "{}", tg_len(&text));
    }

    #[test]
    fn a_gate_at_the_cli_budget_keeps_body_and_questions() {
        // The largest gate `gate open` accepts must arrive whole.
        let body = "Шаг. ".repeat(gates::BUDGET / 5 / 2);
        let per_q = (gates::BUDGET - body.chars().count()) / 12 - 2;
        let questions: Vec<Question> = (1..=12).map(|n| q(n, &"в".repeat(per_q))).collect();
        let gate = sample_gate(&body, "Title");
        let (text, _) = render_gate(&gate, "Project", &questions, &[], &[], &Buttons::default());
        assert!(tg_len(&text) <= TEXT_LIMIT);
        assert!(!text.contains("the full description"), "nothing was cut");
        assert!(text.contains(&"в".repeat(per_q)));
    }

    #[test]
    fn a_normal_gate_is_untouched() {
        let gate = sample_gate("Step 1\nStep 2", "Walls");
        let questions = vec![q(1, "Comfortable?")];
        let (text, kept) = render_gate(
            &gate,
            "Project",
            &questions,
            &["a note".into()],
            &[],
            &Buttons::default(),
        );
        assert!(text.contains("Step 1\nStep 2"));
        assert!(text.contains("📝 a note"));
        assert_eq!(kept, 1);
        assert!(!text.contains("the full description"));
    }

    #[test]
    fn the_operator_s_own_button_labels_are_on_the_buttons_and_in_the_hint() {
        let buttons = Buttons {
            send: "📨 Отправить агенту".into(),
            accept: "✅ Принято".into(),
            launch: "▶️ Запустить на Mac".into(),
        };
        let gate = sample_gate("Step 1", "Walls");
        let (text, _) = render_gate(
            &gate,
            "Project",
            &[q(1, "Comfortable?")],
            &[],
            &[],
            &buttons,
        );
        assert!(text.contains("“📨 Отправить агенту”"), "{text}");
        assert!(text.contains("“✅ Принято”"), "{text}");
        let kb = keyboard(&buttons, 7, None);
        assert_eq!(kb["inline_keyboard"][0][0]["text"], "📨 Отправить агенту");
        assert_eq!(kb["inline_keyboard"][0][0]["callback_data"], "w:send:7");
        assert_eq!(kb["inline_keyboard"][0][1]["text"], "✅ Принято");
        let default = keyboard(&Buttons::default(), 7, None);
        assert_eq!(default["inline_keyboard"][0][1]["text"], "✅ Accepted");
    }
}
