//! Setup aids shown by `topside status`: where the bot was last added or removed, and where
//! the operator last sent `/gates`. The service stores what Telegram reported (chat, topic,
//! names). The verdict (is this the configured chat, which project has this topic) is
//! worked out from the config each time the note is shown. A note saved before the config
//! was fixed then reads correctly without a new command.

use serde_json::{Value, json};

use crate::config::Ctx;
use crate::util::local;

pub const SEEN_KEY: &str = "tg_seen";
pub const MEMBERSHIP_KEY: &str = "tg_membership";

#[derive(Clone, Copy)]
pub enum Kind {
    Seen,
    Membership,
}

/// What to store for the operator's `/gates` message.
pub fn seen(at: &str, msg: &Value) -> Value {
    json!({
        "at": at,
        "chat": msg["chat"]["id"].as_i64().unwrap_or(0),
        "title": msg["chat"]["title"].as_str().unwrap_or(""),
        "topic": msg["message_thread_id"].as_i64(),
        // Telegram names the topic in the creation message it attaches: map by name, not order.
        "topic_name": msg["reply_to_message"]["forum_topic_created"]["name"].as_str(),
    })
}

/// What to store for a `my_chat_member` update.
pub fn membership(at: &str, m: &Value) -> Value {
    let chat = &m["chat"];
    json!({
        "at": at,
        "chat": chat["id"].as_i64().unwrap_or(0),
        "title": chat["title"].as_str().unwrap_or(""),
        "type": chat["type"].as_str().unwrap_or("?"),
        "old": m["old_chat_member"]["status"].as_str().unwrap_or("?"),
        "new": m["new_chat_member"]["status"].as_str().unwrap_or("?"),
    })
}

/// The note without its time, judged against the config as it is now.
pub fn text(ctx: &Ctx, kind: Kind, n: &Value) -> String {
    let chat = n["chat"].as_i64().unwrap_or(0);
    let s = |key: &str| n[key].as_str().unwrap_or("").to_string();
    match kind {
        Kind::Seen => {
            let topic = n["topic"].as_i64();
            format!(
                "chat {chat} «{}», topic {}{}{}",
                s("title"),
                topic.map_or_else(|| "none".into(), |t| t.to_string()),
                n["topic_name"]
                    .as_str()
                    .map(|name| format!(" «{name}»"))
                    .unwrap_or_default(),
                match ctx.telegram() {
                    None => " — telegram is not configured".to_string(),
                    Some(tg) if tg.chat_id != chat => " — not the configured chat_id".to_string(),
                    Some(_) => match ctx.project_by_topic(topic) {
                        Some(p) => format!(" = {p}"),
                        None => " — no project has this topic".to_string(),
                    },
                }
            )
        }
        Kind::Membership => format!(
            "chat {chat} «{}» ({}): {} → {}{}",
            s("title"),
            s("type"),
            s("old"),
            s("new"),
            match ctx.telegram() {
                None => " — telegram is not configured",
                Some(tg) if tg.chat_id != chat => " — not the configured chat_id",
                Some(_) => "",
            }
        ),
    }
}

/// A stored note as `topside status` prints it: local time, then the note.
pub fn shown(ctx: &Ctx, kind: Kind, raw: &str, local_time: bool) -> String {
    let Some(n) = parse(kind, raw) else {
        return raw.to_string();
    };
    let at = n["at"].as_str().unwrap_or("");
    let at = if local_time {
        local(at)
    } else {
        at.to_string()
    };
    format!("{at} {}", text(ctx, kind, &n))
}

