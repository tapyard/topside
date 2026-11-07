//! topside — builds and operator gates for agent-driven game development.

mod builds;
mod chat_notes;
mod config;
mod daemon;
mod gates;
mod git;
mod launcher;
mod store;
mod t3;
mod telegram;
mod util;

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Result, bail};
use clap::{Parser, Subcommand};
use serde_json::json;

use crate::config::Ctx;
use crate::store::Store;
use crate::util::{age_secs, local};

const GUIDE: &str = include_str!("guide.md");

#[derive(Parser)]
#[command(
    name = "topside",
    version,
    about = "Builds and operator gates for agent-driven game development",
    long_about = "Builds and operator gates for agent-driven game development.\n\n\
        Agents publish builds into a Syncthing folder and ask the operator questions they \
        cannot answer themselves (gates). The operator answers in Telegram; the answer wakes \
        the agent's T3 Code thread. Agents: read `topside guide` first."
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Publish builds, list them, open the latest one
    #[command(subcommand)]
    Build(BuildCmd),
    /// Operator gates: questions only the operator can answer
    #[command(subcommand)]
    Gate(GateCmd),
    /// Run the service: Telegram bot, notifications, T3 wake-ups
    Serve,
    /// Configuration and service health
    Status {
        #[arg(long)]
        json: bool,
    },
    /// The guide for agents: when and how to publish builds and open gates
    Guide,
    /// Link to the project's Telegram topic (for an in-game "questions" button)
    Url {
        #[arg(long)]
        project: Option<String>,
    },
    /// Telegram setup checks
    #[command(subcommand)]
    Telegram(TelegramCmd),
    /// Start builds on this Mac from Telegram buttons
    #[command(subcommand)]
    Launcher(LauncherCmd),
}

#[derive(Subcommand)]
enum LauncherCmd {
    /// Listen on [launcher].url (127.0.0.1) for signed "start this build" links
    Serve,
    /// macOS: install and load a LaunchAgent running `topside launcher serve`
    Install,
    /// Print the signed link for a build (to test the launcher)
    Link {
        project: String,
        /// Relative to the project folder, e.g. macos/<name>.zip
        file: String,
    },
}

