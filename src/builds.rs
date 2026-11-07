//! Builds: publish into the Syncthing folder, prune, describe, open.
//!
//! Layout of the builds root (one Syncthing folder shared by all projects):
//!
//! ```text
//! <root>/README.md                      projects and where their builds are
//! <root>/<project>/LATEST.md            for people: latest builds, what is new, how to install
//! <root>/<project>/builds.json          for programs: every build kept, newest first
//! <root>/<project>/<platform>/<project>-<YYYYmmdd-HHMM>-<sha7>[-<tag>]-<platform><ext>
//! ```
//!
//! The file name carries the date and the commit, so the folder is readable
//! without topside, and `builds.json` lets topside on another machine (the
//! operator's Mac) find the latest build without the database.

use std::collections::{BTreeMap, HashMap};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::config::{Ctx, valid_name};
use crate::git;
use crate::store::{Build, Gate, NewBuild, Store};
use crate::util::{human_size, local, now};

#[derive(Debug, Serialize, Deserialize)]
pub struct Manifest {
    pub project: String,
    pub title: String,
    pub updated_at: String,
    pub builds: Vec<ManifestBuild>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestBuild {
    pub platform: String,
    /// Relative to the project folder: `macos/<name>`.
    pub file: String,
    pub sha: String,
    pub tag: Option<String>,
    pub stamp: String,
    pub size: u64,
    pub subject: String,
    pub published_at: String,
}

pub struct PublishArgs {
    pub project: Option<String>,
    pub tag: Option<String>,
    pub allow_dirty: bool,
    pub artifacts: Vec<String>,
    pub json: bool,
}

/// `.tar.gz` stays whole; otherwise the last extension.
fn extension_of(name: &str) -> String {
    if name.ends_with(".tar.gz") {
        return ".tar.gz".to_string();
    }
    match Path::new(name).extension().and_then(|e| e.to_str()) {
        Some(ext) => format!(".{ext}"),
        None => String::new(),
    }
}

pub fn build_name(
    project: &str,
    stamp: &str,
    short: &str,
    tag: Option<&str>,
    platform: &str,
    ext: &str,
) -> String {
    match tag {
        Some(tag) => format!("{project}-{stamp}-{short}-{tag}-{platform}{ext}"),
        None => format!("{project}-{stamp}-{short}-{platform}{ext}"),
    }
}

/// Inverse of [`build_name`]: `(stamp, short sha, tag)`. The stamp is
/// `YYYYmmdd-HHMM`, or `YYYYmmdd-HHMMSS` when two builds of a minute collided.
pub fn parse_name(
    project: &str,
    platform: &str,
    name: &str,
) -> Option<(String, String, Option<String>)> {
    if !name.is_ascii() {
        return None; // topside never writes such names; byte offsets below need ASCII
    }
    let rest = name.strip_prefix(project)?.strip_prefix('-')?;
    let ext = extension_of(rest);
    let rest = rest.strip_suffix(ext.as_str())?;
    let rest = rest.strip_suffix(platform)?.strip_suffix('-')?;
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|c| c.is_ascii_digit());
    let (date, rest) = rest.split_at_checked(8)?;
    let rest = rest.strip_prefix('-')?;
    let time_len = rest.find('-')?;
    if !digits(date) || !(time_len == 4 || time_len == 6) || !digits(&rest[..time_len]) {
        return None;
    }
    let stamp = format!("{date}-{}", &rest[..time_len]);
    let rest = &rest[time_len + 1..];
    let (sha, tag) = match rest.split_once('-') {
        Some((sha, tag)) => (sha, Some(tag.to_string())),
        None => (rest, None),
    };
    if sha.len() < 7 || !sha.bytes().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    Some((stamp, sha.to_string(), tag))
}

/// A name unique to this process, thread and moment: for temporary files.
fn unique_suffix() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    format!(
        "{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}

/// Move a finished file into the synced folder in one step, so Syncthing never
/// picks up a half-written build.
fn place(src_tmp: &Path, dest: &Path) -> Result<()> {
    if std::fs::rename(src_tmp, dest).is_ok() {
        return Ok(());
    }
    // Different filesystem: copy beside the target under a name Syncthing skips
    // (its own temporary-file pattern), then rename within the folder.
    let dir = dest.parent().context("destination has no parent")?;
    let name = dest.file_name().and_then(|n| n.to_str()).unwrap_or("build");
    let staged = dir.join(format!(".syncthing.{name}.{}.tmp", unique_suffix()));
    let moved = std::fs::copy(src_tmp, &staged)
        .with_context(|| format!("cannot copy into {}", dir.display()))
        .and_then(|_| Ok(std::fs::rename(&staged, dest)?));
    if moved.is_err() {
        // A partial copy in the synced folder is never cleaned by anyone else.
        let _ = std::fs::remove_file(&staged);
    }
    moved?;
    let _ = std::fs::remove_file(src_tmp);
    Ok(())
}

/// Names that look like keys or credentials: never published.
fn secret_looking(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    let key_file = ["id_rsa", "id_ed25519", "id_ecdsa", "id_dsa"]
        .iter()
        .any(|k| lower == *k || lower.starts_with(&format!("{k}.")));
    key_file
        || (lower.starts_with("ssh_host_") && lower.contains("_key"))
        || lower == ".git"
        || lower == "secrets"
        || lower == "secret"
        // .env, release.env, .env.local, release.env.ci, config.env.json: anything with
        // ".env." is refused unless it ends as an asset does (sky.env.hdr).
        || lower.ends_with(".env")
        || (lower.contains(".env.") && !asset_named(&lower))
        || lower == ".netrc"
        || lower == ".npmrc"
        || lower == ".git-credentials"
        || lower == "credentials"
        || lower.starts_with("credentials.")
        || lower == "keystore.properties"
        || lower == "export_credentials.cfg"
        || [".key", ".pem", ".p12", ".pfx", ".keystore", ".jks", ".gpg"]
            .iter()
            .any(|ext| lower.ends_with(ext))
}

/// Extensions of the textures, sounds, models and resources a loose export can carry.
fn asset_named(lower: &str) -> bool {
    const ASSETS: &[&str] = &[
        "hdr", "exr", "png", "jpg", "jpeg", "webp", "svg", "bmp", "tga", "dds", "ktx", "ktx2",
        "ctex", "ogg", "wav", "mp3", "glb", "gltf", "obj", "fbx", "res", "tres", "tscn", "scn",
        "import",
    ];
    lower
        .rsplit_once('.')
        .is_some_and(|(_, ext)| ASSETS.contains(&ext))
}

/// Hidden entries an export may carry: Finder's, and Godot's "do not import this folder".
fn harmless_hidden(name: &str) -> bool {
    name == ".DS_Store" || name == ".gdignore"
}

/// How long a lock that is "not waited for" is still tried. A lock is held by
/// the open file, and a process another thread is starting holds a copy of every
/// descriptor until it executes its program: just after a release the lock can
/// look taken for a moment, which must not turn a refresh away.
const RELEASE_GRACE: Duration = Duration::from_millis(500);

/// Take an exclusive lock on `path`, saying what is awaited; give up after `limit`.
fn lock_file(path: &Path, what: &str, limit: Duration) -> Result<std::fs::File> {
    let file =
        std::fs::File::create(path).with_context(|| format!("cannot create {}", path.display()))?;
    let started = Instant::now();
    let mut told = false;
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(file),
            Err(std::fs::TryLockError::WouldBlock) => {}
            Err(std::fs::TryLockError::Error(e)) => {
                return Err(e).with_context(|| format!("cannot lock {}", path.display()));
            }
        }
        if started.elapsed() >= limit {
            bail!(
                "{what} is still busy after {} s ({}); try again later",
                limit.as_secs(),
                path.display()
            );
        }
        let waited = started.elapsed();
        if !told && limit > RELEASE_GRACE && waited >= RELEASE_GRACE {
            eprintln!("waiting for {what}…");
            told = true;
        }
        // Often at first (a lock that only looks taken frees within milliseconds).
        std::thread::sleep(if waited < RELEASE_GRACE {
            Duration::from_millis(10)
        } else {
            Duration::from_millis(200)
        });
    }
}