/// The stored form; older versions saved the finished line ("<time> <note with verdict>"),
/// which is read back into the same fields so the verdict can be redone.
fn parse(kind: Kind, raw: &str) -> Option<Value> {
    if let Ok(v) = serde_json::from_str::<Value>(raw)
        && v.is_object()
    {
        return Some(v);
    }
    let (at, note) = raw.split_once(' ')?;
    let rest = note.strip_prefix("chat ")?;
    let (chat, rest) = rest.split_once(' ')?;
    let chat: i64 = chat.parse().ok()?;
    let rest = rest.strip_prefix('«')?;
    match kind {
        Kind::Seen => {
            let rest = [
                " — not the configured chat_id",
                " — no project has this topic",
            ]
            .iter()
            .find_map(|v| rest.strip_suffix(v))
            .or_else(|| rest.rfind(" = ").map(|i| &rest[..i]))?;
            let (title, rest) = rest.split_once("», topic ")?;
            let (topic, name) = match rest.split_once(' ') {
                Some((t, name)) => (t, Some(name.strip_prefix('«')?.strip_suffix('»')?)),
                None => (rest, None),
            };
            let topic: Option<i64> = match topic {
                "none" => None,
                t => Some(t.parse().ok()?),
            };
            Some(
                json!({"at": at, "chat": chat, "title": title, "topic": topic, "topic_name": name}),
            )
        }
        Kind::Membership => {
            let rest = rest
                .strip_suffix(" — not the configured chat_id")
                .unwrap_or(rest);
            let (title, rest) = rest.rsplit_once("» (")?;
            let (kind, rest) = rest.split_once("): ")?;
            let (old, new) = rest.split_once(" → ")?;
            Some(
                json!({"at": at, "chat": chat, "title": title, "type": kind, "old": old, "new": new}),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, Paths, ProjectConfig, TelegramConfig};

    fn ctx(chat_id: i64) -> Ctx {
        let mut cfg = Config {
            telegram: Some(TelegramConfig {
                token_file: String::new(),
                chat_id,
                allowed_users: vec![],
                api_base: None,
                proxy: None,
                buttons: Default::default(),
            }),
            ..Config::default()
        };
        cfg.projects.insert(
            "my-game".into(),
            ProjectConfig {
                repo: "~/dev/my-game".into(),
                topic: Some(3),
                title: None,
            },
        );
        Ctx {
            cfg,
            paths: Paths {
                config_file: "/nonexistent/config.toml".into(),
                data_dir: "/nonexistent/data".into(),
                cache_dir: "/nonexistent/cache".into(),
            },
        }
    }

    const OLD_SEEN: &str =
        "2026-11-07T10:40:16Z chat -1001234567890 «topside», topic 3 — not the configured chat_id";

    #[test]
    fn a_note_saved_before_the_config_was_fixed_reads_against_the_config_now() {
        let fixed = ctx(-1001234567890);
        assert_eq!(
            shown(&fixed, Kind::Seen, OLD_SEEN, false),
            "2026-11-07T10:40:16Z chat -1001234567890 «topside», topic 3 = my-game"
        );
        let other = ctx(-100111);
        assert!(
            shown(&other, Kind::Seen, OLD_SEEN, false)
                .ends_with("topic 3 — not the configured chat_id")
        );
    }

    #[test]
    fn stored_notes_are_judged_when_shown() {
        let msg = json!({
            "chat": {"id": -100222, "title": "Games"},
            "message_thread_id": 7,
            "reply_to_message": {"forum_topic_created": {"name": "Arcade"}},
        });
        let raw = seen("2026-11-07T10:00:00Z", &msg).to_string();
        assert_eq!(
            shown(&ctx(-100222), Kind::Seen, &raw, false),
            "2026-11-07T10:00:00Z chat -100222 «Games», topic 7 «Arcade» — no project has this topic"
        );
        assert!(
            shown(&ctx(-100333), Kind::Seen, &raw, false)
                .ends_with(" — not the configured chat_id")
        );

        let m = json!({
            "chat": {"id": -100222, "title": "Games", "type": "supergroup"},
            "old_chat_member": {"status": "left"},
            "new_chat_member": {"status": "administrator"},
        });
        let raw = membership("2026-11-07T10:00:00Z", &m).to_string();
        assert_eq!(
            shown(&ctx(-100222), Kind::Membership, &raw, false),
            "2026-11-07T10:00:00Z chat -100222 «Games» (supergroup): left → administrator"
        );
        assert!(
            shown(&ctx(-1), Kind::Membership, &raw, false)
                .ends_with(" — not the configured chat_id")
        );
    }

    #[test]
    fn old_lines_of_every_shape_are_read_back() {
        let c = ctx(-100222);
        for (old, now) in [
            (
                "T chat -100222 «a», topic none — no project has this topic",
                "T chat -100222 «a», topic none — no project has this topic",
            ),
            (
                "T chat -100222 «a», topic 3 «General» = other-game",
                "T chat -100222 «a», topic 3 «General» = my-game",
            ),
            (
                "T chat -100222 «a» (supergroup): member → administrator — not the configured chat_id",
                "T chat -100222 «a» (supergroup): member → administrator",
            ),
        ] {
            let kind = if old.contains(" → ") {
                Kind::Membership
            } else {
                Kind::Seen
            };
            assert_eq!(shown(&c, kind, old, false), now, "{old}");
        }
        // What cannot be read back is shown as saved.
        assert_eq!(
            shown(&c, Kind::Seen, "T something else", false),
            "T something else"
        );
    }
}
