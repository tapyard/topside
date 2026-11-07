//! Minimal Telegram Bot API client: the dozen methods topside uses.

use std::fmt;
use std::io::Write;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::config::TelegramConfig;
use crate::util::{expand, media_kind};

/// Bot API limits (standard server).
pub const MAX_UPLOAD: u64 = 50 * 1024 * 1024;
pub const MAX_PHOTO: u64 = 10 * 1024 * 1024;
pub const MAX_DOWNLOAD: u64 = 20 * 1024 * 1024;
/// Telegram counts characters (UTF-16 units, close enough for Cyrillic and emoji in practice).
pub const MAX_TEXT: usize = 4096;

#[derive(Debug)]
pub struct TgError {
    pub code: Option<i64>,
    pub description: String,
    pub retry_after: Option<u64>,
}

impl TgError {
    /// An error that repeating the same request will not fix: a malformed or
    /// oversized message. Setup problems (bot not in the chat yet, no rights, a
    /// closed topic, a migrated group) are not permanent — the operator fixes
    /// them, and the queue waits for that instead of dropping messages.
    pub fn permanent(&self) -> bool {
        const SETUP: &[&str] = &[
            "chat not found",
            "not enough rights",
            "have no rights",
            "need administrator rights",
            "bot was kicked",
            "bot is not a member",
            "chat_write_forbidden",
            "thread not found",
            "topic_closed",
            "topic closed",
            "topic_deleted",
            "upgraded to a supergroup",
            "peer_id_invalid",
        ];
        let d = self.description.to_ascii_lowercase();
        matches!(self.code, Some(400) | Some(413)) && !SETUP.iter().any(|s| d.contains(s))
    }

    /// The request was refused because of a button (e.g. a launch link Telegram does not accept).
    pub fn about_buttons(&self) -> bool {
        let d = self.description.to_ascii_lowercase();
        d.contains("button") || d.contains("reply markup") || d.contains("inline keyboard")
    }
}

/// The request never got an answer from Telegram: network, proxy, timeout.
#[derive(Debug)]
pub struct NetError(pub String);

impl fmt::Display for NetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for NetError {}

impl fmt::Display for TgError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "telegram: {}", self.description)
    }
}

impl std::error::Error for TgError {}

pub struct Bot {
    agent: ureq::Agent,
    token: String,
    base: String,
    file_base: String,
}

impl Bot {
    pub fn from_config(cfg: &TelegramConfig) -> Result<Bot> {
        let path = expand(&cfg.token_file);
        let token = std::fs::read_to_string(&path)
            .with_context(|| format!("cannot read the bot token from {}", path.display()))?
            .trim()
            .to_string();
        if token.is_empty() || !token.contains(':') {
            bail!("{} does not look like a bot token", path.display());
        }
        let proxy = match cfg
            .proxy
            .as_deref()
            .map(str::trim)
            .filter(|p| !p.is_empty())
        {
            Some(url) => Some(
                ureq::Proxy::new(url).with_context(|| format!("invalid telegram.proxy '{url}'"))?,
            ),
            None => None,
        };
        Ok(Bot::new(&token, cfg.api_base.as_deref(), proxy))
    }

