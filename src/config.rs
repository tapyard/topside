//! Configuration file and on-disk layout.
//!
//! One TOML file per machine (`~/.config/topside/config.toml`). It names the
//! projects, the Syncthing folder for builds and the Telegram chat. Secrets are
//! not in it: the bot token lives in its own file, referenced by path.

use std::collections::BTreeMap;
use std::env;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::git;
use crate::util::{canon, expand};

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Folder shared by Syncthing; each project gets `<builds_root>/<project>/`.
    pub builds_root: Option<String>,
    /// Builds kept per project and platform.
    #[serde(default = "default_keep")]
    pub keep: usize,
    /// The `t3` CLI used to wake agent threads (default: `t3` in PATH).
    pub t3_cli: Option<String>,
    /// `--base-dir` for the `t3` CLI, when the server does not use the default.
    pub t3_base_dir: Option<String>,
    pub telegram: Option<TelegramConfig>,
    /// Start builds on the operator's Mac from a Telegram button (see `launcher.rs`).
    pub launcher: Option<LauncherConfig>,
    #[serde(default)]
    pub projects: BTreeMap<String, ProjectConfig>,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct LauncherConfig {
    /// `http://127.0.0.1:<port>`: the Mac listens here, buttons on the build machine point here.
    pub url: String,
    /// Secret shared by the build machine and the Mac, one line, 32+ characters.
    pub secret_file: String,
}

fn default_keep() -> usize {
    5
}

#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct TelegramConfig {
    /// File holding the bot token, one line.
    pub token_file: String,
    /// The group (supergroup with topics) where gates are posted.
    pub chat_id: i64,
    /// Telegram user ids allowed to answer gates. Messages from anyone else are ignored.
    #[serde(default)]
    pub allowed_users: Vec<i64>,
    /// Bot API server; default `https://api.telegram.org`.
    pub api_base: Option<String>,
    /// Proxy for Telegram only, e.g. `socks5h://127.0.0.1:1080` (the `h`: the proxy
    /// resolves names — needed where local DNS lies about Telegram) or `http://…`.
    pub proxy: Option<String>,
    /// Labels of the buttons the operator presses, e.g. in their own language.
    #[serde(default)]
    pub buttons: Buttons,
}

/// Button labels. The gate message names the buttons by these labels too.
#[derive(Debug, Deserialize, Clone, PartialEq)]
#[serde(deny_unknown_fields, default)]
pub struct Buttons {
    /// Sends the operator's replies to the agent as the answer.
    pub send: String,
    /// Accepts the work; anything written goes to the agent as remarks.
    pub accept: String,
    /// Starts a macOS build on the operator's Mac (needs `[launcher]`).
    pub launch: String,
}

impl Default for Buttons {
    fn default() -> Self {
        Buttons {
            send: "📨 Send to agent".into(),
            accept: "✅ Accepted".into(),
            launch: "▶️ Run on Mac".into(),
        }
    }
}

/// Longest button label: Telegram cuts longer ones on a phone screen.
pub const MAX_BUTTON: usize = 40;

impl Buttons {
    fn check(&self) -> Result<()> {
        for (key, label) in [
            ("send", &self.send),
            ("accept", &self.accept),
            ("launch", &self.launch),
        ] {
            let len = label.trim().chars().count();
            if len == 0 || len > MAX_BUTTON {
                bail!("telegram.buttons.{key} must be 1 to {MAX_BUTTON} characters");
            }
        }
        if self.send.trim() == self.accept.trim() {
            bail!("telegram.buttons.send and telegram.buttons.accept must differ");
        }
        Ok(())
    }
}

#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct ProjectConfig {
    /// Main checkout of the project. Worktrees of it resolve to the project too.
    pub repo: String,
    /// Forum topic (`message_thread_id`) of the project in the Telegram group.
    pub topic: Option<i64>,
    /// Human name used in messages; the project key by default.
    pub title: Option<String>,
}

pub struct Paths {
    pub config_file: PathBuf,
    pub data_dir: PathBuf,
    pub cache_dir: PathBuf,
}