/// What is published goes to every device of the Syncthing folder, so it must
/// come from an export, not from settings or secrets. Checked on the real path
/// (a symlink cannot point the check elsewhere), which is also what is packed:
/// - not `/`, a top-level folder, the home folder or above it;
/// - no hidden folder on the way (`.godot/`, `~/.ssh`, `~/.config`), counted
///   from the publishing checkout when the artifact is inside it (worktrees
///   may live in a hidden `.worktrees/`), else from the home folder, else from `/`;
/// - a folder: inside a checkout only where git ignores it (`build/`), and
///   nothing hidden or key-like anywhere inside;
/// - a file: not key-like by name.
fn check_artifact(path: &Path, source: &Path) -> Result<PathBuf> {
    let real = crate::util::canon(path);
    let depth = real
        .components()
        .filter(|c| matches!(c, Component::Normal(_)))
        .count();
    let home = std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(|h| crate::util::canon(Path::new(&h)));
    if depth <= 1 || (home.is_none() && depth <= 2) {
        bail!("{} is too close to / to be a build", path.display());
    }
    if let Some(home) = &home
        && home.starts_with(&real)
    {
        bail!(
            "{} is the home folder or above it, not a build",
            path.display()
        );
    }
    let source = crate::util::canon(source);
    let base = if real.starts_with(&source) {
        source
    } else if let Some(home) = home.as_ref().filter(|h| real.starts_with(h)) {
        home.clone()
    } else {
        PathBuf::from("/")
    };
    if let Ok(below) = real.strip_prefix(&base)
        && let Some(hidden) = below.components().find(|c| {
            let name = c.as_os_str().to_string_lossy();
            name.starts_with('.') || secret_looking(&name)
        })
    {
        bail!(
            "{} is inside {}, a hidden or secrets folder (or itself looks like a secret): \
             such places hold settings and keys, not builds",
            path.display(),
            hidden.as_os_str().to_string_lossy()
        );
    }
    if real.is_file() {
        let name = real
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        if secret_looking(&name) {
            bail!(
                "{} looks like a key or credentials, not a build",
                path.display()
            );
        }
        return Ok(real);
    }
    if git::toplevel(&real).is_some() && !git::ok(&real, &["check-ignore", "-q", "."]) {
        bail!(
            "{} is inside a git checkout and not ignored by git: an export folder (e.g. build/) is",
            path.display()
        );
    }
    let mut stack = vec![real.clone()];
    let mut seen = 0usize;
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir)?.flatten() {
            seen += 1;
            if seen > 200_000 {
                bail!(
                    "{} has over 200000 files: not a build folder",
                    path.display()
                );
            }
            let p = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            if secret_looking(&name) || (name.starts_with('.') && !harmless_hidden(&name)) {
                bail!(
                    "{} contains {}, which is hidden or looks like a repository, key or \
                     credentials: it would go to every device of the Syncthing folder",
                    path.display(),
                    p.display()
                );
            }
            if entry.file_type().is_ok_and(|t| t.is_dir()) {
                stack.push(p);
            }
        }
    }
    Ok(real)
}

/// The pid in a staging name (`<project>-<platform>-<pid>-<n><ext>`).
fn staging_pid(name: &str) -> Option<i32> {
    let stem = name.split('.').next()?;
    let mut parts = stem.rsplit('-');
    parts.next()?.parse::<u64>().ok()?;
    parts.next()?.parse().ok()
}

fn running(pid: i32) -> bool {
    #[cfg(unix)]
    {
        // SAFETY: signal 0 only checks that the process exists.
        let r = unsafe { libc::kill(pid, 0) };
        r == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        true
    }
}

/// Leftovers of publishes that were killed (a tool timeout, Ctrl-C): staging
/// files of processes that are gone. Not by age — a copy keeps its source's time.
fn clean_tmp(tmp: &Path) {
    let Ok(entries) = std::fs::read_dir(tmp) else {
        return;
    };
    let me = std::process::id() as i32;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        let Some(pid) = staging_pid(&name) else {
            continue;
        };
        if pid != me && !running(pid) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Staged files are removed when the publish ends, however it ends (placed ones
/// are gone from here already).
struct StagedFiles(Vec<PathBuf>);

impl Drop for StagedFiles {
    fn drop(&mut self) {
        for path in &self.0 {
            let _ = std::fs::remove_file(path);
        }
    }
}

const PUBLISH_LOCK_WAIT: Duration = Duration::from_secs(600);

/// The publish lock of a project. Whoever holds it — a publish (one that fails
/// too), `reindex`, a refresh — answers for the refreshes it turned away: on
/// release, if one left the stale marker meanwhile, the manifests are written
/// again.
struct PublishLock<'a> {
    file: Option<std::fs::File>,
    ctx: &'a Ctx,
    project: String,
}

impl Drop for PublishLock<'_> {
    fn drop(&mut self) {
        drop(self.file.take());
        // Redone only for a marker actually taken away: one that cannot be removed
        // (no write access, a folder in its place) must not make every release
        // refresh again, and again.
        let marker = stale_marker(self.ctx, &self.project);
        if std::fs::remove_file(&marker).is_ok() {
            match Store::open(&self.ctx.paths) {
                Ok(db) => refresh(self.ctx, &db, &self.project),
                // Left for the next holder.
                Err(_) => {
                    let _ = std::fs::write(&marker, b"");
                }
            }
        }
    }
}

fn publish_lock<'a>(ctx: &'a Ctx, project: &str, limit: Duration) -> Result<PublishLock<'a>> {
    std::fs::create_dir_all(&ctx.paths.data_dir)?;
    let file = lock_file(
        &ctx.paths.data_dir.join(format!("publish-{project}.lock")),
        &format!("another publish of {project}"),
        limit,
    )?;
    Ok(PublishLock {
        file: Some(file),
        ctx,
        project: project.to_string(),
    })
}

