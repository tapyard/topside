//! Operator gates: a question an agent cannot answer itself.
//!
//! An agent opens a gate (what to check, numbered questions, which commit);
//! the service posts it to the project's Telegram topic; the operator replies
//! there and presses the send button; the service writes the answer into the
//! T3 thread that opened the gate. Everything is also reachable from the CLI.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde_json::json;

use crate::builds;
use crate::config::Ctx;
use crate::git;
use crate::store::{Gate, NewGate, Store};
use crate::util::{local, media_kind, parse_duration, safe_name};

pub const EXIT_WITHDRAWN: u8 = 3;
pub const EXIT_TIMEOUT: u8 = 75;

/// A gate is one Telegram message (4096 characters): these limits keep it so,
/// with room for the header, the build list and the instructions.
pub const MAX_TITLE: usize = 200;
pub const MAX_BODY: usize = 2500;
pub const MAX_QUESTION: usize = 400;
pub const MAX_QUESTIONS: usize = 12;
pub const MAX_NOTE: usize = 1500;
pub const MAX_REASON: usize = 500;

/// Title, body and questions together, counted as Telegram counts them (after
/// escaping): within this a gate always arrives whole in one message, next to
/// the header, the build list and the instructions.
pub const BUDGET: usize = 2600;

/// As the service measures a gate message: escaped, in UTF-16 units (an emoji is
/// two) — so a gate within the budget is never shortened.
fn escaped_len(text: &str) -> usize {
    crate::util::tg_len(&crate::util::html(text))
}

fn check_budget(title: &str, body: &str, questions: &[String]) -> Result<()> {
    let used = escaped_len(title)
        + escaped_len(body)
        + questions.iter().map(|q| escaped_len(q) + 4).sum::<usize>();
    if used > BUDGET {
        bail!(
            "the gate is {used} characters (title, body and questions together, as Telegram \
             counts them), at most {BUDGET}: it must fit one message on the operator's phone; \
             shorten it or move details into a file with --attach"
        );
    }
    Ok(())
}

fn check_len(what: &str, text: &str, max: usize) -> Result<()> {
    let n = text.chars().count();
    if n > max {
        bail!(
            "{what} is {n} characters, at most {max}: the operator reads it on a phone; \
             put details into a file and pass it with --attach"
        );
    }
    Ok(())
}

pub struct OpenArgs {
    pub project: Option<String>,
    pub title: String,
    pub body: Option<String>,
    pub body_file: Option<PathBuf>,
    pub questions: Vec<String>,
    pub attach: Vec<PathBuf>,
    pub thread: Option<String>,
    pub no_thread: bool,
    pub commit: Option<String>,
    pub no_commit: bool,
    pub json: bool,
}

/// Copy a file into the gate's folder: the service sends and serves only files it owns.
fn copy_into_gate(ctx: &Ctx, gate: i64, role: &str, src: &Path) -> Result<PathBuf> {
    if !src.is_file() {
        bail!("{} is not a file", src.display());
    }
    let dir = ctx.paths.gate_dir(gate).join(role);
    std::fs::create_dir_all(&dir)?;
    let name = safe_name(
        &src.file_name()
            .map(|n| n.to_string_lossy())
            .unwrap_or_default(),
    );
    let mut dest = dir.join(&name);
    let mut i = 1;
    while dest.exists() {
        dest = dir.join(format!("{i}-{name}"));
        i += 1;
    }
    std::fs::copy(src, &dest).with_context(|| format!("cannot copy {}", src.display()))?;
    Ok(dest)
}

fn store_file(ctx: &Ctx, db: &Store, gate: i64, role: &str, src: &Path) -> Result<i64> {
    let dest = copy_into_gate(ctx, gate, role, src)?;
    db.add_file(gate, role, &dest.to_string_lossy(), media_kind(&dest))
}

