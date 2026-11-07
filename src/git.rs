//! Thin wrappers over the git CLI. topside only reads repositories.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

fn run(dir: &Path, args: &[&str]) -> Result<std::process::Output> {
    Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .with_context(|| format!("cannot run git in {}", dir.display()))
}

/// Trimmed stdout of a git command that must succeed.
pub fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let out = run(dir, args)?;
    if !out.status.success() {
        bail!(
            "git {} in {}: {}",
            args.join(" "),
            dir.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

pub fn ok(dir: &Path, args: &[&str]) -> bool {
    run(dir, args).map(|o| o.status.success()).unwrap_or(false)
}

/// Root of the checkout (worktree) containing `dir`.
pub fn toplevel(dir: &Path) -> Option<PathBuf> {
    git(dir, &["rev-parse", "--show-toplevel"])
        .ok()
        .map(PathBuf::from)
}

/// The main repository behind `dir`: for a linked worktree, the checkout that owns `.git`.
pub fn main_root(dir: &Path) -> Option<PathBuf> {
    let common = git(
        dir,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )
    .ok()?;
    let common = PathBuf::from(common);
    if common.file_name().is_some_and(|n| n == ".git") {
        common.parent().map(Path::to_path_buf)
    } else {
        // A bare repository: the common dir is the repository itself.
        Some(common)
    }
}

pub fn rev(dir: &Path, rev: &str) -> Result<String> {
    git(
        dir,
        &["rev-parse", "--verify", &format!("{rev}^{{commit}}")],
    )
}

pub fn branch(dir: &Path) -> Option<String> {
    git(dir, &["rev-parse", "--abbrev-ref", "HEAD"])
        .ok()
        .filter(|b| b != "HEAD")
}

pub fn subject(dir: &Path, rev: &str) -> String {
    git(dir, &["log", "-1", "--format=%s", rev]).unwrap_or_default()
}

pub fn dirty(dir: &Path) -> Result<bool> {
    Ok(!git(dir, &["status", "--porcelain"])?.is_empty())
}

pub fn exists(dir: &Path, rev: &str) -> bool {
    ok(dir, &["cat-file", "-e", &format!("{rev}^{{commit}}")])
}

/// Whether `ancestor` is contained in `descendant`.
pub fn is_ancestor(dir: &Path, ancestor: &str, descendant: &str) -> bool {
    ok(dir, &["merge-base", "--is-ancestor", ancestor, descendant])
}

/// Commit subjects in `since..head`, newest first, at most `limit`.
pub fn subjects_since(dir: &Path, since: Option<&str>, head: &str, limit: usize) -> Vec<String> {
    let range;
    let mut args = vec!["log", "--format=%s"];
    let limit_arg = format!("-{limit}");
    args.push(&limit_arg);
    match since {
        Some(since) if since == head => return Vec::new(),
        Some(since) if exists(dir, since) => {
            range = format!("{since}..{head}");
            args.push(&range);
        }
        // Unknown or no previous commit: the latest history is the best summary.
        _ => args.push(head),
    }
    git(dir, &args)
        .map(|out| out.lines().map(str::to_string).collect())
        .unwrap_or_default()
}
