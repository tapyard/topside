//! Waking an agent: a message into its T3 Code thread.
//!
//! `t3 thread send` queues the message as the thread's next turn and never
//! interrupts a running one, so an answer that arrives mid-turn is not lost.

use std::io::{Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use crate::config::Ctx;
use crate::util::{clip, expand};

const DEADLINE: Duration = Duration::from_secs(90);

/// Read a pipe on its own thread: a grandchild holding it open must not block us.
fn collect<R: Read + Send + 'static>(pipe: Option<R>) -> mpsc::Receiver<String> {
    let (tx, rx) = mpsc::channel();
    if let Some(mut pipe) = pipe {
        std::thread::spawn(move || {
            let mut out = Vec::new();
            let _ = pipe.read_to_end(&mut out);
            let _ = tx.send(String::from_utf8_lossy(&out).to_string());
        });
    }
    rx
}

/// Kill the whole process group: a wrapper script that does not `exec` must not
/// leave the real sender running (it could deliver after the failure is recorded,
/// and the retry would deliver again).
fn kill(child: &mut Child) {
    #[cfg(unix)]
    // SAFETY: plain syscall on a pid we own; a negative pid addresses its group.
    unsafe {
        libc::kill(-(child.id() as i32), libc::SIGKILL);
    }
    let _ = child.kill();
    let _ = child.wait(); // reap: no zombies
}

pub fn send(ctx: &Ctx, thread: &str, message: &str) -> Result<()> {
    run(&command(ctx, thread), message, DEADLINE)
}

fn command(ctx: &Ctx, thread: &str) -> Command {
    let cli = ctx
        .cfg
        .t3_cli
        .as_deref()
        .map(expand)
        .unwrap_or_else(|| "t3".into());
    let mut cmd = Command::new(cli);
    cmd.args(["thread", "send"]);
    if let Some(base) = &ctx.cfg.t3_base_dir {
        cmd.arg("--base-dir").arg(expand(base));
    }
    cmd.arg(thread).arg("-");
    cmd
}

fn run(cmd: &Command, message: &str, deadline: Duration) -> Result<()> {
    let program = cmd.get_program().to_string_lossy().to_string();
    let mut command = Command::new(cmd.get_program());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0); // its own group, so a timeout kills its children too
    }
    let mut child = command
        .args(cmd.get_args())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("cannot start {program}"))?;
    let started = Instant::now();
    let stdout = collect(child.stdout.take());
    let stderr = collect(child.stderr.take());
    // Writing a long message to a program that does not read it would block: own thread too.
    let mut stdin = child.stdin.take().context("no stdin for t3")?;
    let input = message.as_bytes().to_vec();
    let (wtx, wrx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = wtx.send(stdin.write_all(&input));
    });
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if started.elapsed() > deadline {
            kill(&mut child);
            bail!("t3 thread send did not finish in {} s", deadline.as_secs());
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    let grace = Duration::from_secs(2);
    let wrote = wrx.recv_timeout(grace).ok();
    let out = stdout.recv_timeout(grace).unwrap_or_default();
    let err = stderr.recv_timeout(grace).unwrap_or_default();
    if !status.success() {
        let text = if err.trim().is_empty() { out } else { err };
        bail!("t3 thread send: {}", clip(text.trim(), 600));
    }
    if let Some(Err(e)) = wrote {
        bail!("t3 thread send exited without reading the message: {e}");
    }
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn sh(script: &str) -> Command {
        let mut cmd = Command::new("/bin/sh");
        cmd.args(["-c", script]);
        cmd
    }

    #[test]
    fn delivers_stdin() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("got");
        run(
            &sh(&format!("cat > '{}'", out.display())),
            "привет",
            DEADLINE,
        )
        .unwrap();
        assert_eq!(std::fs::read_to_string(out).unwrap(), "привет");
    }

    #[test]
    fn reports_the_programs_error() {
        let e = run(
            &sh("cat >/dev/null; echo 'thread not found' >&2; exit 1"),
            "x",
            DEADLINE,
        )
        .unwrap_err();
        assert!(e.to_string().contains("thread not found"), "{e}");
    }

    #[test]
    fn a_wrapper_without_exec_is_killed_with_its_children() {
        let dir = tempfile::tempdir().unwrap();
        let mark = dir.path().join("delivered");
        // The "real sender" would write the mark after 3 s; the wrapper waits for it.
        let script = format!("sh -c 'sleep 3; touch \"{}\"'; exit 0", mark.display());
        let e = run(&sh(&script), "x", Duration::from_secs(1)).unwrap_err();
        assert!(e.to_string().contains("did not finish"), "{e}");
        std::thread::sleep(Duration::from_secs(4));
        assert!(
            !mark.exists(),
            "the child kept running and delivered after the timeout"
        );
    }

    #[test]
    fn a_hanging_program_is_killed_in_time() {
        let started = Instant::now();
        let e = run(&sh("sleep 30"), "x", Duration::from_secs(1)).unwrap_err();
        assert!(e.to_string().contains("did not finish"), "{e}");
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn a_grandchild_holding_stdout_does_not_block() {
        let started = Instant::now();
        // Exits at once, but leaves a background process with the pipe open.
        run(
            &sh("cat >/dev/null; (sleep 20 &) ; exit 0"),
            "x",
            Duration::from_secs(10),
        )
        .unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(6),
            "{:?}",
            started.elapsed()
        );
    }

    #[test]
    fn a_large_message_to_a_program_that_does_not_read_it_does_not_block() {
        let started = Instant::now();
        let big = "x".repeat(400_000);
        let e = run(
            &sh("echo 'boom' >&2; exit 3"),
            &big,
            Duration::from_secs(10),
        )
        .unwrap_err();
        assert!(e.to_string().contains("boom"), "{e}");
        assert!(started.elapsed() < Duration::from_secs(6));
    }
}