pub fn open(ctx: &Ctx, a: OpenArgs) -> Result<u8> {
    let cwd = std::env::current_dir()?;
    let project = ctx.resolve_project(a.project.as_deref(), &cwd)?;
    let title = a.title.trim();
    if title.is_empty() {
        bail!("the title is empty");
    }
    check_len("the title", title, MAX_TITLE)?;
    let body = match (&a.body, &a.body_file) {
        (Some(_), Some(_)) => bail!("pass --body or --body-file, not both"),
        (Some(text), None) => text.clone(),
        (None, Some(path)) if path.as_os_str() == "-" => {
            std::io::read_to_string(std::io::stdin()).context("cannot read the body from stdin")?
        }
        (None, Some(path)) => std::fs::read_to_string(path)
            .with_context(|| format!("cannot read {}", path.display()))?,
        (None, None) => String::new(),
    };
    let questions: Vec<String> = a
        .questions
        .iter()
        .map(|q| q.trim().to_string())
        .filter(|q| !q.is_empty())
        .collect();
    check_len("the body", body.trim(), MAX_BODY)?;
    if questions.len() > MAX_QUESTIONS {
        bail!(
            "{} questions, at most {MAX_QUESTIONS}: split the check into several gates",
            questions.len()
        );
    }
    for q in &questions {
        check_len("a question", q, MAX_QUESTION)?;
    }
    check_budget(title, body.trim(), &questions)?;
    // Attachments are checked before the gate exists: a gate that fails halfway
    // would stay open, unposted and with an id the agent never saw.
    let attachments: Vec<PathBuf> = a.attach.iter().map(|p| cwd.join(p)).collect();
    for path in &attachments {
        if !path.is_file() {
            bail!("{} is not a file", path.display());
        }
    }
    if questions.is_empty() {
        bail!(
            "a gate needs at least one --question: the operator answers questions, not a wall of text"
        );
    }

    let (commit, branch, worktree) = if a.no_commit {
        (None, None, None)
    } else {
        // The work under review is the checkout the agent runs in.
        let checkout = ctx.checkout(&project, &cwd)?;
        let rev = a.commit.as_deref().unwrap_or("HEAD");
        let sha = git::rev(&checkout, rev)?;
        if a.commit.is_none() && git::dirty(&checkout).unwrap_or(false) {
            eprintln!(
                "warning: {} has uncommitted changes; the operator checks a published build, \
                 and a build is published only from a commit",
                checkout.display()
            );
        }
        // An explicit --commit is not the checkout's HEAD: its branch and folder say nothing.
        let here = a.commit.is_none() || git::rev(&checkout, "HEAD").ok().as_deref() == Some(&sha);
        (
            Some(sha),
            if here { git::branch(&checkout) } else { None },
            here.then(|| checkout.to_string_lossy().to_string()),
        )
    };
    let thread = if a.no_thread {
        None
    } else {
        a.thread
            .clone()
            .or_else(|| std::env::var("T3CODE_THREAD_ID").ok())
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty())
    };

    let mut db = Store::open(&ctx.paths)?;
    let id = db.insert_gate(&NewGate {
        project: &project,
        title,
        body: body.trim(),
        questions: &questions,
        thread_id: thread.as_deref(),
        commit_sha: commit.as_deref(),
        branch: branch.as_deref(),
        worktree: worktree.as_deref(),
        created_by: std::env::var("T3CODE_THREAD_ID").ok().as_deref(),
    })?;
    for path in &attachments {
        if let Err(e) = store_file(ctx, &db, id, "agent", path) {
            // Never shown, so withdrawing it is silent; the agent just retries.
            let _ = db.withdraw_gate(id, &format!("attachment failed: {e:#}"));
            return Err(e);
        }
    }
    db.enqueue("gate_opened", json!({"gate": id}))?;

    if a.json {
        println!(
            "{}",
            json!({"gate": id, "project": project, "thread": thread, "commit": commit})
        );
    } else {
        println!("{id}");
    }
    if thread.is_none() && !a.no_thread {
        eprintln!(
            "warning: no T3 thread (T3CODE_THREAD_ID is not set): the answer wakes nobody; \
             pass --thread or poll with `topside gate wait {id}`"
        );
    }
    if ctx.telegram().is_none() {
        eprintln!("warning: Telegram is not configured: the operator will not see this gate");
    }
    Ok(0)
}