pub fn publish(ctx: &Ctx, args: PublishArgs) -> Result<u8> {
    if args.artifacts.is_empty() {
        bail!("nothing to publish: pass <platform>=<path>, e.g. macos=build/macos/game.zip");
    }
    let cwd = std::env::current_dir()?;
    let project = ctx.resolve_project(args.project.as_deref(), &cwd)?;
    // The commit is the checkout the command runs in (a worktree builds its own branch).
    let source = ctx.checkout(&project, &cwd)?;
    if let Some(tag) = &args.tag
        && !valid_name(tag)
    {
        bail!("tag '{tag}' must be lowercase letters, digits and dashes");
    }
    if !args.allow_dirty && git::dirty(&source)? {
        bail!(
            "{} has uncommitted changes: only committed builds are published \
             (a build from local edits cannot be reproduced); commit, or pass --allow-dirty",
            source.display()
        );
    }
    let mut artifacts = Vec::new();
    for spec in &args.artifacts {
        let Some((platform, path)) = spec.split_once('=') else {
            bail!("'{spec}' is not <platform>=<path>");
        };
        if !valid_name(platform) {
            bail!("platform '{platform}' must be lowercase letters, digits and dashes");
        }
        if path.trim().is_empty() {
            bail!("'{spec}': the path is empty (an unset variable in the publish script?)");
        }
        let path = cwd.join(path);
        if !path.exists() {
            bail!("{} does not exist", path.display());
        }
        // Never a checkout, settings or keys: it goes to every device of the folder.
        let path = check_artifact(&path, &source)?;
        if artifacts
            .iter()
            .any(|(p, _): &(String, PathBuf)| p == platform)
        {
            bail!("platform {platform} is given twice");
        }
        artifacts.push((platform.to_string(), path));
    }

    let sha = git::rev(&source, "HEAD")?;
    let short: String = sha.chars().take(7).collect();
    let subject = git::subject(&source, "HEAD");
    let branch = git::branch(&source);
    let now_local = chrono::Local::now();
    let root = ctx.builds_root()?;
    let project_dir = root.join(&project);
    let tmp = ctx.paths.tmp_dir();
    std::fs::create_dir_all(&tmp)?;
    clean_tmp(&tmp);

    // 1. Stage everything outside the synced folder. A failure here leaves the
    //    folder untouched: no half-published set reaches the devices.
    struct Staged {
        platform: String,
        staged: PathBuf,
        ext: String,
        size: u64,
    }
    let mut staged = Vec::new();
    let mut staged_files = StagedFiles(Vec::new());
    for (platform, path) in &artifacts {
        let unique = format!("{project}-{platform}-{}", unique_suffix());
        // Registered before it exists: a tar that fails halfway leaves its part too.
        let ext = if path.is_dir() {
            ".tar.gz".to_string()
        } else {
            extension_of(path.file_name().and_then(|n| n.to_str()).unwrap_or(""))
        };
        staged_files.0.push(tmp.join(format!("{unique}{ext}")));
        let result = (|| -> Result<Staged> {
            if path.is_dir() {
                // A folder export (Linux: binary + pck + libraries) travels as one archive.
                let file = tmp.join(format!("{unique}.tar.gz"));
                let parent = path.parent().context("folder has no parent")?;
                let name = path.file_name().context("folder has no name")?;
                let status = Command::new("tar")
                    .arg("-czf")
                    .arg(&file)
                    .arg("-C")
                    .arg(parent)
                    .arg(name)
                    .status()
                    .context("cannot run tar")?;
                if !status.success() {
                    bail!("tar failed for {}", path.display());
                }
                let size = std::fs::metadata(&file)?.len();
                Ok(Staged {
                    platform: platform.clone(),
                    staged: file,
                    ext: ".tar.gz".into(),
                    size,
                })
            } else {
                let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                let ext = extension_of(name);
                let file = tmp.join(format!("{unique}{ext}"));
                std::fs::copy(path, &file)
                    .with_context(|| format!("cannot copy {}", path.display()))?;
                let size = std::fs::metadata(&file)?.len();
                Ok(Staged {
                    platform: platform.clone(),
                    staged: file,
                    ext,
                    size,
                })
            }
        })();
        staged.push(result?);
    }

    // From here to the record, one publish of a project at a time: two publishes of
    // the same commit would otherwise pick one name and overwrite each other.
    let _lock = publish_lock(ctx, &project, PUBLISH_LOCK_WAIT)?;
    let known = Store::open(&ctx.paths)?;

    // 2. Names: the minute stamp, with seconds when that name is taken (a second
    //    publish of the same commit within a minute must not overwrite the first).
    //    A name is taken if it was ever used, pruned or not.
    let minute = now_local.format("%Y%m%d-%H%M").to_string();
    let seconds = now_local.format("%Y%m%d-%H%M%S").to_string();
    let taken = |stamp: &str| -> Result<bool> {
        for s in &staged {
            let name = build_name(
                &project,
                stamp,
                &short,
                args.tag.as_deref(),
                &s.platform,
                &s.ext,
            );
            if project_dir.join(&s.platform).join(&name).exists()
                || known.file_known(&format!("{project}/{}/{name}", s.platform))?
            {
                return Ok(true);
            }
        }
        Ok(false)
    };
    let stamp = if !taken(&minute)? {
        minute
    } else if !taken(&seconds)? {
        seconds
    } else {
        bail!("builds named {seconds} already exist; try again in a second");
    };

    // 3. Place into the folder, one rename each.
    let mut placed: Vec<(String, String, u64, PathBuf)> = Vec::new();
    for s in &staged {
        let dir = project_dir.join(&s.platform);
        let name = build_name(
            &project,
            &stamp,
            &short,
            args.tag.as_deref(),
            &s.platform,
            &s.ext,
        );
        let dest = dir.join(&name);
        let result = std::fs::create_dir_all(&dir)
            .with_context(|| format!("cannot create {}", dir.display()))
            .and_then(|_| place(&s.staged, &dest));
        if let Err(e) = result {
            for (_, _, _, path) in &placed {
                let _ = std::fs::remove_file(path);
            }
            return Err(e);
        }
        placed.push((
            s.platform.clone(),
            format!("{project}/{}/{name}", s.platform),
            s.size,
            dest,
        ));
    }

    // 4. Record and queue the announcement together; if that fails, take the files back.
    let recorded = (|| -> Result<i64> {
        let mut db = Store::open(&ctx.paths)?;
        let previous = db.last_publish_sha(&project)?;
        let changes = git::subjects_since(&source, previous.as_deref(), &sha, 40);
        let new_builds: Vec<NewBuild> = placed
            .iter()
            .map(|(platform, file, size, _)| NewBuild {
                platform,
                file,
                stamp: &stamp,
                size: *size as i64,
            })
            .collect();
        db.insert_publish(
            &project,
            &sha,
            args.tag.as_deref(),
            branch.as_deref(),
            &subject,
            &changes,
            &new_builds,
        )
    })();
    let publish_id = match recorded {
        Ok(id) => id,
        Err(e) => {
            for (_, _, _, path) in &placed {
                let _ = std::fs::remove_file(path);
            }
            return Err(e.context("the build was not recorded; its files were taken back"));
        }
    };

    // 5. Housekeeping: the build is published whatever happens here.
    let db = Store::open(&ctx.paths)?;
    let platforms: Vec<String> = placed.iter().map(|(p, _, _, _)| p.clone()).collect();
    for platform in &platforms {
        if let Err(e) = prune(ctx, &db, &project, platform) {
            eprintln!("warning: prune {platform}: {e:#}");
        }
    }
    if let Err(e) = write_manifests(ctx, &db, &project) {
        eprintln!("warning: manifests: {e:#}");
    }
    // Released here: a refresh turned away meanwhile (gates changed) is redone now.
    drop(_lock);

    if args.json {
        let files: Vec<_> = placed
            .iter()
            .map(|(platform, _, size, path)| json!({"platform": platform, "path": path, "size": size}))
            .collect();
        println!(
            "{}",
            json!({"project": project, "sha": sha, "publish": publish_id, "files": files})
        );
    } else {
        println!("published to {}:", project_dir.display());
        for (_, file, size, _) in &placed {
            println!(
                "  {}  {}",
                file.trim_start_matches(&format!("{project}/")),
                human_size(*size)
            );
        }
    }
    Ok(0)
}