#[derive(Subcommand)]
enum BuildCmd {
    /// Copy finished builds into the Syncthing folder and announce them
    Publish {
        /// <platform>=<path>, e.g. android=build/game.apk macos=build/macos/game.zip linux=build/linux
        /// (a folder is packed into .tar.gz)
        #[arg(required = true)]
        artifacts: Vec<String>,
        /// Project (default: the one whose repository holds the current directory)
        #[arg(long)]
        project: Option<String>,
        /// Label for builds not from the main line, e.g. proto-build
        #[arg(long)]
        tag: Option<String>,
        /// Publish even with uncommitted changes (the build cannot be reproduced)
        #[arg(long)]
        allow_dirty: bool,
        #[arg(long)]
        json: bool,
    },
    /// Builds in the folder, newest first
    List {
        #[arg(long)]
        project: Option<String>,
        #[arg(long)]
        platform: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Path of the latest build
    Latest {
        project: String,
        #[arg(long)]
        platform: Option<String>,
        #[arg(long)]
        tag: Option<String>,
    },
    /// Unpack the latest build of a project and start it (macOS: the .app)
    Open {
        project: String,
        #[arg(long)]
        platform: Option<String>,
        #[arg(long)]
        tag: Option<String>,
        /// Only unpack and print what would start
        #[arg(long)]
        print: bool,
    },
    /// Register builds already in the folder and rewrite its manifests
    Reindex { project: String },
}

#[derive(Subcommand)]
enum GateCmd {
    /// Ask the operator; prints the gate id
    Open {
        /// Short title: what is being checked
        #[arg(long)]
        title: String,
        /// What to do before answering: where, which steps (plain text)
        #[arg(long)]
        body: Option<String>,
        /// Read the body from a file (- for stdin)
        #[arg(long)]
        body_file: Option<PathBuf>,
        /// A question to answer, one per flag; numbered in order
        #[arg(long = "question", short = 'q', required = true)]
        questions: Vec<String>,
        /// Screenshot, video or file for the operator, one per flag
        #[arg(long)]
        attach: Vec<PathBuf>,
        #[arg(long)]
        project: Option<String>,
        /// T3 thread woken by the answer (default: $T3CODE_THREAD_ID)
        #[arg(long)]
        thread: Option<String>,
        /// Wake nobody: the answer is read with `topside gate show/wait`
        #[arg(long, conflicts_with = "thread")]
        no_thread: bool,
        /// Commit under review (default: HEAD of the current checkout)
        #[arg(long)]
        commit: Option<String>,
        /// The gate is not about a build (no commit, no build link)
        #[arg(long, conflicts_with = "commit")]
        no_commit: bool,
        #[arg(long)]
        json: bool,
    },
    /// Gates, oldest first
    List {
        #[arg(long)]
        project: Option<String>,
        /// open | answered | withdrawn | all
        #[arg(long, default_value = "open")]
        status: String,
        #[arg(long)]
        json: bool,
    },
    /// Everything about a gate, including the operator's answer
    Show {
        id: i64,
        #[arg(long)]
        json: bool,
    },
    /// Add a question to an open gate
    Ask { id: i64, question: String },
    /// Add a note to an open gate (no answer expected)
    Note { id: i64, text: String },
    /// Add screenshots or files to an open gate
    Attach {
        id: i64,
        #[arg(required = true)]
        files: Vec<PathBuf>,
    },
    /// Take back a gate that is no longer needed
    Withdraw {
        id: i64,
        #[arg(long, default_value = "")]
        reason: String,
    },
    /// Block until the operator answers; exit 0 answered, 3 withdrawn, 75 timeout
    Wait {
        id: i64,
        /// e.g. 90s, 30m, 2h (default: no limit)
        #[arg(long)]
        timeout: Option<String>,
    },
    /// Answer as the operator from the command line
    Answer {
        id: i64,
        /// Answer text, one per flag
        #[arg(long = "text", short = 't')]
        texts: Vec<String>,
        /// File to attach, one per flag
        #[arg(long = "file")]
        files: Vec<PathBuf>,
        /// Accepted; with --text/--file the agent gets them as remarks (verdict "accepted with remarks")
        #[arg(long)]
        accept: bool,
    },
    /// Change the T3 thread an answer goes to (re-delivers an undelivered answer)
    SetThread { id: i64, thread: String },
}

#[derive(Subcommand)]
enum TelegramCmd {
    /// Send a test message to the project's topic
    Test {
        #[arg(long)]
        project: Option<String>,
    },
}

fn status(ctx: &Ctx, as_json: bool) -> Result<u8> {
    let db = Store::open(&ctx.paths)?;
    let heartbeat = db.meta("serve_heartbeat")?;
    let alive = heartbeat
        .as_deref()
        .and_then(age_secs)
        .is_some_and(|age| age < 30);
    let (pending, last_error) = db.outbox_pending()?;
    // Stored as Telegram reported them; the verdict follows the config as it is now.
    let membership = db.meta(chat_notes::MEMBERSHIP_KEY)?;
    let seen = db.meta(chat_notes::SEEN_KEY)?;
    let note = |kind, raw: &Option<String>, local_time| {
        raw.as_deref()
            .map(|r| chat_notes::shown(ctx, kind, r, local_time))
    };
    let gave_up = db.meta("outbox_gave_up")?;
    let gave_up_count: i64 = db
        .meta("outbox_gave_up_count")?
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    // Waiting events per lane (project): a stuck project shows here with the error at its head.
    let lanes: Vec<serde_json::Value> = db
        .outbox_lanes()?
        .into_iter()
        .map(|(lane, waiting, error)| json!({"project": lane, "waiting": waiting, "head_error": error}))
        .collect();
    // The service's pid: a deploy checks that the heartbeat is the new process's.
    let pid: Option<u32> = db.meta("serve_pid")?.and_then(|p| p.parse().ok());
    let mut projects = Vec::new();
    for (name, p) in &ctx.cfg.projects {
        let open = db.gates(Some(name), Some("open"))?.len();
        projects.push(json!({"name": name, "repo": p.repo, "topic": p.topic, "open_gates": open}));
    }
    let undelivered = db
        .gates(None, Some("answered"))?
        .into_iter()
        .filter(|g| {
            matches!(
                g.wake_state.as_deref(),
                Some("pending") | Some("sending") | Some("failed")
            )
        })
        .map(|g| g.id)
        .collect::<Vec<_>>();
    if as_json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "version": env!("CARGO_PKG_VERSION"),
                "config": ctx.paths.config_file,
                "database": ctx.paths.db(),
                "builds_root": ctx.cfg.builds_root,
                "telegram": ctx.telegram().is_some(),
                "service": {"alive": alive, "heartbeat": heartbeat, "pid": pid},
                "outbox_gave_up": gave_up,
                "outbox_gave_up_count": gave_up_count,
                "outbox_pending": pending,
                "outbox_lanes": lanes,
                "telegram_membership": note(chat_notes::Kind::Membership, &membership, false),
                "telegram_last_command": note(chat_notes::Kind::Seen, &seen, false),
                "outbox_last_error": last_error,
                "undelivered_answers": undelivered,
                "projects": projects,
            }))?
        );
        return Ok(if alive { 0 } else { 1 });
    }
    println!("topside {}", env!("CARGO_PKG_VERSION"));
    println!("config:   {}", ctx.paths.config_file.display());
    println!("database: {}", ctx.paths.db().display());
    println!(
        "builds:   {}",
        ctx.cfg
            .builds_root
            .as_deref()
            .unwrap_or("(builds_root not set)")
    );
    println!(
        "telegram: {}",
        if ctx.telegram().is_some() {
            "configured"
        } else {
            "not configured"
        }
    );
    match (&heartbeat, alive) {
        (Some(h), true) => println!("service:  running (heartbeat {})", local(h)),
        (Some(h), false) => println!("service:  NOT running (last heartbeat {})", local(h)),
        (None, _) => println!("service:  never started (`topside serve`)"),
    }
    if let Some(m) = note(chat_notes::Kind::Membership, &membership, true) {
        println!("bot:      last membership change {m}");
    }
    if let Some(m) = note(chat_notes::Kind::Seen, &seen, true) {
        println!("bot:      last operator command {m}");
    }
    if let Some(g) = &gave_up {
        println!("dropped:  {gave_up_count} event(s) given up, the last {g}");
    }
    if pending > 0 {
        println!(
            "outbox:   {pending} waiting{}",
            last_error
                .map(|e| format!(" — last error: {e}"))
                .unwrap_or_default()
        );
        for lane in &lanes {
            let name = lane["project"].as_str().unwrap_or("");
            println!(
                "          {}: {} waiting{}",
                if name.is_empty() {
                    "(no project)"
                } else {
                    name
                },
                lane["waiting"],
                lane["head_error"]
                    .as_str()
                    .map(|e| format!(" — first one failed: {}", util::clip(e, 200)))
                    .unwrap_or_default()
            );
        }
    }
    if !undelivered.is_empty() {
        println!("answers not delivered to T3: {undelivered:?} (`topside gate show <id>`)");
    }
    for p in &projects {
        println!(
            "project:  {} — {} open gate(s), repo {}",
            p["name"].as_str().unwrap_or(""),
            p["open_gates"],
            p["repo"].as_str().unwrap_or("")
        );
    }
    Ok(if alive { 0 } else { 1 })
}