pub fn list(ctx: &Ctx, project: Option<&str>, status: &str, as_json: bool) -> Result<u8> {
    if let Some(p) = project {
        ctx.project(p)?;
    }
    let db = Store::open(&ctx.paths)?;
    let status = if status == "all" { None } else { Some(status) };
    let gates = db.gates(project, status)?;
    if as_json {
        println!("{}", serde_json::to_string_pretty(&gates)?);
        return Ok(0);
    }
    if gates.is_empty() {
        println!("no gates");
    }
    for g in &gates {
        println!(
            "#{:<4} {:<15} {:<10} {}  {}",
            g.id,
            g.project,
            g.status,
            local(&g.created_at),
            g.title
        );
    }
    Ok(0)
}

pub fn show(ctx: &Ctx, id: i64, as_json: bool) -> Result<u8> {
    let db = Store::open(&ctx.paths)?;
    let gate = db.gate(id)?;
    let questions = db.questions(id)?;
    let replies = db.replies(id)?;
    let notes = db.notes(id)?;
    let agent_files = db.files(id, Some("agent"))?;
    let builds = builds::covering(ctx, &db, &gate).unwrap_or_default();
    let root = ctx.builds_root().ok();
    if as_json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "gate": gate,
                "questions": questions,
                "notes": notes,
                "agent_files": agent_files,
                "replies": replies,
                "builds": builds.iter().map(|b| json!({
                    "platform": b.platform,
                    "path": root.as_ref().map(|r| r.join(&b.file)),
                    "sha": b.sha,
                })).collect::<Vec<_>>(),
                "message_url": gate.tg_anchor.and_then(|m| ctx.message_url(&gate.project, m)),
            }))?
        );
        return Ok(0);
    }
    println!("#{} {} — {}", gate.id, gate.title, gate.project);
    println!(
        "status: {}{}",
        gate.status,
        gate.verdict
            .as_deref()
            .map(|v| format!(", verdict: {v}"))
            .unwrap_or_default()
    );
    if let Some(sha) = &gate.commit_sha {
        println!(
            "commit: {} {}",
            &sha[..7.min(sha.len())],
            gate.branch.as_deref().unwrap_or("")
        );
    }
    if let Some(thread) = &gate.thread_id {
        println!(
            "thread: {thread}{}",
            gate.wake_state
                .as_deref()
                .map(|s| format!(" (wake: {s})"))
                .unwrap_or_default()
        );
    }
    if let Some(err) = &gate.wake_error {
        println!("wake error: {err}");
    }
    if let Some(url) = gate
        .tg_anchor
        .and_then(|m| ctx.message_url(&gate.project, m))
    {
        println!("telegram: {url}");
    }
    for b in &builds {
        println!("build: {}", b.file);
    }
    if !gate.body.is_empty() {
        println!("\n{}", gate.body);
    }
    println!("\nQuestions:");
    for q in &questions {
        println!("{}. {}", q.n, q.text);
    }
    for n in &notes {
        println!("note ({}): {}", local(&n.at), n.text);
    }
    for f in &agent_files {
        println!("agent file: {}", f.path);
    }
    if !replies.is_empty() {
        println!("\nOperator's answer:");
        for r in &replies {
            if !r.text.is_empty() {
                println!("{}", r.text);
            }
            if let Some(file) = &r.file {
                println!("[file] {file}");
            }
        }
    }
    Ok(0)
}

fn open_gate(db: &Store, id: i64) -> Result<Gate> {
    let gate = db.gate(id)?;
    if !gate.is_open() {
        bail!("gate #{id} is {}", gate.status);
    }
    Ok(gate)
}

pub fn ask(ctx: &Ctx, id: i64, question: &str) -> Result<u8> {
    let db = Store::open(&ctx.paths)?;
    let gate = open_gate(&db, id)?;
    let question = question.trim();
    if question.is_empty() {
        bail!("the question is empty");
    }
    check_len("the question", question, MAX_QUESTION)?;
    // Not posted yet: the question will be part of the gate message, so it counts.
    if gate.tg_anchor.is_none() {
        let mut all: Vec<String> = db.questions(id)?.into_iter().map(|q| q.text).collect();
        all.push(question.to_string());
        if all.len() > MAX_QUESTIONS {
            bail!(
                "gate #{id} would have {} questions, at most {MAX_QUESTIONS}",
                all.len()
            );
        }
        check_budget(&gate.title, &gate.body, &all)?;
    }
    let n = db.add_question(id, question)?;
    db.enqueue("gate_question", json!({"gate": id, "n": n}))?;
    println!("question {n} added to gate #{id}");
    Ok(0)
}

