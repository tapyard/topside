//! Start a build on the operator's machine from a Telegram button.
//!
//! Telegram accepts only http(s) links, so a button cannot use a custom URL
//! scheme. It points to `http://127.0.0.1:<port>/open?...` instead: tapped in
//! Telegram on the Mac, the link opens in the browser and reaches `topside
//! launcher serve` running there. The service checks the link's signature (an
//! HMAC with a secret shared by the build machine and the Mac — without it any
//! web page could make the Mac start builds), waits until Syncthing has
//! delivered that exact build, unpacks and starts it.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;

use crate::builds;
use crate::config::{Ctx, LauncherConfig, valid_name};
use crate::util::{expand, html};

const WAIT_FOR_BUILD: Duration = Duration::from_secs(300);

fn secret(cfg: &LauncherConfig) -> Result<Vec<u8>> {
    let path = expand(&cfg.secret_file);
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("cannot read the launcher secret {}", path.display()))?;
    let text = text.trim();
    if text.len() < 32 {
        bail!("{} is shorter than 32 characters", path.display());
    }
    Ok(text.as_bytes().to_vec())
}

fn signature(key: &[u8], project: &str, file: &str) -> String {
    let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(key).expect("any key length");
    mac.update(format!("open\n{project}\n{file}").as_bytes());
    mac.finalize()
        .into_bytes()
        .iter()
        .take(16)
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn encode(text: &str) -> String {
    text.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

fn decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(b) => {
                        out.push(b);
                        i += 3;
                        continue;
                    }
                    Err(_) => out.push(b'%'),
                }
            }
            b'+' => out.push(b' '),
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

/// The signed link a button carries; `None` when no launcher is configured (or
/// it is misconfigured: then a warning says why).
/// `file` is relative to the project folder: `macos/<name>`.
pub fn link(ctx: &Ctx, project: &str, file: &str) -> Option<String> {
    try_link(ctx, project, file).unwrap_or_else(|e| {
        crate::util::warn(format_args!("warning: no launch button: {e:#}"));
        None
    })
}

/// [`link`], with the reason when the launcher is set but cannot make one.
pub fn try_link(ctx: &Ctx, project: &str, file: &str) -> Result<Option<String>> {
    let Some(cfg) = ctx.cfg.launcher.as_ref() else {
        return Ok(None);
    };
    // A link Telegram would refuse (e.g. "localhost") must not ride on a gate message.
    listen_addr(&cfg.url)?;
    let key = secret(cfg)?;
    Ok(Some(format!(
        "{}/open?project={}&file={}&sig={}",
        cfg.url.trim_end_matches('/'),
        encode(project),
        encode(file),
        signature(&key, project, file)
    )))
}

/// `macos/<name>`: one platform folder and a plain file name, nothing else.
fn valid_file(file: &str) -> bool {
    let Some((platform, name)) = file.split_once('/') else {
        return false;
    };
    valid_name(platform)
        && !name.is_empty()
        && !name.starts_with('.')
        && !name.contains("..")
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// `http://127.0.0.1:<port>` (a trailing `/` allowed) → `127.0.0.1:<port>`.
/// Nothing else: Telegram rejects `localhost`, and a path or user part would make
/// links the listener does not answer.
pub fn listen_addr(url: &str) -> Result<String> {
    let rest = url
        .strip_prefix("http://")
        .with_context(|| format!("launcher url '{url}' must start with http://"))?;
    let host_port = rest.strip_suffix('/').unwrap_or(rest);
    let Some(port) = host_port.strip_prefix("127.0.0.1:") else {
        bail!(
            "launcher url must be http://127.0.0.1:<port> (Telegram rejects 'localhost'), got '{url}'"
        );
    };
    if port.is_empty()
        || !port.bytes().all(|b| b.is_ascii_digit())
        || port.parse::<u16>().map_or(true, |p| p == 0)
    {
        bail!("launcher url must be http://127.0.0.1:<port>, got '{url}'");
    }
    Ok(host_port.to_string())
}

fn notify(title: &str, text: &str) {
    eprintln!(
        "{} {title}: {text}",
        chrono::Local::now().format("%H:%M:%S")
    );
    if cfg!(target_os = "macos") {
        let quote = |s: &str| s.replace('\\', "\\\\").replace('"', "\\\"");
        let script = format!(
            "display notification \"{}\" with title \"{}\"",
            quote(text),
            quote(title)
        );
        let _ = std::process::Command::new("osascript")
            .args(["-e", &script])
            .status();
    }
}

/// Syncthing is bringing something into the archive's folder: one of its
/// temporary files is there and was written to lately. (Not matched by name:
/// Syncthing shortens long ones. Not any: it keeps failed ones for a day.)
fn arriving(archive: &Path) -> bool {
    let Some(dir) = archive.parent() else {
        return false;
    };
    let recent = |e: &std::fs::DirEntry| {
        e.metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age < Duration::from_secs(120))
    };
    std::fs::read_dir(dir).is_ok_and(|entries| {
        entries.flatten().any(|e| {
            let n = e.file_name().to_string_lossy().to_string();
            (n.starts_with(".syncthing.") || n.starts_with("~syncthing~"))
                && n.ends_with(".tmp")
                && recent(&e)
        })
    })
}