/// Keep the newest `keep` builds of a platform; delete the rest from the folder.
fn prune(ctx: &Ctx, db: &Store, project: &str, platform: &str) -> Result<()> {
    let root = ctx.builds_root()?;
    for old in db
        .builds(project, Some(platform))?
        .iter()
        .skip(ctx.cfg.keep)
    {
        let path = root.join(&old.file);
        let removed = match std::fs::symlink_metadata(&path) {
            Ok(meta) if meta.is_dir() => std::fs::remove_dir_all(&path),
            Ok(_) => std::fs::remove_file(&path),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        };
        removed.with_context(|| format!("cannot remove {}", path.display()))?;
        db.mark_pruned(old.id)?;
    }
    Ok(())
}

/// Newest build of each platform that contains the gate's commit. A build
/// without a tag wins over a tagged one (a prototype branch also contains main).
pub fn covering(ctx: &Ctx, db: &Store, gate: &Gate) -> Result<Vec<Build>> {
    let Some(commit) = gate.commit_sha.as_deref() else {
        return Ok(Vec::new());
    };
    let repo = ctx.repo(&gate.project)?;
    let mut contains: HashMap<String, bool> = HashMap::new();
    let mut best: BTreeMap<String, (Option<Build>, Option<Build>)> = BTreeMap::new();
    for build in db.builds(&gate.project, None)? {
        let ok = *contains
            .entry(build.sha.clone())
            .or_insert_with(|| git::is_ancestor(&repo, commit, &build.sha));
        if !ok {
            continue;
        }
        let slot = best.entry(build.platform.clone()).or_default();
        if build.tag.is_none() {
            slot.0.get_or_insert(build);
        } else {
            slot.1.get_or_insert(build);
        }
    }
    Ok(best
        .into_values()
        .filter_map(|(plain, tagged)| plain.or(tagged))
        .collect())
}

/// Rewrite the project's manifests when gates change (their list is in `LATEST.md`).
/// Best effort: a machine without a builds folder has nothing to refresh.
pub fn refresh(ctx: &Ctx, db: &Store, project: &str) {
    if ctx.cfg.builds_root.is_none()
        || db
            .builds(project, None)
            .map(|b| b.is_empty())
            .unwrap_or(true)
    {
        return;
    }
    // A publish writes them too: one writer at a time, or an older picture could
    // win. Never waited for beyond RELEASE_GRACE (agent commands and the service
    // must not hang on a publish): when the lock is held, a marker asks its holder
    // to write them again on release (see `PublishLock`).
    let written = (|| -> Result<()> {
        let _lock = match publish_lock(ctx, project, RELEASE_GRACE) {
            Ok(lock) => lock,
            Err(_) => {
                std::fs::write(stale_marker(ctx, project), b"")?;
                // Tried again after the marker: the holder has either not released
                // yet (and will see the marker) or has, and the lock is free now.
                match publish_lock(ctx, project, RELEASE_GRACE) {
                    Ok(lock) => lock,
                    Err(_) => return Ok(()),
                }
            }
        };
        let _ = std::fs::remove_file(stale_marker(ctx, project));
        write_manifests(ctx, db, project)
    })();
    if let Err(e) = written {
        crate::util::warn(format_args!(
            "warning: cannot refresh the manifests of {project}: {e:#}"
        ));
    }
}

fn stale_marker(ctx: &Ctx, project: &str) -> PathBuf {
    ctx.paths
        .data_dir
        .join(format!("manifests-stale-{project}"))
}

/// Rewrite `builds.json`, `LATEST.md` and the root `README.md` from the database.
pub fn write_manifests(ctx: &Ctx, db: &Store, project: &str) -> Result<()> {
    let root = ctx.builds_root()?;
    let project_dir = root.join(project);
    std::fs::create_dir_all(&project_dir)?;
    let builds = db.builds(project, None)?;
    let prefix = format!("{project}/");
    let manifest = Manifest {
        project: project.to_string(),
        title: ctx.title(project),
        updated_at: now(),
        builds: builds
            .iter()
            .map(|b| ManifestBuild {
                platform: b.platform.clone(),
                file: b.file.trim_start_matches(&prefix).to_string(),
                sha: b.sha.clone(),
                tag: b.tag.clone(),
                stamp: b.stamp.clone(),
                size: b.size as u64,
                subject: b.subject.clone(),
                published_at: b.created_at.clone(),
            })
            .collect(),
    };
    write_atomic(
        &project_dir.join("builds.json"),
        serde_json::to_string_pretty(&manifest)?.as_bytes(),
    )?;
    write_atomic(
        &project_dir.join("LATEST.md"),
        latest_md(ctx, db, project, &builds)?.as_bytes(),
    )?;
    write_atomic(&root.join("README.md"), root_readme(&root)?.as_bytes())?;
    Ok(())
}