pub fn note(ctx: &Ctx, id: i64, text: &str) -> Result<u8> {
    let db = Store::open(&ctx.paths)?;
    open_gate(&db, id)?;
    if text.trim().is_empty() {
        bail!("the note is empty");
    }
    check_len("the note", text.trim(), MAX_NOTE)?;
    let note = db.add_note(id, text.trim())?;
    db.enqueue("gate_note", json!({"gate": id, "note": note}))?;
    Ok(0)
}

pub fn attach(ctx: &Ctx, id: i64, files: &[PathBuf]) -> Result<u8> {
    let db = Store::open(&ctx.paths)?;
    open_gate(&db, id)?;
    let cwd = std::env::current_dir()?;
    let mut ids = Vec::new();
    for path in files {
        ids.push(store_file(ctx, &db, id, "agent", &cwd.join(path))?);
    }
    db.enqueue("gate_files", json!({"gate": id, "files": ids}))?;
    Ok(0)
}

pub fn withdraw(ctx: &Ctx, id: i64, reason: &str) -> Result<u8> {
    let db = Store::open(&ctx.paths)?;
    check_len("the reason", reason.trim(), MAX_REASON)?;
    if !db.withdraw_gate(id, reason.trim())? {
        bail!("gate #{id} is not open");
    }
    db.enqueue("gate_withdrawn", json!({"gate": id}))?;
    builds::refresh(ctx, &db, &db.gate(id)?.project);
    println!("gate #{id} withdrawn");
    Ok(0)
}

pub fn set_thread(ctx: &Ctx, id: i64, thread: Option<&str>) -> Result<u8> {
    let db = Store::open(&ctx.paths)?;
    db.gate(id)?;
    // An answered gate whose thread was missing or wrong: deliver to the new one.
    if db.set_thread_and_rewake(id, thread)? {
        println!(
            "gate #{id}: answer queued for thread {}",
            thread.unwrap_or("")
        );
    }
    Ok(0)
}

/// The operator answers from the CLI (e.g. over ssh) instead of Telegram. All or
/// nothing: files are checked first, and replies and the close happen in one
/// transaction — a gate closed meanwhile (in Telegram) gets none of them.
pub fn answer(ctx: &Ctx, id: i64, texts: &[String], files: &[PathBuf], accept: bool) -> Result<u8> {
    let db = Store::open(&ctx.paths)?;
    open_gate(&db, id)?;
    if texts.is_empty() && files.is_empty() && !accept {
        bail!("nothing to answer: pass --text, --file or --accept");
    }
    let cwd = std::env::current_dir()?;
    let mut copied = Vec::new();
    for path in files {
        match copy_into_gate(ctx, id, "operator", &cwd.join(path)) {
            Ok(dest) => copied.push(dest),
            Err(e) => {
                copied.iter().for_each(|c| {
                    let _ = std::fs::remove_file(c);
                });
                return Err(e);
            }
        }
    }
    let remove_copies = || {
        copied.iter().for_each(|c| {
            let _ = std::fs::remove_file(c);
        });
    };
    if let Err(e) = db.conn.execute_batch("BEGIN IMMEDIATE") {
        remove_copies();
        return Err(e.into());
    }
    let done = (|| -> Result<bool> {
        let gate = open_gate(&db, id)?;
        for text in texts {
            db.add_reply(id, "cli", text.trim(), None, None)?;
        }
        for dest in &copied {
            let file = db.add_file(id, "operator", &dest.to_string_lossy(), media_kind(dest))?;
            db.add_reply(id, "cli", "", Some(file), None)?;
        }
        let verdict = if accept { "accepted" } else { "answered" };
        let closed = close(&db, &gate, verdict, "cli")?;
        if closed {
            db.conn.execute_batch("COMMIT")?;
        }
        Ok(closed)
    })();
    match done {
        Ok(true) => {}
        other => {
            let _ = db.conn.execute_batch("ROLLBACK");
            remove_copies();
            other?;
            bail!("gate #{id} was closed meanwhile (in Telegram?): this answer was not recorded");
        }
    }
    after_close(ctx, &db, id);
    println!("gate #{id} answered");
    Ok(0)
}