/// Syncthing visibly copying is waited for this long at most.
const WAIT_WHILE_ARRIVING: Duration = Duration::from_secs(3600);
/// A build the manifest does not list (pruned, or the manifest is late).
const WAIT_UNLISTED: Duration = Duration::from_secs(180);

/// Wait until Syncthing has delivered the archive (it appears whole, by a rename;
/// its size is checked to be steady too), then start it. A listed build is waited
/// for five minutes, an unlisted one three — longer while Syncthing is visibly
/// copying into the folder.
fn launch(ctx: &Ctx, project: &str, file: &str) -> Result<PathBuf> {
    let root = ctx.builds_root()?;
    let archive = root.join(project).join(file);
    let started = Instant::now();
    let mut last_size = None;
    let mut told = false;
    // A listed build keeps the long limit once Syncthing was seen copying: between
    // two files its temporary file may be gone for a moment. An unlisted one waits
    // long only while copying is seen right now.
    let mut seen_copying = false;
    loop {
        let size = std::fs::metadata(&archive).ok().map(|m| m.len());
        if size.is_some() && size == last_size {
            break;
        }
        let manifest = builds::read_manifest(&root, project).ok().flatten();
        let listed = manifest
            .as_ref()
            .is_none_or(|m| m.builds.iter().any(|b| b.file == file));
        // Not listed while a newer build of its platform is: most likely pruned.
        // Stamps are the build machine's local time (a time-zone change or the autumn clock
        // change can misorder them), so it gets the short wait, never the long one
        // while some other build is being copied.
        let pruned = size.is_none()
            && !listed
            && manifest
                .as_ref()
                .is_some_and(|m| superseded(m, project, file));
        let copying_now = size.is_none() && arriving(&archive) && !pruned;
        seen_copying = seen_copying || (listed && copying_now);
        let copying = seen_copying || copying_now;
        let limit = if copying {
            WAIT_WHILE_ARRIVING
        } else if listed {
            WAIT_FOR_BUILD
        } else {
            WAIT_UNLISTED
        };
        // The deadline is for a file that is missing: one that has appeared is
        // waited for until its size is steady (bounded below).
        if size.is_none() && started.elapsed() > limit {
            let minutes = limit.as_secs() / 60;
            if pruned {
                bail!(
                    "{file} is no longer among the builds of {project} (pruned: newer ones are there) — use the button of a newer build"
                );
            }
            if copying {
                bail!(
                    "Syncthing has been copying into {} for {minutes} minutes and {file} is still not there",
                    archive.parent().unwrap_or(&root).display()
                );
            }
            if listed {
                bail!(
                    "{} has not arrived in {minutes} minutes — is Syncthing running and connected?",
                    archive.display()
                );
            }
            bail!(
                "{file} is not among the builds of {project} (pruned?) — use the button of a newer build"
            );
        }
        if size.is_some() && started.elapsed() > limit + WAIT_WHILE_ARRIVING {
            bail!(
                "{} keeps changing — something is still writing it",
                archive.display()
            );
        }
        if size.is_none() && !told {
            told = true;
            notify(
                "topside",
                &format!("Waiting for the {project} build: Syncthing has not delivered it yet"),
            );
        }
        last_size = size;
        std::thread::sleep(Duration::from_secs(2));
    }
    // The cache lock is held until the build is started.
    let (app, _lock) = builds::unpack(ctx, project, &archive)?;
    builds::start(&app)?;
    Ok(app)
}