fn write_atomic(path: &Path, data: &[u8]) -> Result<()> {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");
    // Unique per process: two projects publishing at once both rewrite README.md.
    let tmp = path.with_file_name(format!(".syncthing.{name}.{}.tmp", unique_suffix()));
    std::fs::write(&tmp, data).with_context(|| format!("cannot write {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("cannot replace {}", path.display()))?;
    Ok(())
}

fn latest_md(ctx: &Ctx, db: &Store, project: &str, builds: &[Build]) -> Result<String> {
    let mut out = format!("# {} builds\n\n", ctx.title(project));
    out.push_str(&format!(
        "Updated {}. topside on the build machine writes this folder; do not edit it by hand.\n\n",
        local(&now())
    ));
    let mut latest: BTreeMap<&str, &Build> = BTreeMap::new();
    for b in builds {
        latest.entry(b.platform.as_str()).or_insert(b);
    }
    if latest.is_empty() {
        out.push_str("No builds yet.\n");
        return Ok(out);
    }
    out.push_str("## Latest\n\n| platform | file | commit | size |\n|---|---|---|---|\n");
    for (platform, b) in &latest {
        out.push_str(&format!(
            "| {platform} | `{}` | `{}` {} | {} |\n",
            b.file.trim_start_matches(&format!("{project}/")),
            &b.sha[..b.sha.len().min(7)],
            b.subject.replace('|', "/"),
            human_size(b.size as u64)
        ));
    }
    if let Some(publish_id) = builds.first().and_then(|b| b.publish_id) {
        let publish = db.publish(publish_id)?;
        if !publish.changes.is_empty() {
            out.push_str(&format!(
                "\n## What is new in `{}`\n\n",
                &publish.sha[..7.min(publish.sha.len())]
            ));
            for change in &publish.changes {
                out.push_str(&format!("- {change}\n"));
            }
        }
    }
    let open = db.gates(Some(project), Some("open"))?;
    if !open.is_empty() {
        out.push_str("\n## Waiting for a check (gates)\n\n");
        for gate in &open {
            let link = gate
                .tg_anchor
                .and_then(|m| ctx.message_url(project, m))
                .map(|u| format!(" — {u}"))
                .unwrap_or_default();
            out.push_str(&format!("- #{} {}{link}\n", gate.id, gate.title));
        }
    }
    out.push_str("\n## How to install\n\n");
    for platform in latest.keys() {
        let line = match *platform {
            "android" => {
                "- **Android:** open the APK from `android/` and install it. A build signed with \
                          another machine's key does not install over this one: uninstall that first."
            }
            "macos" => {
                "- **macOS:** `topside build open <project>` unpacks the latest one and starts it; \
                        by hand, unpack the archive from `macos/` and start it the first time with \
                        right click → Open (ad-hoc signature)."
            }
            "linux" => "- **Linux:** unpack the archive from `linux/` and run `*.x86_64`.",
            "windows" => "- **Windows:** unpack the archive from `windows/` and run the `.exe`.",
            _ => continue,
        };
        out.push_str(&line.replace("<project>", project));
        out.push('\n');
    }
    Ok(out)
}

fn root_readme(root: &Path) -> Result<String> {
    let mut out = String::from(
        "# Builds\n\nSyncthing shares this folder; topside writes it. Per project: `<project>/LATEST.md`.\n\n",
    );
    let mut names: Vec<String> = std::fs::read_dir(root)?
        .filter_map(|e| e.ok())
        .filter(|e| e.path().join("builds.json").exists())
        .filter_map(|e| e.file_name().into_string().ok())
        .collect();
    names.sort();
    for name in names {
        out.push_str(&format!("- [{name}]({name}/LATEST.md)\n"));
    }
    Ok(out)
}

pub fn read_manifest(root: &Path, project: &str) -> Result<Option<Manifest>> {
    let path = root.join(project).join("builds.json");
    if !path.exists() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(&path)?;
    Ok(Some(
        serde_json::from_str(&text).with_context(|| format!("invalid {}", path.display()))?,
    ))
}

fn manifests(root: &Path, project: Option<&str>) -> Result<Vec<Manifest>> {
    let names: Vec<String> = match project {
        Some(p) => vec![p.to_string()],
        None => {
            let mut v: Vec<String> = std::fs::read_dir(root)
                .with_context(|| format!("cannot read {}", root.display()))?
                .filter_map(|e| e.ok())
                .filter_map(|e| e.file_name().into_string().ok())
                .collect();
            v.sort();
            v
        }
    };
    let mut out = Vec::new();
    for name in names {
        if let Some(m) = read_manifest(root, &name)? {
            out.push(m);
        }
    }
    Ok(out)
}

pub fn list(ctx: &Ctx, project: Option<&str>, platform: Option<&str>, as_json: bool) -> Result<u8> {
    let root = ctx.builds_root()?;
    let all = manifests(&root, project)?;
    if as_json {
        let rows: Vec<_> = all
            .iter()
            .flat_map(|m| {
                m.builds
                    .iter()
                    .filter(|b| platform.is_none_or(|p| b.platform == p))
                    .map(|b| {
                        let mut v = serde_json::to_value(b).unwrap_or_default();
                        v["project"] = json!(m.project);
                        v["path"] = json!(root.join(&m.project).join(&b.file));
                        v
                    })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(0);
    }
    if all.is_empty() {
        println!("no builds under {}", root.display());
    }
    for m in &all {
        println!("{}", m.project);
        for b in m
            .builds
            .iter()
            .filter(|b| platform.is_none_or(|p| b.platform == p))
        {
            println!(
                "  {:<8} {}  {}  {}",
                b.platform,
                local(&b.published_at),
                human_size(b.size),
                b.file
            );
        }
    }
    Ok(0)
}

fn default_platform() -> &'static str {
    if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(target_os = "windows") {
        "windows"
    } else {
        "linux"
    }
}

fn pick<'a>(m: &'a Manifest, platform: &str, tag: Option<&str>) -> Option<&'a ManifestBuild> {
    m.builds
        .iter()
        .filter(|b| b.platform == platform)
        .find(|b| tag.is_none_or(|t| b.tag.as_deref() == Some(t)))
}

pub fn latest(ctx: &Ctx, project: &str, platform: Option<&str>, tag: Option<&str>) -> Result<u8> {
    let root = ctx.builds_root()?;
    let platform = platform.unwrap_or(default_platform());
    let Some(m) = read_manifest(&root, project)? else {
        bail!("no builds of {project} under {}", root.display());
    };
    let Some(b) = pick(&m, platform, tag) else {
        bail!("no {platform} build of {project}");
    };
    println!("{}", root.join(project).join(&b.file).display());
    Ok(0)
}

/// Unpack the latest build into the cache and start it.
pub fn open(
    ctx: &Ctx,
    project: &str,
    platform: Option<&str>,
    tag: Option<&str>,
    print_only: bool,
) -> Result<u8> {
    let root = ctx.builds_root()?;
    let platform = platform.unwrap_or(default_platform());
    let Some(m) = read_manifest(&root, project)? else {
        bail!("no builds of {project} under {}", root.display());
    };
    let Some(b) = pick(&m, platform, tag) else {
        bail!("no {platform} build of {project}");
    };
    let archive = root.join(project).join(&b.file);
    if !archive.exists() {
        bail!(
            "{} is listed but not here yet — Syncthing may still be copying it",
            archive.display()
        );
    }
    let (app, _lock) = unpack(ctx, project, &archive)?;
    if print_only {
        println!("{}", app.display());
        return Ok(0);
    }
    println!(
        "{} ({}, {})",
        app.display(),
        &b.sha[..7.min(b.sha.len())],
        b.subject
    );
    start(&app)?;
    Ok(0)
}

/// Marker written after a complete unpack: a folder without it is not trusted.
const UNPACKED: &str = ".topside-unpacked";
/// Unpacked builds kept per project: the one being started and two before it,
/// so a game still running from an older build keeps its files.
const KEEP_UNPACKED: usize = 3;

/// Unpack an archive into the cache (once) and find what to start in it. The
/// returned lock is the cache's: hold it until the build is started, so that no
/// other unpack cleans the folder up in between.
pub fn unpack(ctx: &Ctx, project: &str, archive: &Path) -> Result<(PathBuf, std::fs::File)> {
    let name = archive
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("build");
    let stem = name
        .trim_end_matches(".tar.gz")
        .trim_end_matches(".zip")
        .to_string();
    let cache = ctx.paths.cache_dir.join(project);
    std::fs::create_dir_all(&cache)?;
    // One unpack of a project at a time: two taps on a button must not write
    // into the same folder or delete each other's result.
    let lock = std::fs::File::create(cache.join(".lock"))?;
    lock.lock().context("cannot lock the unpack cache")?;
    let target = cache.join(&stem);
    if !target.join(UNPACKED).exists() {
        let _ = std::fs::remove_dir_all(&target);
        let partial = cache.join(format!(".{stem}.{}.partial", std::process::id()));
        let _ = std::fs::remove_dir_all(&partial);
        std::fs::create_dir_all(&partial)?;
        let status = if name.ends_with(".zip") {
            if cfg!(target_os = "macos") {
                Command::new("ditto")
                    .arg("-x")
                    .arg("-k")
                    .arg(archive)
                    .arg(&partial)
                    .status()
            } else {
                Command::new("unzip")
                    .arg("-q")
                    .arg(archive)
                    .arg("-d")
                    .arg(&partial)
                    .status()
            }
        } else if name.ends_with(".tar.gz") {
            Command::new("tar")
                .arg("-xzf")
                .arg(archive)
                .arg("-C")
                .arg(&partial)
                .status()
        } else {
            let _ = std::fs::remove_dir_all(&partial);
            bail!(
                "{name} is not an archive topside can open; the file is {}",
                archive.display()
            );
        }
        .context("cannot run the unpacker")?;
        if !status.success() {
            let _ = std::fs::remove_dir_all(&partial);
            bail!("cannot unpack {}", archive.display());
        }
        std::fs::write(partial.join(UNPACKED), name)?;
        std::fs::rename(&partial, &target)?;
    }
    // The marker's time is the last use: cleanup keeps what was started recently.
    std::fs::write(target.join(UNPACKED), name)?;
    // Older unpacked builds: keep a few, newest first; leftovers of failed runs go.
    if let Ok(entries) = std::fs::read_dir(&cache) {
        let mut others: Vec<(std::time::SystemTime, PathBuf)> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_dir() && *p != target)
            .map(|p| {
                let time = std::fs::metadata(p.join(UNPACKED))
                    .and_then(|m| m.modified())
                    .unwrap_or(std::time::UNIX_EPOCH);
                (time, p)
            })
            .collect();
        others.sort_by_key(|o| std::cmp::Reverse(o.0));
        let mut kept = 1;
        for (_, path) in others {
            let complete = path.join(UNPACKED).exists();
            if complete && kept < KEEP_UNPACKED {
                kept += 1;
                continue;
            }
            let _ = std::fs::remove_dir_all(path);
        }
    }
    let app = find_launchable(&target, &target, 3).with_context(|| {
        format!(
            "nothing to start in {} (no .app or .x86_64)",
            target.display()
        )
    })?;
    Ok((app, lock))
}