impl Paths {
    pub fn detect() -> Result<Paths> {
        if let Some(home) = env::var_os("TOPSIDE_HOME").filter(|v| !v.is_empty()) {
            let home = PathBuf::from(home);
            return Ok(Paths {
                config_file: home.join("config.toml"),
                data_dir: home.join("data"),
                cache_dir: home.join("cache"),
            });
        }
        let home = PathBuf::from(env::var_os("HOME").context("HOME is not set")?);
        let xdg = |var: &str, default: &str| {
            env::var_os(var)
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(default))
        };
        let config_file = env::var_os("TOPSIDE_CONFIG")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| xdg("XDG_CONFIG_HOME", ".config").join("topside/config.toml"));
        Ok(Paths {
            config_file,
            data_dir: xdg("XDG_DATA_HOME", ".local/share").join("topside"),
            cache_dir: xdg("XDG_CACHE_HOME", ".cache").join("topside"),
        })
    }

    pub fn db(&self) -> PathBuf {
        self.data_dir.join("topside.db")
    }

    pub fn gate_dir(&self, id: i64) -> PathBuf {
        self.data_dir.join("gates").join(id.to_string())
    }

    pub fn tmp_dir(&self) -> PathBuf {
        self.data_dir.join("tmp")
    }
}

pub struct Ctx {
    pub cfg: Config,
    pub paths: Paths,
}

impl Ctx {
    pub fn load() -> Result<Ctx> {
        let paths = Paths::detect()?;
        let cfg = if paths.config_file.exists() {
            let text = std::fs::read_to_string(&paths.config_file)
                .with_context(|| format!("cannot read {}", paths.config_file.display()))?;
            toml::from_str(&text)
                .with_context(|| format!("invalid config {}", paths.config_file.display()))?
        } else {
            Config {
                keep: default_keep(),
                ..Config::default()
            }
        };
        if cfg.keep == 0 {
            bail!("keep must be at least 1 ({})", paths.config_file.display());
        }
        if let Some(tg) = &cfg.telegram {
            tg.buttons
                .check()
                .with_context(|| format!("invalid config {}", paths.config_file.display()))?;
        }
        for name in cfg.projects.keys() {
            if !valid_name(name) {
                bail!("project name '{name}' must be lowercase letters, digits and dashes");
            }
        }
        Ok(Ctx { cfg, paths })
    }

    pub fn builds_root(&self) -> Result<PathBuf> {
        match &self.cfg.builds_root {
            Some(root) => Ok(expand(root)),
            None => bail!(
                "builds_root is not set in {}",
                self.paths.config_file.display()
            ),
        }
    }

    pub fn project(&self, name: &str) -> Result<&ProjectConfig> {
        self.cfg.projects.get(name).with_context(|| {
            format!(
                "unknown project '{name}'; configured: {}",
                self.project_names()
            )
        })
    }

    pub fn project_names(&self) -> String {
        if self.cfg.projects.is_empty() {
            format!(
                "none (add [projects.<name>] to {})",
                self.paths.config_file.display()
            )
        } else {
            self.cfg
                .projects
                .keys()
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        }
    }

    pub fn title(&self, name: &str) -> String {
        self.cfg
            .projects
            .get(name)
            .and_then(|p| p.title.clone())
            .unwrap_or_else(|| name.to_string())
    }

    pub fn repo(&self, name: &str) -> Result<PathBuf> {
        Ok(expand(&self.project(name)?.repo))
    }

    /// The checkout the work in `dir` is in: `dir`'s own checkout when it is the
    /// project's repository or one of its worktrees, the repository itself when
    /// `dir` is in no checkout. A checkout of something else — another clone of
    /// the project included — is refused: its commits are not the repository's,
    /// and taking the repository's HEAD instead would describe the wrong work.
    pub fn checkout(&self, project: &str, dir: &Path) -> Result<PathBuf> {
        let repo = self.repo(project)?;
        let Some(top) = git::toplevel(dir) else {
            return Ok(repo);
        };
        if git::main_root(&top).map(|r| canon(&r)) == Some(canon(&repo)) {
            return Ok(top);
        }
        bail!(
            "{} is a checkout of something else, not of {project} ({}) or its worktree; \
             run from the project's checkout",
            top.display(),
            repo.display()
        )
    }

    /// The project: named explicitly, or the one whose repository holds `dir`
    /// (any worktree of it counts).
    pub fn resolve_project(&self, explicit: Option<&str>, dir: &Path) -> Result<String> {
        if let Some(name) = explicit {
            self.project(name)?;
            return Ok(name.to_string());
        }
        let Some(root) = git::main_root(dir) else {
            bail!(
                "{} is not inside a git checkout; pass --project ({})",
                dir.display(),
                self.project_names()
            );
        };
        let root = canon(&root);
        for (name, project) in &self.cfg.projects {
            if canon(&expand(&project.repo)) == root {
                return Ok(name.clone());
            }
        }
        bail!(
            "{} belongs to no configured project; pass --project ({})",
            root.display(),
            self.project_names()
        )
    }