/// The manifest lists a build of the file's platform newer than the file: the
/// file was pruned (builds are listed in publishing order, so a newer one listed
/// means this one would be too, were it kept).
fn superseded(m: &builds::Manifest, project: &str, file: &str) -> bool {
    let Some((platform, name)) = file.split_once('/') else {
        return false;
    };
    let Some((stamp, _, _)) = builds::parse_name(project, platform, name) else {
        return false;
    };
    m.builds
        .iter()
        .any(|b| b.platform == platform && b.stamp.as_str() > stamp.as_str())
}

fn respond(stream: &mut TcpStream, code: &str, body: &str) {
    let page = format!(
        "<!doctype html><meta charset=utf-8><meta name=viewport content='width=device-width'>\
         <title>topside</title><body style='font:16px -apple-system,sans-serif;margin:2em'>{body}"
    );
    let _ = write!(
        stream,
        "HTTP/1.1 {code}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\n\
         Cache-Control: no-store\r\nConnection: close\r\n\r\n{page}",
        page.len()
    );
}

fn handle(key: &[u8], mut stream: TcpStream) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let mut buf = [0u8; 8192];
    let n = stream.read(&mut buf).unwrap_or(0);
    let request = String::from_utf8_lossy(&buf[..n]);
    let target = request
        .lines()
        .next()
        .and_then(|line| line.strip_prefix("GET "))
        .and_then(|rest| rest.split(' ').next())
        .unwrap_or("");
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    match path {
        "/" => return respond(&mut stream, "200 OK", "topside launcher"),
        "/open" => {}
        _ => return respond(&mut stream, "404 Not Found", "not found"),
    }
    let param = |name: &str| {
        query
            .split('&')
            .filter_map(|kv| kv.split_once('='))
            .find(|(k, _)| *k == name)
            .map(|(_, v)| decode(v))
            .unwrap_or_default()
    };
    let (project, file, sig) = (param("project"), param("file"), param("sig"));
    let expected = signature(key, &project, &file);
    let signed = sig.len() == expected.len()
        && sig
            .bytes()
            .zip(expected.bytes())
            .fold(0u8, |acc, (a, b)| acc | (a ^ b))
            == 0;
    if !signed || !valid_name(&project) || !valid_file(&file) {
        eprintln!("rejected request for {project:?} {file:?}");
        return respond(
            &mut stream,
            "403 Forbidden",
            "This link is not signed by this topside.",
        );
    }
    respond(
        &mut stream,
        "200 OK",
        &format!(
            "▶️ Starting <b>{}</b> on this Mac: <code>{}</code>.<p>You can close this tab.",
            html(&project),
            html(&file)
        ),
    );
    // The unpack takes seconds and may wait for Syncthing; the browser already has its answer.
    std::thread::spawn(move || {
        let ctx = match Ctx::load() {
            Ok(ctx) => ctx,
            Err(e) => return notify("topside", &format!("config: {e:#}")),
        };
        match launch(&ctx, &project, &file) {
            Ok(app) => notify(
                "topside",
                &format!(
                    "Started: {}",
                    app.file_name()
                        .map(|n| n.to_string_lossy())
                        .unwrap_or_default()
                ),
            ),
            Err(e) => notify("topside: could not start it", &format!("{e:#}")),
        }
    });
}