/// Close the gate with the collected answer and queue the wake-up; then write
/// the answer file and refresh the builds overview.
pub fn finalize(ctx: &Ctx, db: &Store, gate: &Gate, verdict: &str, source: &str) -> Result<bool> {
    let tx =
        rusqlite::Transaction::new_unchecked(&db.conn, rusqlite::TransactionBehavior::Immediate)?;
    if !close(db, gate, verdict, source)? {
        return Ok(false);
    }
    tx.commit()?;
    after_close(ctx, db, gate.id);
    Ok(true)
}

/// The close itself, database only — run inside the caller's transaction, so
/// nothing outside the database happens for a close that is rolled back.
fn close(db: &Store, gate: &Gate, verdict: &str, source: &str) -> Result<bool> {
    // Accepted with something written is acceptance with remarks: stored as such,
    // so `gate show`, --json and the wake-up message all say the same.
    let has_notes = db
        .replies(gate.id)?
        .iter()
        .any(|r| !r.text.trim().is_empty() || r.file.is_some());
    let verdict = if verdict == "accepted" && has_notes {
        "accepted_with_notes"
    } else {
        verdict
    };
    if !db.answer_gate(gate.id, verdict, source, gate.thread_id.is_some())? {
        return Ok(false);
    }
    db.enqueue("gate_closed", json!({"gate": gate.id}))?;
    Ok(true)
}

/// After a committed close: conveniences, so a failure here is only reported.
fn after_close(ctx: &Ctx, db: &Store, id: i64) {
    let written = (|| -> Result<()> {
        let gate = db.gate(id)?;
        let text = compose_answer(ctx, db, &gate)?;
        let dir = ctx.paths.gate_dir(id);
        std::fs::create_dir_all(&dir)?;
        std::fs::write(dir.join("answer.md"), &text)?;
        builds::refresh(ctx, db, &gate.project);
        Ok(())
    })();
    if let Err(e) = written {
        crate::util::warn(format_args!(
            "gate #{id}: answer recorded, but answer.md was not written: {e:#}"
        ));
    }
}

/// The message the agent receives: answer, files, and the questions it answers.
pub fn compose_answer(_ctx: &Ctx, db: &Store, gate: &Gate) -> Result<String> {
    let replies = db.replies(gate.id)?;
    let questions = db.questions(gate.id)?;
    let mut out = format!(
        "[topside] The operator answered gate #{} \"{}\" ({}).\n",
        gate.id, gate.title, gate.project
    );
    let texts: Vec<&str> = replies
        .iter()
        .map(|r| r.text.trim())
        .filter(|t| !t.is_empty())
        .collect();
    let files: Vec<&str> = replies.iter().filter_map(|r| r.file.as_deref()).collect();
    let remarks = gate.verdict.as_deref() == Some("accepted_with_notes");
    out.push_str(match gate.verdict.as_deref() {
        Some("accepted") => "Verdict: accepted. The check passed; move on.\n",
        Some("accepted_with_notes") => {
            "Verdict: accepted with remarks. Take the remarks below into account; no rework is needed.\n"
        }
        _ => "Verdict: answered. Read the answer below and act on it.\n",
    });
    if !texts.is_empty() {
        out.push_str(if remarks {
            "\nThe operator's remarks:\n"
        } else {
            "\nThe operator's answer:\n"
        });
        for t in texts {
            out.push_str(t);
            out.push('\n');
        }
    }
    if !files.is_empty() {
        out.push_str("\nThe operator's files:\n");
        for f in files {
            out.push_str(&format!("- {f}\n"));
        }
    }
    out.push_str("\nThe gate's questions:\n");
    for q in &questions {
        out.push_str(&format!("{}. {}\n", q.n, q.text));
    }
    out.push_str(&format!(
        "\nEverything: `topside gate show {}`. Fixes go into new commits; \
         a new question for the operator is a new gate.\n",
        gate.id
    ));
    Ok(out)
}