    pub fn telegram(&self) -> Option<&TelegramConfig> {
        self.cfg.telegram.as_ref()
    }

    /// The configured button labels, or the defaults without `[telegram]`.
    pub fn buttons(&self) -> Buttons {
        self.telegram()
            .map(|t| t.buttons.clone())
            .unwrap_or_default()
    }

    pub fn topic(&self, project: &str) -> Option<i64> {
        self.cfg.projects.get(project).and_then(|p| p.topic)
    }

    /// Project whose topic is `topic`.
    pub fn project_by_topic(&self, topic: Option<i64>) -> Option<String> {
        let topic = topic?;
        self.cfg
            .projects
            .iter()
            .find(|(_, p)| p.topic == Some(topic))
            .map(|(name, _)| name.clone())
    }

    /// `t.me` link to the project's topic, or to the group without a topic.
    pub fn topic_url(&self, project: &str) -> Option<String> {
        let tg = self.telegram()?;
        let chat = internal_chat_id(tg.chat_id)?;
        Some(match self.topic(project) {
            Some(topic) => format!("https://t.me/c/{chat}/{topic}"),
            None => format!("https://t.me/c/{chat}"),
        })
    }

    /// `t.me` link to one message of the project's topic.
    pub fn message_url(&self, project: &str, message_id: i64) -> Option<String> {
        let tg = self.telegram()?;
        let chat = internal_chat_id(tg.chat_id)?;
        Some(match self.topic(project) {
            Some(topic) => format!("https://t.me/c/{chat}/{topic}/{message_id}"),
            None => format!("https://t.me/c/{chat}/{message_id}"),
        })
    }
}

/// `-1001234567890` → `1234567890`, the id `t.me/c/` links use.
fn internal_chat_id(chat_id: i64) -> Option<String> {
    chat_id.to_string().strip_prefix("-100").map(str::to_string)
}

pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_full_config() {
        let cfg: Config = toml::from_str(
            r#"
            builds_root = "~/builds"
            keep = 3
            t3_cli = "~/t3"
            [telegram]
            token_file = "~/.config/topside/telegram-token"
            chat_id = -1001234567890
            allowed_users = [42]
            proxy = "socks5h://127.0.0.1:10818"
            [projects.my-game]
            repo = "~/dev/my-game"
            topic = 7
            "#,
        )
        .unwrap();
        assert_eq!(cfg.keep, 3);
        assert_eq!(cfg.projects["my-game"].topic, Some(7));
        assert_eq!(cfg.telegram.unwrap().allowed_users, vec![42]);
    }

    #[test]
    fn button_labels_default_to_english_and_can_be_set_partly() {
        let tg = |extra: &str| {
            toml::from_str::<Config>(&format!(
                "[telegram]\ntoken_file = \"t\"\nchat_id = -1\n{extra}"
            ))
            .unwrap()
            .telegram
            .unwrap()
            .buttons
        };
        assert_eq!(tg(""), Buttons::default());
        let own =
            tg("[telegram.buttons]\nsend = \"📨 Отправить агенту\"\naccept = \"✅ Принято\"\n");
        assert_eq!(own.send, "📨 Отправить агенту");
        assert_eq!(own.accept, "✅ Принято");
        assert_eq!(own.launch, Buttons::default().launch);
        assert!(own.check().is_ok());
        assert!(
            toml::from_str::<Config>(
                "[telegram]\ntoken_file = \"t\"\nchat_id = -1\n[telegram.buttons]\nsnd = \"x\"\n"
            )
            .is_err(),
            "a misspelt label is an error, not a silent default"
        );
    }

    #[test]
    fn button_labels_are_checked() {
        let with = |send: &str, accept: &str| Buttons {
            send: send.into(),
            accept: accept.into(),
            ..Buttons::default()
        };
        assert!(with(" ", "ok").check().is_err());
        assert!(with(&"x".repeat(MAX_BUTTON + 1), "ok").check().is_err());
        assert!(with(&"я".repeat(MAX_BUTTON), "ok").check().is_ok());
        assert!(with("Same", "Same").check().is_err());
    }

    #[test]
    fn rejects_unknown_keys() {
        assert!(toml::from_str::<Config>("bulds_root = \"x\"").is_err());
    }

    #[test]
    fn chat_links() {
        assert_eq!(
            internal_chat_id(-1001234567890).as_deref(),
            Some("1234567890")
        );
        assert_eq!(internal_chat_id(12345), None);
    }

    #[test]
    fn names() {
        assert!(valid_name("my-game"));
        assert!(!valid_name("FF Game"));
        assert!(!valid_name(""));
    }
}