pub fn serve(ctx: &Ctx) -> Result<u8> {
    let cfg = ctx.cfg.launcher.as_ref().with_context(|| {
        format!(
            "[launcher] is not set in {}",
            ctx.paths.config_file.display()
        )
    })?;
    let key = secret(cfg)?;
    ctx.builds_root()?;
    let addr = listen_addr(&cfg.url)?;
    let listener = TcpListener::bind(&addr).with_context(|| format!("cannot listen on {addr}"))?;
    eprintln!("topside launcher on http://{addr}");
    // A connection per thread: an idle socket (a browser's pre-opened one, or
    // anything local) must not hold up the next tap.
    for stream in listener.incoming().flatten() {
        let key = key.clone();
        std::thread::spawn(move || handle(&key, stream));
    }
    Ok(0)
}

/// macOS: a LaunchAgent that keeps `topside launcher serve` running.
pub fn install(ctx: &Ctx) -> Result<u8> {
    if !cfg!(target_os = "macos") {
        bail!(
            "launcher install writes a macOS LaunchAgent; elsewhere run `topside launcher serve` as a service"
        );
    }
    let cfg = ctx.cfg.launcher.as_ref().with_context(|| {
        format!(
            "[launcher] is not set in {}",
            ctx.paths.config_file.display()
        )
    })?;
    secret(cfg)?;
    listen_addr(&cfg.url)?;
    let root = ctx.builds_root()?;
    if !root.is_absolute() || !root.is_dir() {
        bail!(
            "builds_root {} must be an existing absolute folder: the service runs from /",
            root.display()
        );
    }
    if !crate::util::expand(&cfg.secret_file).is_absolute() {
        bail!("[launcher].secret_file must be absolute (or start with ~): the service runs from /");
    }
    let exe = std::env::current_exe()?;
    let cwd = std::env::current_dir()?;
    let home = PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?);
    let label = "com.tapyard.topside-launcher";
    let plist = home
        .join("Library/LaunchAgents")
        .join(format!("{label}.plist"));
    let log = home.join("Library/Logs/topside-launcher.log");
    let xml = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{label}</string>
  <key>ProgramArguments</key>
  <array><string>{exe}</string><string>launcher</string><string>serve</string></array>
  <key>EnvironmentVariables</key>
  <dict><key>PATH</key><string>/usr/bin:/bin:/usr/sbin:/sbin</string>{env}</dict>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>StandardOutPath</key><string>{log}</string>
  <key>StandardErrorPath</key><string>{log}</string>
