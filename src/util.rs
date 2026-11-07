//! Small helpers shared by every module: time, paths, text.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Result, bail};
use chrono::{DateTime, Local, SecondsFormat, Utc};

/// Current time as stored in the database: UTC, RFC 3339, second precision.
/// Every timestamp has the same shape, so they compare correctly as text.
pub fn now() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// A stored timestamp shifted by `secs` seconds (negative goes back).
pub fn later(secs: i64) -> String {
    (Utc::now() + chrono::Duration::seconds(secs)).to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// A stored timestamp for people: local time, minutes.
pub fn local(stamp: &str) -> String {
    match DateTime::parse_from_rfc3339(stamp) {
        Ok(t) => t.with_timezone(&Local).format("%Y-%m-%d %H:%M").to_string(),
        Err(_) => stamp.to_string(),
    }
}

/// Seconds since a stored timestamp, if it parses.
pub fn age_secs(stamp: &str) -> Option<i64> {
    let t = DateTime::parse_from_rfc3339(stamp).ok()?;
    Some((Utc::now() - t.with_timezone(&Utc)).num_seconds())
}

/// `~/x` and `$HOME/x` to absolute paths; everything else unchanged.
pub fn expand(path: &str) -> PathBuf {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    if let Some(home) = home {
        if path == "~" {
            return home;
        }
        if let Some(rest) = path.strip_prefix("~/") {
            return home.join(rest);
        }
        if let Some(rest) = path.strip_prefix("$HOME/") {
            return home.join(rest);
        }
    }
    PathBuf::from(path)
}

/// Canonical path when it exists, the path itself otherwise.
pub fn canon(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Escape text for Telegram's HTML parse mode.
pub fn html(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '&' => out.push_str("&amp;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(c),
        }
    }
    out
}

/// At most `max` characters of `text`, with "…" when cut. Cut before escaping, so
/// HTML tags never end up halved.
/// A warning on stderr that never panics: inside `topside serve` stderr is the
/// journal's stream, which breaks when journald restarts.
pub fn warn(msg: impl std::fmt::Display) {
    use std::io::Write;
    let _ = writeln!(std::io::stderr().lock(), "{msg}");
}

pub fn clip(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Length as Telegram counts it: UTF-16 code units (an emoji outside the basic
/// plane is two).
pub fn tg_len(text: &str) -> usize {
    text.encode_utf16().count()
}

/// Length of a message in HTML mode as Telegram limits it — "after entities
/// parsing": tags do not count, `&amp;` and the like are one character. For the
/// markup topside writes (tags and the four entities of [`html`]).
pub fn tg_visible_len(markup: &str) -> usize {
    let mut n = 0;
    let mut rest = markup;
    while let Some(c) = rest.chars().next() {
        if c == '<'
            && let Some(end) = rest.find('>')
        {
            rest = &rest[end + 1..];
            continue;
        }
        if c == '&'
            && let Some(entity) = ["&lt;", "&gt;", "&amp;", "&quot;"]
                .iter()
                .find(|e| rest.starts_with(**e))
        {
            n += 1;
            rest = &rest[entity.len()..];
            continue;
        }
        n += c.len_utf16();
        rest = &rest[c.len_utf8()..];
    }
    n
}

/// At most `max` bytes of `text`, cut on a character boundary: for file names
/// (file systems count bytes; 255 on ext4).
pub fn clip_bytes(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_string()
}

pub fn human_size(bytes: u64) -> String {
    const MB: f64 = 1024.0 * 1024.0;
    if bytes as f64 >= MB {
        format!("{:.1} MB", bytes as f64 / MB)
    } else {
        format!("{} KB", bytes.div_ceil(1024))
    }
}

/// `90`, `90s`, `30m`, `2h` → duration.
pub fn parse_duration(text: &str) -> Result<Duration> {
    let text = text.trim();
    let (number, unit) = match text.find(|c: char| !c.is_ascii_digit()) {
        Some(i) => text.split_at(i),
        None => (text, "s"),
    };
    let n: u64 = match number.parse() {
        Ok(n) => n,
        Err(_) => bail!("'{text}' is not a duration: use 90s, 30m or 2h"),
    };
    let secs = match unit {
        "s" => n,
        "m" => n * 60,
        "h" => n * 3600,
        _ => bail!("'{text}' is not a duration: use 90s, 30m or 2h"),
    };
    Ok(Duration::from_secs(secs))
}

/// What kind of media a file is, by extension: decides how Telegram shows it.
pub fn media_kind(path: &Path) -> &'static str {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "png" | "jpg" | "jpeg" | "webp" => "photo",
        "mp4" | "mov" | "m4v" | "webm" => "video",
        _ => "document",
    }
}

/// A file name safe to store: no directories, no leading dots, no control characters.
pub fn safe_name(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let cleaned: String = base
        .chars()
        .map(|c| if c.is_control() || c == ':' { '_' } else { c })
        .collect();
    let cleaned = cleaned.trim_start_matches('.').trim();
    if cleaned.is_empty() {
        "file".to_string()
    } else {
        cleaned.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn visible_length_is_after_parsing() {
        assert_eq!(tg_visible_len("<b>a&amp;b</b> &quot;🟢&quot;"), 8);
        assert_eq!(tg_visible_len(&html(&"&".repeat(1500))), 1500);
        assert_eq!(tg_visible_len("a < b"), 5, "a lone < is text");
    }

    #[test]
    fn durations() {
        assert_eq!(parse_duration("90").unwrap(), Duration::from_secs(90));
        assert_eq!(parse_duration("30m").unwrap(), Duration::from_secs(1800));
        assert_eq!(parse_duration("2h").unwrap(), Duration::from_secs(7200));
        assert!(parse_duration("soon").is_err());
        assert!(parse_duration("3d").is_err());
    }

    #[test]
    fn escaping() {
        assert_eq!(html("a<b>&\"c\""), "a&lt;b&gt;&amp;&quot;c&quot;");
    }

    #[test]
    fn clipping() {
        assert_eq!(clip("короткий", 20), "короткий");
        assert_eq!(clip("абвгдеёжзи", 5), "абвг…");
        assert_eq!(clip("абвгдеёжзи", 5).chars().count(), 5);
    }

    #[test]
    fn telegram_length_and_byte_clipping() {
        assert_eq!(tg_len("абв"), 3);
        assert_eq!(tg_len("🟢"), 2);
        let name = "名".repeat(100);
        let cut = clip_bytes(&name, 120);
        assert!(cut.len() <= 120 && name.starts_with(&cut));
    }

    #[test]
    fn names() {
        assert_eq!(safe_name("../../etc/passwd"), "passwd");
        assert_eq!(safe_name(".hidden"), "hidden");
        assert_eq!(safe_name(""), "file");
        assert_eq!(safe_name("снимок 1.png"), "снимок 1.png");
    }

    #[test]
    fn kinds() {
        assert_eq!(media_kind(Path::new("a.PNG")), "photo");
        assert_eq!(media_kind(Path::new("a.mov")), "video");
        assert_eq!(media_kind(Path::new("a.zip")), "document");
    }

    #[test]
    fn stamps_compare_as_text() {
        assert!(later(-10) < now());
        assert!(now() < later(10));
    }
}