fn telegram_test(ctx: &Ctx, project: Option<&str>) -> Result<u8> {
    let Some(tg) = ctx.telegram() else {
        bail!(
            "telegram is not configured in {}",
            ctx.paths.config_file.display()
        );
    };
    let bot = telegram::Bot::from_config(tg)?;
    let me = bot.get_me()?;
    let topic = match project {
        Some(p) => {
            ctx.project(p)?;
            ctx.topic(p)
        }
        None => None,
    };
    let text = format!(
        "✅ topside is connected{}",
        project
            .map(|p| format!(" — {}", util::html(&ctx.title(p))))
            .unwrap_or_default()
    );
    let id = bot.send_message(tg.chat_id, topic, &text, None, None)?;
    println!(
        "sent as @{} to chat {} topic {:?}, message {id}",
        me["username"].as_str().unwrap_or("?"),
        tg.chat_id,
        topic
    );
    Ok(0)
}

fn run(cli: Cli) -> Result<u8> {
    let ctx = Ctx::load()?;
    match cli.cmd {
        Cmd::Serve => daemon::serve(ctx),
        Cmd::Status { json } => status(&ctx, json),
        Cmd::Guide => {
            print!("{GUIDE}");
            Ok(0)
        }
        Cmd::Url { project } => {
            let project = ctx.resolve_project(project.as_deref(), &std::env::current_dir()?)?;
            match ctx.topic_url(&project) {
                Some(url) => {
                    println!("{url}");
                    Ok(0)
                }
                None => bail!("telegram is not configured: no link for {project}"),
            }
        }
        Cmd::Telegram(TelegramCmd::Test { project }) => telegram_test(&ctx, project.as_deref()),
        Cmd::Launcher(LauncherCmd::Serve) => launcher::serve(&ctx),
        Cmd::Launcher(LauncherCmd::Install) => launcher::install(&ctx),
        Cmd::Launcher(LauncherCmd::Link { project, file }) => {
            match launcher::try_link(&ctx, &project, &file)? {
                Some(url) => {
                    println!("{url}");
                    Ok(0)
                }
                None => bail!(
                    "[launcher] is not set in {}",
                    ctx.paths.config_file.display()
                ),
            }
        }
        Cmd::Build(cmd) => match cmd {
            BuildCmd::Publish {
                artifacts,
                project,
                tag,
                allow_dirty,
                json,
            } => builds::publish(
                &ctx,
                builds::PublishArgs {
                    project,
                    tag,
                    allow_dirty,
                    artifacts,
                    json,
                },
            ),
            BuildCmd::List {
                project,
                platform,
                json,
            } => builds::list(&ctx, project.as_deref(), platform.as_deref(), json),
            BuildCmd::Latest {
                project,
                platform,
                tag,
            } => builds::latest(&ctx, &project, platform.as_deref(), tag.as_deref()),
            BuildCmd::Open {
                project,
                platform,
                tag,
                print,
            } => builds::open(&ctx, &project, platform.as_deref(), tag.as_deref(), print),
            BuildCmd::Reindex { project } => builds::reindex(&ctx, &project),
        },
        Cmd::Gate(cmd) => match cmd {
            GateCmd::Open {
                title,
                body,
                body_file,
                questions,
                attach,
                project,
                thread,
                no_thread,
                commit,
                no_commit,
                json,
            } => gates::open(
                &ctx,
                gates::OpenArgs {
                    project,
                    title,
                    body,
                    body_file,
                    questions,
                    attach,
                    thread,
                    no_thread,
                    commit,
                    no_commit,
                    json,
                },
            ),
            GateCmd::List {
                project,
                status,
                json,
            } => {
                if !["open", "answered", "withdrawn", "all"].contains(&status.as_str()) {
                    bail!("--status is open, answered, withdrawn or all");
                }
                gates::list(&ctx, project.as_deref(), &status, json)
            }
            GateCmd::Show { id, json } => gates::show(&ctx, id, json),
            GateCmd::Ask { id, question } => gates::ask(&ctx, id, &question),
            GateCmd::Note { id, text } => gates::note(&ctx, id, &text),
            GateCmd::Attach { id, files } => gates::attach(&ctx, id, &files),
            GateCmd::Withdraw { id, reason } => gates::withdraw(&ctx, id, &reason),
            GateCmd::Wait { id, timeout } => gates::wait(&ctx, id, timeout.as_deref()),
            GateCmd::Answer {
                id,
                texts,
                files,
                accept,
            } => gates::answer(&ctx, id, &texts, &files, accept),
            GateCmd::SetThread { id, thread } => gates::set_thread(&ctx, id, Some(&thread)),
        },
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    // `topside … | head` must end quietly, not panic on a closed pipe. Not for the
    // services: they write into other programs' pipes (t3), and the default
    // SIGPIPE would kill the whole service when such a program exits early.
    let service = matches!(cli.cmd, Cmd::Serve | Cmd::Launcher(LauncherCmd::Serve));
    #[cfg(unix)]
    if !service {
        // SAFETY: setting a signal disposition at start, before any thread exists.
        unsafe {
            libc::signal(libc::SIGPIPE, libc::SIG_DFL);
        }
    }
    #[cfg(not(unix))]
    let _ = service;
    match run(cli) {
        Ok(code) => ExitCode::from(code),
        Err(e) => {
            eprintln!("topside: {e:#}");
            ExitCode::FAILURE
        }
    }
}