</dict>
</plist>
"#,
        exe = html(&exe.to_string_lossy()),
        // The service must read the configuration that was checked here.
        env = [
            "TOPSIDE_HOME",
            "TOPSIDE_CONFIG",
            "XDG_CONFIG_HOME",
            "XDG_DATA_HOME",
            "XDG_CACHE_HOME"
        ]
        .iter()
        .filter_map(|k| std::env::var(k)
            .ok()
            .filter(|v| !v.is_empty())
            .map(|v| (k, v)))
        // launchd starts the service in /: relative paths would point elsewhere.
        .map(|(k, v)| (k, cwd.join(v).to_string_lossy().to_string()))
        .map(|(k, v)| format!("<key>{k}</key><string>{}</string>", html(&v)))
        .collect::<String>(),
        log = html(&log.to_string_lossy()),
    );
    std::fs::create_dir_all(plist.parent().unwrap_or(Path::new("/")))?;
    std::fs::write(&plist, xml)?;
    // SAFETY: getuid has no preconditions and cannot fail.
    #[cfg(unix)]
    let uid = unsafe { libc::getuid() };
    #[cfg(not(unix))]
    let uid = 0;
    let domain = format!("gui/{uid}");
    // Not loaded yet on the first install: that error is expected, so it is not shown.
    let _ = std::process::Command::new("launchctl")
        .args(["bootout", &format!("{domain}/{label}")])
        .stderr(std::process::Stdio::null())
        .status();
    // Right after a bootout launchd may still be tearing the old job down.
    let mut loaded = false;
    for attempt in 0..5 {
        if attempt > 0 {
            std::thread::sleep(Duration::from_secs(1));
        }
        let status = std::process::Command::new("launchctl")
            .args(["bootstrap", &domain])
            .arg(&plist)
            .status()?;
        if status.success() {
            loaded = true;
            break;
        }
    }
    if !loaded {
        bail!("launchctl bootstrap {} failed", plist.display());
    }
    println!("{} loaded; log: {}", plist.display(), log.display());
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signatures_bind_project_and_file() {
        let key = b"0123456789abcdef0123456789abcdef";
        let a = signature(key, "my-game", "macos/a.zip");
        assert_eq!(a.len(), 32);
        assert_ne!(a, signature(key, "my-game", "macos/b.zip"));
        assert_ne!(a, signature(key, "other", "macos/a.zip"));
        assert_ne!(
            a,
            signature(
                b"another-secret-another-secret-xx",
                "my-game",
                "macos/a.zip"
            )
        );
    }

    #[test]
    fn query_round_trip() {
        let file = "macos/my-game-20261107-1355-655fc5a-macos.zip";
        assert_eq!(decode(&encode(file)), file);
        assert_eq!(decode("a%2Fb%20c"), "a/b c");
        assert_eq!(decode("100%"), "100%");
    }

    #[test]
    fn files_are_confined() {
        assert!(valid_file("macos/my-game-20261107-1355-655fc5a-macos.zip"));
        assert!(!valid_file("../etc/passwd"));
        assert!(!valid_file("macos/../../x.zip"));
        assert!(!valid_file("macos/.hidden.zip"));
        assert!(!valid_file("macos/a/b.zip"));
        assert!(!valid_file("a.zip"));
    }

    #[test]
    fn listens_only_on_loopback() {
        assert_eq!(
            listen_addr("http://127.0.0.1:47711").unwrap(),
            "127.0.0.1:47711"
        );
        assert!(listen_addr("http://localhost:47711").is_err());
        assert!(listen_addr("http://0.0.0.0:47711").is_err());
        assert!(listen_addr("http://127.0.0.1").is_err());
        assert!(listen_addr("https://127.0.0.1:1").is_err());
        assert_eq!(
            listen_addr("http://127.0.0.1:47711/").unwrap(),
            "127.0.0.1:47711"
        );
        assert!(listen_addr("http://127.0.0.1:47711/x").is_err());
        assert!(listen_addr("http://u@127.0.0.1:47711").is_err());
        assert!(listen_addr("http://127.0.0.1:47a").is_err());
        assert!(listen_addr("http://127.0.0.1:99999").is_err());
        assert!(listen_addr("http://127.0.0.1:").is_err());
        assert!(listen_addr("http://127.0.0.1:0").is_err());
    }

    #[test]
    fn a_pruned_build_is_told_apart_from_a_late_manifest() {
        let b = |stamp: &str| builds::ManifestBuild {
            platform: "macos".into(),
            file: format!("macos/p-{stamp}-abc1234-macos.zip"),
            sha: "abc1234".into(),
            tag: None,
            stamp: stamp.into(),
            size: 1,
            subject: String::new(),
            published_at: String::new(),
        };
        let m = builds::Manifest {
            project: "p".into(),
            title: "p".into(),
            updated_at: String::new(),
            builds: vec![b("20261107-1600"), b("20261107-153012")],
        };
        assert!(superseded(
            &m,
            "p",
            "macos/p-20261107-1500-abc1234-macos.zip"
        ));
        assert!(superseded(
            &m,
            "p",
            "macos/p-20261107-1530-abc1234-macos.zip"
        ));
        // Newer than anything listed: the manifest has not arrived yet.
        assert!(!superseded(
            &m,
            "p",
            "macos/p-20261107-1700-abc1234-macos.zip"
        ));
        assert!(!superseded(
            &m,
            "p",
            "linux/p-20261107-1500-abc1234-linux.tar.gz"
        ));
    }

    #[test]
    fn any_syncthing_temp_file_means_arriving() {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir
            .path()
            .join("p-20261107-1200-abc1234-a-very-long-tag-macos.zip");
        assert!(!arriving(&archive));
        std::fs::write(dir.path().join(".syncthing.p-2026110~e3b0c442.tmp"), b"").unwrap();
        assert!(arriving(&archive));
    }
}