/// Start an unpacked build: an .app through Launch Services, a binary detached.
pub fn start(app: &Path) -> Result<()> {
    if app.extension().is_some_and(|e| e == "app") {
        // Best effort: files from Syncthing usually carry no quarantine, and a running app refuses.
        let _ = Command::new("xattr")
            .args(["-dr", "com.apple.quarantine"])
            .arg(app)
            .stderr(Stdio::null())
            .status();
        let status = Command::new("open").arg(app).status()?;
        if !status.success() {
            bail!("open failed for {}", app.display());
        }
    } else {
        Command::new(app)
            .current_dir(app.parent().unwrap_or(Path::new("/")))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .with_context(|| format!("cannot start {}", app.display()))?;
    }
    Ok(())
}

/// The .app or Linux binary inside an unpacked build. Symlinks are not
/// followed: whatever starts must be a real file inside the unpacked folder.
fn find_launchable(root: &Path, dir: &Path, depth: usize) -> Option<PathBuf> {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| std::fs::symlink_metadata(p).is_ok_and(|m| !m.file_type().is_symlink()))
        .collect();
    entries.sort();
    let inside = |p: &Path| crate::util::canon(p).starts_with(crate::util::canon(root));
    for path in &entries {
        let name = path.file_name()?.to_str()?;
        if path.is_dir() && name.ends_with(".app") && inside(path) {
            return Some(path.clone());
        }
        if path.is_file() && (name.ends_with(".x86_64") || name.ends_with(".arm64")) && inside(path)
        {
            return Some(path.clone());
        }
    }
    if depth == 0 {
        return None;
    }
    entries
        .iter()
        .filter(|p| p.is_dir())
        .find_map(|p| find_launchable(root, p, depth - 1))
}

/// `YYYYmmdd-HHMM` or `YYYYmmdd-HHMMSS` in local time → a stored (UTC) timestamp.
fn stamp_time(stamp: &str) -> String {
    use chrono::TimeZone;
    chrono::NaiveDateTime::parse_from_str(stamp, "%Y%m%d-%H%M%S")
        .or_else(|_| chrono::NaiveDateTime::parse_from_str(stamp, "%Y%m%d-%H%M"))
        .ok()
        .and_then(|t| chrono::Local.from_local_datetime(&t).earliest())
        .map(|t| {
            t.with_timezone(&chrono::Utc)
                .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
        })
        .unwrap_or_else(now)
}