/// Block until the operator answers. For agents that cannot be woken.
pub fn wait(ctx: &Ctx, id: i64, timeout: Option<&str>) -> Result<u8> {
    let limit = timeout.map(parse_duration).transpose()?;
    let started = Instant::now();
    loop {
        let db = Store::open(&ctx.paths)?;
        let gate = db.gate(id)?;
        match gate.status.as_str() {
            "answered" => {
                print!("{}", compose_answer(ctx, &db, &gate)?);
                return Ok(0);
            }
            "withdrawn" => {
                eprintln!("gate #{id} was withdrawn");
                return Ok(EXIT_WITHDRAWN);
            }
            _ => {}
        }
        drop(db);
        if limit.is_some_and(|l| started.elapsed() >= l) {
            eprintln!("gate #{id}: no answer yet");
            return Ok(EXIT_TIMEOUT);
        }
        std::thread::sleep(Duration::from_secs(3));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, Paths};
    use crate::store::NewGate;

    fn setup() -> (tempfile::TempDir, Ctx, Store, i64) {
        let dir = tempfile::tempdir().unwrap();
        let ctx = Ctx {
            cfg: Config::default(),
            paths: Paths {
                config_file: dir.path().join("config.toml"),
                data_dir: dir.path().join("data"),
                cache_dir: dir.path().join("cache"),
            },
        };
        let mut db = Store::open(&ctx.paths).unwrap();
        let id = db
            .insert_gate(&NewGate {
                project: "my-game",
                title: "Walls",
                body: "",
                questions: &["Comfortable?".into()],
                thread_id: Some("t"),
                commit_sha: None,
                branch: None,
                worktree: None,
                created_by: None,
            })
            .unwrap();
        (dir, ctx, db, id)
    }

    #[test]
    fn accepted_without_replies_has_no_remarks() {
        let (_d, ctx, db, id) = setup();
        assert!(finalize(&ctx, &db, &db.gate(id).unwrap(), "accepted", "telegram").unwrap());
        let gate = db.gate(id).unwrap();
        assert_eq!(gate.verdict.as_deref(), Some("accepted"));
        let text = compose_answer(&ctx, &db, &gate).unwrap();
        assert!(text.contains("Verdict: accepted. "), "{text}");
    }

    #[test]
    fn accepted_with_replies_carries_remarks() {
        let (_d, ctx, db, id) = setup();
        db.add_reply(id, "telegram", "the gate is a bit crooked", None, None)
            .unwrap();
        assert!(finalize(&ctx, &db, &db.gate(id).unwrap(), "accepted", "telegram").unwrap());
        let gate = db.gate(id).unwrap();
        assert_eq!(gate.verdict.as_deref(), Some("accepted_with_notes"));
        let text = compose_answer(&ctx, &db, &gate).unwrap();
        assert!(text.contains("Verdict: accepted with remarks"), "{text}");
        assert!(text.contains("The operator's remarks"), "{text}");
        assert!(!text.contains("Verdict: accepted. "), "{text}");
        assert!(text.contains("the gate is a bit crooked"), "{text}");
    }

    #[test]
    fn a_second_close_reports_false() {
        let (_d, ctx, db, id) = setup();
        let gate = db.gate(id).unwrap();
        assert!(finalize(&ctx, &db, &gate, "accepted", "telegram").unwrap());
        assert!(!finalize(&ctx, &db, &gate, "answered", "cli").unwrap());
    }

    #[test]
    fn the_budget_counts_escaping() {
        assert!(check_budget("t", &"a".repeat(2000), &[]).is_ok());
        // The same length of '&' is five times longer once escaped.
        assert!(check_budget("t", &"&".repeat(2000), &[]).is_err());
        let qs: Vec<String> = (0..12).map(|_| "в".repeat(400)).collect();
        assert!(
            check_budget("t", "", &qs).is_err(),
            "12 long questions do not fit"
        );
    }

    #[test]
    fn length_limits() {
        assert!(check_len("t", &"я".repeat(MAX_TITLE), MAX_TITLE).is_ok());
        assert!(check_len("t", &"я".repeat(MAX_TITLE + 1), MAX_TITLE).is_err());
    }

    #[test]
    fn answered_reads_as_answer() {
        let (_d, ctx, db, id) = setup();
        db.add_reply(id, "telegram", "1. no", None, None).unwrap();
        db.answer_gate(id, "answered", "telegram", true).unwrap();
        let text = compose_answer(&ctx, &db, &db.gate(id).unwrap()).unwrap();
        assert!(text.contains("Verdict: answered"), "{text}");
    }
}