    pub fn new(token: &str, api_base: Option<&str>, proxy: Option<ureq::Proxy>) -> Bot {
        let root = api_base
            .unwrap_or("https://api.telegram.org")
            .trim_end_matches('/');
        let config = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(120)))
            .timeout_connect(Some(Duration::from_secs(20)))
            .http_status_as_error(false)
            .proxy(proxy)
            .build();
        Bot {
            agent: config.into(),
            token: token.to_string(),
            base: format!("{root}/bot{token}"),
            file_base: format!("{root}/file/bot{token}"),
        }
    }

    /// Errors never carry the token: ureq may print the URL.
    fn redact(&self, text: impl fmt::Display) -> String {
        text.to_string().replace(&self.token, "<token>")
    }

    fn result(&self, method: &str, mut resp: ureq::http::Response<ureq::Body>) -> Result<Value> {
        let body: Value = resp.body_mut().read_json().map_err(|e| {
            NetError(format!(
                "telegram {method}: unreadable response: {}",
                self.redact(e)
            ))
        })?;
        if body["ok"].as_bool() == Some(true) {
            return Ok(body["result"].clone());
        }
        Err(TgError {
            code: body["error_code"].as_i64(),
            description: format!(
                "{method}: {}",
                body["description"].as_str().unwrap_or("unknown error")
            ),
            retry_after: body["parameters"]["retry_after"].as_u64(),
        }
        .into())
    }

    pub fn call(&self, method: &str, params: &Value) -> Result<Value> {
        let resp = self
            .agent
            .post(&format!("{}/{method}", self.base))
            .send_json(params)
            .map_err(|e| NetError(format!("telegram {method}: {}", self.redact(e))))?;
        self.result(method, resp)
    }

    fn multipart(
        &self,
        method: &str,
        fields: &[(&str, String)],
        file_field: &str,
        file_name: &str,
        data: &[u8],
    ) -> Result<Value> {
        let boundary = format!(
            "topside-{:x}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        );
        let mut body = Vec::with_capacity(data.len() + 1024);
        for (name, value) in fields {
            write!(
                body,
                "--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n"
            )?;
        }
        let file_name = file_name.replace(['"', '\r', '\n'], "_");
        write!(
            body,
            "--{boundary}\r\nContent-Disposition: form-data; name=\"{file_field}\"; \
             filename=\"{file_name}\"\r\nContent-Type: application/octet-stream\r\n\r\n"
        )?;
        body.extend_from_slice(data);
        write!(body, "\r\n--{boundary}--\r\n")?;
        let resp = self
            .agent
            .post(&format!("{}/{method}", self.base))
            // Up to 50 MB through a slow proxy: the default two minutes are not enough.
            .config()
            .timeout_global(Some(Duration::from_secs(900)))
            .build()
            .header(
                "Content-Type",
                &format!("multipart/form-data; boundary={boundary}"),
            )
            .send(&body[..])
            .map_err(|e| NetError(format!("telegram {method}: {}", self.redact(e))))?;
        self.result(method, resp)
    }

    pub fn get_me(&self) -> Result<Value> {
        self.call("getMe", &json!({}))
    }

    pub fn send_message(
        &self,
        chat: i64,
        topic: Option<i64>,
        html: &str,
        reply_to: Option<i64>,
        keyboard: Option<Value>,
    ) -> Result<i64> {
        let visible = crate::util::tg_visible_len(html);
        if visible > MAX_TEXT {
            // A local check: sending would fail the same way every time.
            return Err(TgError {
                code: Some(400),
                description: format!("sendMessage: message is too long ({visible} characters)"),
                retry_after: None,
            }
            .into());
        }
        let mut params = json!({
            "chat_id": chat,
            "text": html,
            "parse_mode": "HTML",
            "link_preview_options": {"is_disabled": true},
        });
        if let Some(topic) = topic {
            params["message_thread_id"] = json!(topic);
        }
        if let Some(reply) = reply_to {
            params["reply_parameters"] =
                json!({"message_id": reply, "allow_sending_without_reply": true});
        }
        if let Some(keyboard) = keyboard {
            params["reply_markup"] = keyboard;
        }
        let msg = self.call("sendMessage", &params)?;
        msg["message_id"]
            .as_i64()
            .context("telegram sendMessage: no message_id")
    }

    /// Send a file as a photo, a video or a document, by its extension and size.
    pub fn send_file(
        &self,
        chat: i64,
        topic: Option<i64>,
        path: &Path,
        caption: Option<&str>,
        reply_to: Option<i64>,
    ) -> Result<i64> {
        let size = std::fs::metadata(path)
            .with_context(|| format!("cannot read {}", path.display()))?
            .len();
        if size > MAX_UPLOAD {
            bail!(
                "{} is larger than the bot upload limit (50 MB)",
                path.display()
            );
        }
        let (method, field) = match media_kind(path) {
            "photo" if size <= MAX_PHOTO => ("sendPhoto", "photo"),
            "video" => ("sendVideo", "video"),
            _ => ("sendDocument", "document"),
        };
        let data =
            std::fs::read(path).with_context(|| format!("cannot read {}", path.display()))?;
        let mut fields = vec![("chat_id", chat.to_string())];
        if let Some(topic) = topic {
            fields.push(("message_thread_id", topic.to_string()));
        }
        if let Some(reply) = reply_to {
            fields.push((
                "reply_parameters",
                json!({"message_id": reply, "allow_sending_without_reply": true}).to_string(),
            ));
        }
        if let Some(caption) = caption {
            fields.push(("caption", caption.to_string()));
            fields.push(("parse_mode", "HTML".to_string()));
        }
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");
        let msg = self.multipart(method, &fields, field, name, &data)?;
        msg["message_id"]
            .as_i64()
            .with_context(|| format!("telegram {method}: no message_id"))
    }

    pub fn remove_keyboard(&self, chat: i64, message: i64) -> Result<()> {
        match self.call(
            "editMessageReplyMarkup",
            &json!({"chat_id": chat, "message_id": message, "reply_markup": {"inline_keyboard": []}}),
        ) {
            Ok(_) => Ok(()),
            // Already without buttons: nothing to do.
            Err(e) if e.to_string().contains("message is not modified") => Ok(()),
            Err(e) => Err(e),
        }
    }

    pub fn react(&self, chat: i64, message: i64, emoji: &str) -> Result<()> {
        self.call(
            "setMessageReaction",
            &json!({"chat_id": chat, "message_id": message,
                    "reaction": [{"type": "emoji", "emoji": emoji}]}),
        )?;
        Ok(())
    }

    pub fn answer_callback(&self, id: &str, text: &str, alert: bool) -> Result<()> {
        self.call(
            "answerCallbackQuery",
            &json!({"callback_query_id": id, "text": text, "show_alert": alert}),
        )?;
        Ok(())
    }

    pub fn set_commands(&self, commands: &[(&str, &str)]) -> Result<()> {
        let list: Vec<Value> = commands
            .iter()
            .map(|(c, d)| json!({"command": c, "description": d}))
            .collect();
        self.call("setMyCommands", &json!({"commands": list}))?;
        Ok(())
    }

    pub fn get_updates(&self, offset: i64, timeout_secs: u64) -> Result<Vec<Value>> {
        let result = self.call(
            "getUpdates",
            &json!({"offset": offset, "timeout": timeout_secs,
                    "allowed_updates": ["message", "edited_message", "callback_query", "my_chat_member"]}),
        )?;
        Ok(result.as_array().cloned().unwrap_or_default())
    }

    /// Download a file the operator sent. The standard Bot API serves up to 20 MB.
    pub fn download(&self, file_id: &str, dest: &Path) -> Result<u64> {
        let file = self.call("getFile", &json!({"file_id": file_id}))?;
        let remote = file["file_path"]
            .as_str()
            .context("telegram getFile: no file_path (file too large?)")?;
        let resp = self
            .agent
            .get(&format!("{}/{remote}", self.file_base))
            // Up to 20 MB through a slow proxy: the default two minutes are not enough.
            .config()
            .timeout_global(Some(Duration::from_secs(900)))
            .build()
            .call()
            .map_err(|e| NetError(format!("telegram download: {}", self.redact(e))))?;
        if !resp.status().is_success() {
            bail!("telegram download: HTTP {}", resp.status());
        }
        let tmp = dest.with_extension("part");
        let mut out = std::fs::File::create(&tmp)
            .with_context(|| format!("cannot create {}", tmp.display()))?;
        let mut reader = resp.into_body().into_reader();
        let n = std::io::copy(&mut reader, &mut out)
            .map_err(|e| NetError(format!("telegram download: {}", self.redact(e))))?;
        drop(out);
        std::fs::rename(&tmp, dest)?;
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permanent_errors() {
        let e = |code, d: &str| TgError {
            code: Some(code),
            description: d.into(),
            retry_after: None,
        };
        assert!(e(400, "sendMessage: Bad Request: message is too long").permanent());
        assert!(e(400, "sendMessage: Bad Request: can't parse entities").permanent());
        assert!(!e(400, "sendMessage: Bad Request: chat not found").permanent());
        assert!(!e(400, "sendMessage: Bad Request: message thread not found").permanent());
        assert!(!e(429, "Too Many Requests").permanent());
        assert!(!e(403, "Forbidden: bot was kicked").permanent());
        assert!(!e(400, "sendMessage: Bad Request: TOPIC_CLOSED").permanent());
        assert!(
            !e(
                400,
                "sendMessage: Bad Request: group chat was upgraded to a supergroup chat"
            )
            .permanent()
        );
        assert!(e(400, "sendMessage: Bad Request: BUTTON_URL_INVALID").about_buttons());
    }

    #[test]
    fn redacts_token() {
        let bot = Bot::new("123:SECRET", None, None);
        assert_eq!(
            bot.redact("https://api.telegram.org/bot123:SECRET/getMe"),
            "https://api.telegram.org/bot<token>/getMe"
        );
    }
}