/// Register builds already in the folder (moved in by hand, or after a lost database).
pub fn reindex(ctx: &Ctx, project: &str) -> Result<u8> {
    ctx.project(project)?;
    // It rewrites the manifests: not in the middle of a publish.
    let _lock = publish_lock(ctx, project, PUBLISH_LOCK_WAIT)?;
    let repo = ctx.repo(project)?;
    let root = ctx.builds_root()?;
    let dir = root.join(project);
    let db = Store::open(&ctx.paths)?;
    let (mut added, mut skipped) = (0, 0);
    for platform_dir in std::fs::read_dir(&dir)
        .with_context(|| format!("cannot read {}", dir.display()))?
        .flatten()
        .filter(|e| e.path().is_dir())
    {
        let platform = platform_dir.file_name().to_string_lossy().to_string();
        for file in std::fs::read_dir(platform_dir.path())?.flatten() {
            if !file.file_type().is_ok_and(|t| t.is_file()) {
                continue; // builds are files; a folder here is not one
            }
            let name = file.file_name().to_string_lossy().to_string();
            let Some((stamp, short, tag)) = parse_name(project, &platform, &name) else {
                if !name.starts_with('.') {
                    eprintln!(
                        "skip {platform}/{name}: name is not <project>-<stamp>-<sha>[-tag]-<platform>"
                    );
                    skipped += 1;
                }
                continue;
            };
            let sha = git::rev(&repo, &short).unwrap_or(short.clone());
            let subject = git::subject(&repo, &sha);
            let size = file.metadata()?.len() as i64;
            let rel = format!("{project}/{platform}/{name}");
            if db.adopt_build(
                project,
                &platform,
                &rel,
                &sha,
                tag.as_deref(),
                &stamp,
                size,
                &subject,
                &stamp_time(&stamp),
            )? {
                added += 1;
            }
        }
    }
    write_manifests(ctx, &db, project)?;
    println!("{project}: {added} builds registered, {skipped} files skipped");
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_round_trip() {
        let n = build_name("my-game", "20261107-1530", "abc1234", None, "macos", ".zip");
        assert_eq!(n, "my-game-20261107-1530-abc1234-macos.zip");
        assert_eq!(
            parse_name("my-game", "macos", &n),
            Some(("20261107-1530".into(), "abc1234".into(), None))
        );
        let t = build_name(
            "my-game",
            "20261107-1530",
            "abc1234",
            Some("proto-build"),
            "linux",
            ".tar.gz",
        );
        assert_eq!(
            parse_name("my-game", "linux", &t),
            Some((
                "20261107-1530".into(),
                "abc1234".into(),
                Some("proto-build".into())
            ))
        );
    }

    #[test]
    fn names_with_seconds_and_strangers() {
        assert_eq!(
            parse_name(
                "my-game",
                "macos",
                "my-game-20261107-153012-abc1234-macos.zip"
            ),
            Some(("20261107-153012".into(), "abc1234".into(), None))
        );
        // Non-ASCII names are not topside's: skipped, not a panic.
        assert_eq!(
            parse_name("my-game", "macos", "my-game-v1.20-финальная-macos.zip"),
            None
        );
        assert_eq!(
            parse_name("my-game", "macos", "my-game-2026110-1530-abc1234-macos.zip"),
            None
        );
        assert_eq!(
            parse_name(
                "my-game",
                "macos",
                "my-game-20261107-15301-abc1234-macos.zip"
            ),
            None
        );
    }

    #[test]
    fn rejects_foreign_names() {
        assert_eq!(parse_name("my-game", "android", "my-game-debug.apk"), None);
        assert_eq!(
            parse_name(
                "my-game",
                "android",
                "other-20261107-1530-abc1234-android.apk"
            ),
            None
        );
        assert_eq!(
            parse_name(
                "my-game",
                "android",
                "my-game-2026110-1530-abc1234-android.apk"
            ),
            None
        );
        assert_eq!(
            parse_name(
                "my-game",
                "macos",
                "my-game-20261107-1530-abc1234-android.apk"
            ),
            None
        );
    }

    #[cfg(unix)]
    #[test]
    fn concurrent_unpacks_of_one_build_agree() {
        use crate::config::{Config, Paths};
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src/linux");
        std::fs::create_dir_all(&src).unwrap();
        for i in 0..300 {
            std::fs::write(src.join(format!("res{i}.dat")), vec![b'x'; 2048]).unwrap();
        }
        std::fs::write(src.join("game.x86_64"), b"bin").unwrap();
        let archive = dir.path().join("p-20261107-1200-abc1234-linux.tar.gz");
        let ok = Command::new("tar")
            .arg("-czf")
            .arg(&archive)
            .arg("-C")
            .arg(dir.path().join("src"))
            .arg("linux")
            .status()
            .unwrap()
            .success();
        assert!(ok);
        let paths = || Paths {
            config_file: dir.path().join("config.toml"),
            data_dir: dir.path().join("data"),
            cache_dir: dir.path().join("cache"),
        };
        let handles: Vec<_> = (0..4)
            .map(|_| {
                let archive = archive.clone();
                let ctx = Ctx {
                    cfg: Config::default(),
                    paths: paths(),
                };
                // Each thread releases the cache lock itself, as callers do after starting.
                std::thread::spawn(move || unpack(&ctx, "p", &archive).map(|(app, _lock)| app))
            })
            .collect();
        for h in handles {
            let app = h.join().unwrap().unwrap();
            assert!(app.ends_with("linux/game.x86_64"), "{}", app.display());
        }
        let target = dir
            .path()
            .join("cache/p/p-20261107-1200-abc1234-linux/linux");
        assert_eq!(
            std::fs::read_dir(&target).unwrap().count(),
            301,
            "every file is there"
        );
    }

    #[cfg(unix)]
    #[test]
    fn only_exports_are_published() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("game");
        std::fs::create_dir_all(repo.join("build/linux")).unwrap();
        std::fs::create_dir_all(repo.join(".godot")).unwrap();
        std::fs::create_dir_all(repo.join("src")).unwrap();
        std::fs::write(repo.join(".gitignore"), "build/\n.godot/\n").unwrap();
        std::fs::write(repo.join("build/linux/game.x86_64"), b"bin").unwrap();
        std::fs::write(repo.join("build/game.apk"), b"apk").unwrap();
        std::fs::write(repo.join(".godot/export_credentials.cfg"), b"secret").unwrap();
        assert!(git::ok(&repo, &["init", "-q"]));
        let real = check_artifact(&repo.join("build/linux"), &repo).unwrap();
        assert!(real.ends_with("build/linux"));
        assert!(check_artifact(&repo.join("build/game.apk"), &repo).is_ok());
        let e = check_artifact(&repo.join(".godot"), &repo).unwrap_err();
        assert!(e.to_string().contains("hidden"), "{e}");
        let e = check_artifact(&repo.join(".godot/export_credentials.cfg"), &repo).unwrap_err();
        assert!(e.to_string().contains("hidden"), "{e}");
        let e = check_artifact(&repo.join("src"), &repo).unwrap_err();
        assert!(e.to_string().contains("not ignored"), "{e}");
        // Ignored by git, but a key or a hidden folder slipped into the export.
        std::fs::write(repo.join("build/linux/release.keystore"), b"k").unwrap();
        let e = check_artifact(&repo.join("build/linux"), &repo).unwrap_err();
        assert!(e.to_string().contains("release.keystore"), "{e}");
        std::fs::remove_file(repo.join("build/linux/release.keystore")).unwrap();
        std::fs::create_dir_all(repo.join("build/linux/.aws")).unwrap();
        assert!(check_artifact(&repo.join("build/linux"), &repo).is_err());
        std::fs::remove_dir(repo.join("build/linux/.aws")).unwrap();
        std::fs::write(repo.join("build/linux/.DS_Store"), b"").unwrap();
        std::fs::write(repo.join("build/linux/.gdignore"), b"").unwrap();
        assert!(check_artifact(&repo.join("build/linux"), &repo).is_ok());
        // Ignored secrets (both games have such): a folder of them, or one file.
        std::fs::create_dir_all(repo.join("secrets")).unwrap();
        std::fs::write(repo.join("secrets/kanban.env"), b"TOKEN=x").unwrap();
        std::fs::write(repo.join("release.env"), b"PASS=x").unwrap();
        std::fs::write(
            repo.join(".gitignore"),
            "build/\n.godot/\nsecrets/\nrelease.env\n",
        )
        .unwrap();
        assert!(check_artifact(&repo.join("secrets"), &repo).is_err());
        assert!(check_artifact(&repo.join("secrets/kanban.env"), &repo).is_err());
        assert!(check_artifact(&repo.join("release.env"), &repo).is_err());
        // Outside any checkout the contents decide — in a folder whose own name is
        // not hidden (tempdir's ".tmpXXXX" would decide it already).
        let plain = tempfile::Builder::new().prefix("wt").tempdir().unwrap();
        let export = plain.path().join("out/linux");
        std::fs::create_dir_all(&export).unwrap();
        std::fs::write(export.join("game.x86_64"), b"bin").unwrap();
        std::fs::write(export.join("sky.env.hdr"), b"img").unwrap();
        assert!(
            check_artifact(&export, &repo).is_ok(),
            "a plain export outside"
        );
        let keys = plain.path().join("keys");
        std::fs::create_dir_all(&keys).unwrap();
        std::fs::write(keys.join("id_ed25519"), b"k").unwrap();
        let e = check_artifact(&keys, &repo).unwrap_err();
        assert!(e.to_string().contains("id_ed25519"), "by contents: {e}");
        assert!(check_artifact(&keys.join("id_ed25519"), &repo).is_err());
        // A link to a folder is checked and packed as the folder it points to.
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(repo.join(".godot"), &link).unwrap();
        assert!(check_artifact(&link, &repo).is_err());
        assert!(check_artifact(Path::new("/"), &repo).is_err());
        assert!(check_artifact(Path::new("/tmp"), &repo).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn a_worktree_in_a_hidden_folder_publishes() {
        // Some projects keep worktrees in <repo>/.worktrees/<name>.
        let dir = tempfile::tempdir().unwrap();
        let tree = dir.path().join("game/.worktrees/g2");
        std::fs::create_dir_all(tree.join("build/linux")).unwrap();
        std::fs::write(tree.join(".gitignore"), "build/\n").unwrap();
        std::fs::write(tree.join("build/linux/game.x86_64"), b"bin").unwrap();
        assert!(git::ok(&tree, &["init", "-q"]));
        assert!(check_artifact(&tree.join("build/linux"), &tree).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn a_checkout_inside_a_hidden_folder_is_not_the_base() {
        // ~/.config as a dotfiles repo with topside/ ignored: still hidden for a publish of a game.
        let dir = tempfile::tempdir().unwrap();
        let game = dir.path().join("game");
        let config = dir.path().join(".config");
        std::fs::create_dir_all(&game).unwrap();
        std::fs::create_dir_all(config.join("topside")).unwrap();
        std::fs::write(config.join(".gitignore"), "topside/\n").unwrap();
        std::fs::write(config.join("topside/config.toml"), b"x").unwrap();
        assert!(git::ok(&config, &["init", "-q"]));
        assert!(check_artifact(&config.join("topside"), &game).is_err());
    }

    #[test]
    fn a_refresh_turned_away_is_redone_when_the_lock_is_released() {
        use crate::config::{Config, Paths};
        let dir = tempfile::tempdir().unwrap();
        let ctx = Ctx {
            cfg: Config {
                builds_root: Some(dir.path().join("builds").display().to_string()),
                ..Config::default()
            },
            paths: Paths {
                config_file: dir.path().join("config.toml"),
                data_dir: dir.path().join("data"),
                cache_dir: dir.path().join("cache"),
            },
        };
        let mut db = Store::open(&ctx.paths).unwrap();
        db.insert_publish(
            "p",
            "abc1234def",
            None,
            None,
            "s",
            &[],
            &[NewBuild {
                platform: "linux",
                file: "p/linux/p-20261107-1200-abc1234-linux.tar.gz",
                stamp: "20261107-1200",
                size: 1,
            }],
        )
        .unwrap();
        let latest = dir.path().join("builds/p/LATEST.md");
        // A holder that is not a successful publish (a failing one, reindex).
        let held = publish_lock(&ctx, "p", Duration::ZERO).unwrap();
        refresh(&ctx, &db, "p");
        assert!(!latest.exists(), "turned away, not waited for");
        assert!(stale_marker(&ctx, "p").exists());
        drop(held);
        assert!(latest.exists(), "redone on release");
        assert!(!stale_marker(&ctx, "p").exists());
        // A marker that cannot be removed (a folder in its place): no endless redo.
        std::fs::create_dir_all(stale_marker(&ctx, "p")).unwrap();
        let held = publish_lock(&ctx, "p", RELEASE_GRACE).unwrap();
        drop(held);
    }

    #[test]
    fn staging_names_carry_the_pid() {
        assert_eq!(staging_pid("my-game-linux-4242-0.tar.gz"), Some(4242));
        assert_eq!(staging_pid("other-game-macos-17-3.zip"), Some(17));
        assert_eq!(staging_pid("README.md"), None);
        let dir = tempfile::tempdir().unwrap();
        let mine = dir
            .path()
            .join(format!("p-linux-{}-0.tar.gz", std::process::id()));
        let gone = dir.path().join("p-linux-999999999-0.tar.gz");
        std::fs::write(&mine, b"").unwrap();
        std::fs::write(&gone, b"").unwrap();
        clean_tmp(dir.path());
        assert!(mine.exists() && !gone.exists());
    }

    #[test]
    fn secret_names() {
        for name in [
            "id_rsa",
            "id_ed25519.pub",
            ".env",
            ".env.local",
            "a.pem",
            "x.KEY",
            "export_credentials.cfg",
            ".git",
            ".git-credentials",
            "credentials",
            "ssh_host_ed25519_key",
            "keystore.properties",
            "secrets.gpg",
            "release.env",
            "kanban.env.example",
            "release.env.ci",
            "release.env.stage",
            "release.env.save",
            "release.env.1",
            "kanban.env.keys",
            "config.env.json",
            "secrets",
        ] {
            assert!(secret_looking(name), "{name}");
        }
        for name in [
            "id_map.json",
            "game.pck",
            "env.cfg",
            "keys.png",
            "libgodot.so",
            "sky.env.hdr",
            "SKY.ENV.EXR",
            "studio.env.png",
            "sky.env.hdr.import",
            "environment.tres",
            "secret_level.tscn",
        ] {
            assert!(!secret_looking(name), "{name}");
        }
    }

    #[test]
    fn extensions() {
        assert_eq!(extension_of("a.tar.gz"), ".tar.gz");
        assert_eq!(extension_of("a.apk"), ".apk");
        assert_eq!(extension_of("a"), "");
    }
}
