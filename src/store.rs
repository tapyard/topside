//! SQLite store shared by the CLI and the service.
//!
//! The CLI writes records and queues outgoing events (`outbox`); the service
//! sends them. So an agent's command never waits for the network, and a
//! message that failed to send is retried instead of lost.

use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::Serialize;
use serde_json::Value;

use crate::config::Paths;
use crate::util::now;

const SCHEMA: &str = r#"
CREATE TABLE meta(key TEXT PRIMARY KEY, value TEXT NOT NULL);

CREATE TABLE gates(
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    project TEXT NOT NULL,
    title TEXT NOT NULL,
    body TEXT NOT NULL,
    status TEXT NOT NULL,            -- open | answered | withdrawn
    verdict TEXT,                    -- accepted | accepted_with_notes | answered
    thread_id TEXT,                  -- T3 thread woken by the answer
    commit_sha TEXT,
    branch TEXT,
    worktree TEXT,
    created_at TEXT NOT NULL,
    created_by TEXT,
    closed_at TEXT,
    close_reason TEXT,
    build_announced INTEGER NOT NULL DEFAULT 0,
    tg_anchor INTEGER,               -- first Telegram message of the gate
    tg_keyboard INTEGER,             -- the message carrying the buttons
    wake_state TEXT,                 -- pending | done | failed | none
    wake_attempts INTEGER NOT NULL DEFAULT 0,
    wake_next_at TEXT,
    wake_error TEXT,
    woke_at TEXT
);

CREATE TABLE questions(
    gate_id INTEGER NOT NULL REFERENCES gates(id),
    n INTEGER NOT NULL,
    text TEXT NOT NULL,
    added_at TEXT NOT NULL,
    PRIMARY KEY(gate_id, n)
);

CREATE TABLE files(
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    gate_id INTEGER NOT NULL REFERENCES gates(id),
    role TEXT NOT NULL,              -- agent | operator
    path TEXT NOT NULL,
    kind TEXT NOT NULL,              -- photo | video | document
    added_at TEXT NOT NULL,
    tg_message_id INTEGER
);

CREATE TABLE replies(
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    gate_id INTEGER NOT NULL REFERENCES gates(id),
    at TEXT NOT NULL,
    source TEXT NOT NULL,            -- telegram | cli
    text TEXT NOT NULL,
    file_id INTEGER REFERENCES files(id),
    tg_message_id INTEGER
);

CREATE TABLE notes(
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    gate_id INTEGER NOT NULL REFERENCES gates(id),
    at TEXT NOT NULL,
    text TEXT NOT NULL
);

CREATE TABLE publishes(
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    project TEXT NOT NULL,
    sha TEXT NOT NULL,
    tag TEXT,
    branch TEXT,
    subject TEXT NOT NULL,
    changes TEXT NOT NULL,           -- JSON array of commit subjects
    created_at TEXT NOT NULL
);

CREATE TABLE builds(
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    publish_id INTEGER REFERENCES publishes(id),
    project TEXT NOT NULL,
    platform TEXT NOT NULL,
    file TEXT NOT NULL UNIQUE,       -- relative to builds_root
    sha TEXT NOT NULL,
    tag TEXT,
    stamp TEXT NOT NULL,             -- YYYYmmdd-HHMM, from the file name
    size INTEGER NOT NULL,
    subject TEXT NOT NULL,
    created_at TEXT NOT NULL,
    pruned INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE outbox(
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    kind TEXT NOT NULL,
    payload TEXT NOT NULL,
    created_at TEXT NOT NULL,
    attempts INTEGER NOT NULL DEFAULT 0,
    next_at TEXT NOT NULL,
    done_at TEXT,
    error TEXT
);

CREATE TABLE tg_messages(
    chat_id INTEGER NOT NULL,
    message_id INTEGER NOT NULL,
    gate_id INTEGER NOT NULL REFERENCES gates(id),
    PRIMARY KEY(chat_id, message_id)
);
"#;

/// Changes after the first schema, applied in order: entry N takes the database to version N + 2.
const MIGRATIONS: &[&str] = &[
    // 2: a wake-up belongs to one generation of the gate's thread (set-thread starts a new one),
    //    and the gate remembers when it was posted (later questions and notes are sent apart).
    "ALTER TABLE gates ADD COLUMN wake_gen INTEGER NOT NULL DEFAULT 0;
     ALTER TABLE gates ADD COLUMN tg_posted_at TEXT;",
    // 3: what the gate message showed — the last question number and note id — so that
    //    later ones are sent apart and none is lost (a timestamp could not tell).
    "ALTER TABLE gates ADD COLUMN tg_shown_question INTEGER;
     ALTER TABLE gates ADD COLUMN tg_shown_note INTEGER;",
    // 4: every outbox event belongs to a lane (its project): order is kept within a
    //    lane, and one project's stuck topic no longer holds up the others.
    //    Gates posted before schema 3 get their "shown" marks from their post time
    //    (or, posted before schema 2, from what exists now: their events were sent).
    "ALTER TABLE outbox ADD COLUMN lane TEXT NOT NULL DEFAULT '';
     UPDATE outbox SET lane = COALESCE(
         (SELECT project FROM gates WHERE id = json_extract(outbox.payload, '$.gate')),
         (SELECT project FROM publishes WHERE id = json_extract(outbox.payload, '$.publish')),
         '');
     UPDATE gates SET
         tg_shown_question = COALESCE((SELECT MAX(n) FROM questions q WHERE q.gate_id = gates.id
                                       AND (gates.tg_posted_at IS NULL OR q.added_at <= gates.tg_posted_at)), 0),
         tg_shown_note = COALESCE((SELECT MAX(id) FROM notes n WHERE n.gate_id = gates.id
                                   AND (gates.tg_posted_at IS NULL OR n.at <= gates.tg_posted_at)), 0)
       WHERE tg_anchor IS NOT NULL AND tg_shown_question IS NULL;",
];

const VERSION: i64 = 1 + MIGRATIONS.len() as i64;
/// The oldest schema whose topside can still use a database of this schema. A
/// migration that only adds (a table, a column with a default) keeps it, so a
/// rollback to the previous release keeps working; raise it with a change an
/// older topside would misuse. Recorded in `meta` by the topside that migrates.
/// An older topside writes such a column's default: give every such column an
/// idempotent fix-up in [`Store::repair`], which `topside serve` runs at start.
const MIN_READER: i64 = 4;
const MIN_READER_KEY: &str = "min_reader_schema";

pub struct Store {
    pub conn: Connection,
    /// The schema checked when this connection was opened.
    schema_at_open: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Gate {
    pub id: i64,
    pub project: String,
    pub title: String,
    pub body: String,
    pub status: String,
    pub verdict: Option<String>,
    pub thread_id: Option<String>,
    pub commit_sha: Option<String>,
    pub branch: Option<String>,
    pub worktree: Option<String>,
    pub created_at: String,
    pub created_by: Option<String>,
    pub closed_at: Option<String>,
    pub close_reason: Option<String>,
    pub build_announced: bool,
    pub tg_anchor: Option<i64>,
    pub tg_keyboard: Option<i64>,
    pub wake_state: Option<String>,
    pub wake_attempts: i64,
    pub wake_error: Option<String>,
    pub woke_at: Option<String>,
    pub wake_gen: i64,
    pub tg_posted_at: Option<String>,
    pub tg_shown_question: Option<i64>,
    pub tg_shown_note: Option<i64>,
}

impl Gate {
    fn from_row(row: &Row) -> rusqlite::Result<Gate> {
        Ok(Gate {
            id: row.get("id")?,
            project: row.get("project")?,
            title: row.get("title")?,
            body: row.get("body")?,
            status: row.get("status")?,
            verdict: row.get("verdict")?,
            thread_id: row.get("thread_id")?,
            commit_sha: row.get("commit_sha")?,
            branch: row.get("branch")?,
            worktree: row.get("worktree")?,
            created_at: row.get("created_at")?,
            created_by: row.get("created_by")?,
            closed_at: row.get("closed_at")?,
            close_reason: row.get("close_reason")?,
            build_announced: row.get::<_, i64>("build_announced")? != 0,
            tg_anchor: row.get("tg_anchor")?,
            tg_keyboard: row.get("tg_keyboard")?,
            wake_state: row.get("wake_state")?,
            wake_attempts: row.get("wake_attempts")?,
            wake_error: row.get("wake_error")?,
            woke_at: row.get("woke_at")?,
            wake_gen: row.get("wake_gen")?,
            tg_posted_at: row.get("tg_posted_at")?,
            tg_shown_question: row.get("tg_shown_question")?,
            tg_shown_note: row.get("tg_shown_note")?,
        })
    }

    pub fn is_open(&self) -> bool {
        self.status == "open"
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Question {
    pub n: i64,
    pub text: String,
    pub added_at: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct FileRec {
    pub id: i64,
    pub role: String,
    pub path: String,
    pub kind: String,
    pub tg_message_id: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Reply {
    pub id: i64,
    pub at: String,
    pub source: String,
    pub text: String,
    pub file: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Note {
    pub id: i64,
    pub at: String,
    pub text: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Build {
    pub id: i64,
    pub publish_id: Option<i64>,
    pub project: String,
    pub platform: String,
    pub file: String,
    pub sha: String,
    pub tag: Option<String>,
    pub stamp: String,
    pub size: i64,
    pub subject: String,
    pub created_at: String,
}

impl Build {
    fn from_row(row: &Row) -> rusqlite::Result<Build> {
        Ok(Build {
            id: row.get("id")?,
            publish_id: row.get("publish_id")?,
            project: row.get("project")?,
            platform: row.get("platform")?,
            file: row.get("file")?,
            sha: row.get("sha")?,
            tag: row.get("tag")?,
            stamp: row.get("stamp")?,
            size: row.get("size")?,
            subject: row.get("subject")?,
            created_at: row.get("created_at")?,
        })
    }

    pub fn name(&self) -> &str {
        self.file.rsplit('/').next().unwrap_or(&self.file)
    }
}

#[derive(Debug, Clone)]
pub struct Publish {
    pub project: String,
    pub sha: String,
    pub tag: Option<String>,
    pub subject: String,
    pub changes: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct OutboxItem {
    pub id: i64,
    pub kind: String,
    pub payload: Value,
    pub attempts: i64,
    pub lane: String,
}

pub struct NewGate<'a> {
    pub project: &'a str,
    pub title: &'a str,
    pub body: &'a str,
    pub questions: &'a [String],
    pub thread_id: Option<&'a str>,
    pub commit_sha: Option<&'a str>,
    pub branch: Option<&'a str>,
    pub worktree: Option<&'a str>,
    pub created_by: Option<&'a str>,
}

pub struct NewBuild<'a> {
    pub platform: &'a str,
    pub file: &'a str,
    pub stamp: &'a str,
    pub size: i64,
}

impl Store {
    pub fn open(paths: &Paths) -> Result<Store> {
        std::fs::create_dir_all(&paths.data_dir)
            .with_context(|| format!("cannot create {}", paths.data_dir.display()))?;
        Store::open_at(&paths.db())
    }

    pub fn open_at(path: &Path) -> Result<Store> {
        let conn = Connection::open(path)
            .with_context(|| format!("cannot open database {}", path.display()))?;
        conn.busy_timeout(std::time::Duration::from_secs(10))?;
        // Switching a fresh file to WAL needs a moment of exclusive access; another
        // process opening it at the same time may hold it.
        let mut wal = conn.pragma_update(None, "journal_mode", "WAL");
        for _ in 0..20 {
            if wal.is_ok() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
            wal = conn.pragma_update(None, "journal_mode", "WAL");
        }
        wal?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        let mut version = Store::schema_of(&conn)?;
        if version < VERSION {
            version = Store::migrate(&conn)
                .with_context(|| format!("cannot set up or migrate {}", path.display()))?;
        }
        // Newer than this topside (also when a newer one migrated while we waited
        // for the lock): usable only if that topside said older ones may use it.
        if version > VERSION && !Store::readable_by_us(&conn) {
            anyhow::bail!(
                "database {} is from a newer topside (schema {version}); update topside",
                path.display()
            );
        }
        Ok(Store {
            conn,
            schema_at_open: version,
        })
    }

    fn schema_of(conn: &Connection) -> Result<i64> {
        Ok(conn.pragma_query_value(None, "user_version", |r| r.get(0))?)
    }

    /// The schema of this database (a running service checks whether a newer
    /// topside has migrated it underneath).
    pub fn schema(&self) -> Result<i64> {
        Store::schema_of(&self.conn)
    }

    /// The schema `open` checked: the baseline for that watch (read later, a
    /// migration in between would become the baseline and never be noticed).
    pub fn schema_at_open(&self) -> i64 {
        self.schema_at_open
    }

    /// The newer topside that migrated the database recorded the oldest schema
    /// that can use it; a newer database is never migrated back.
    fn readable_by_us(conn: &Connection) -> bool {
        conn.query_row(
            "SELECT value FROM meta WHERE key = ?1",
            [MIN_READER_KEY],
            |r| r.get::<_, String>(0),
        )
        .optional()
        .ok()
        .flatten()
        .and_then(|v| v.parse::<i64>().ok())
        .is_some_and(|m| m <= VERSION)
    }

    /// Create or upgrade the schema under the write lock, reading the version
    /// again inside it: two processes opening an old or new database at once
    /// must not both run the same change. The minimum reader is written only by
    /// the topside that actually migrated — never over a newer one's. Returns the
    /// schema found or reached.
    fn migrate(conn: &Connection) -> Result<i64> {
        conn.execute_batch("BEGIN IMMEDIATE")?;
        let result = (|| -> Result<i64> {
            let mut version = Store::schema_of(conn)?;
            if version >= VERSION {
                return Ok(version); // migrated by another process meanwhile
            }
            if version == 0 {
                conn.execute_batch(SCHEMA)?;
                version = 1;
            }
            while version < VERSION {
                conn.execute_batch(MIGRATIONS[(version - 1) as usize])?;
                version += 1;
            }
            conn.pragma_update(None, "user_version", version)?;
            conn.execute(
                "INSERT INTO meta(key, value) VALUES(?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![MIN_READER_KEY, MIN_READER.to_string()],
            )?;
            Ok(version)
        })();
        match result {
            Ok(version) => {
                conn.execute_batch("COMMIT")?;
                Ok(version)
            }
            Err(e) => {
                let _ = conn.execute_batch("ROLLBACK");
                Err(e)
            }
        }
    }

    // ---- meta

    pub fn meta(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0))
            .optional()?)
    }

    pub fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO meta(key, value) VALUES(?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    // ---- gates

    pub fn insert_gate(&mut self, g: &NewGate) -> Result<i64> {
        let tx = self.conn.transaction()?;
        let at = now();
        tx.execute(
            "INSERT INTO gates(project, title, body, status, thread_id, commit_sha, branch,
                               worktree, created_at, created_by)
             VALUES(?1, ?2, ?3, 'open', ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                g.project,
                g.title,
                g.body,
                g.thread_id,
                g.commit_sha,
                g.branch,
                g.worktree,
                at,
                g.created_by
            ],
        )?;
        let id = tx.last_insert_rowid();
        for (i, q) in g.questions.iter().enumerate() {
            tx.execute(
                "INSERT INTO questions(gate_id, n, text, added_at) VALUES(?1, ?2, ?3, ?4)",
                params![id, i as i64 + 1, q, at],
            )?;
        }
        tx.commit()?;
        Ok(id)
    }

    pub fn gate(&self, id: i64) -> Result<Gate> {
        self.conn
            .query_row("SELECT * FROM gates WHERE id = ?1", [id], Gate::from_row)
            .optional()?
            .with_context(|| format!("no gate #{id}"))
    }

    pub fn gates(&self, project: Option<&str>, status: Option<&str>) -> Result<Vec<Gate>> {
        let mut stmt = self.conn.prepare(
            "SELECT * FROM gates
             WHERE (?1 IS NULL OR project = ?1) AND (?2 IS NULL OR status = ?2)
             ORDER BY id",
        )?;
        let rows = stmt.query_map(params![project, status], Gate::from_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn questions(&self, gate: i64) -> Result<Vec<Question>> {
        let mut stmt = self
            .conn
            .prepare("SELECT n, text, added_at FROM questions WHERE gate_id = ?1 ORDER BY n")?;
        let rows = stmt.query_map([gate], |r| {
            Ok(Question {
                n: r.get(0)?,
                text: r.get(1)?,
                added_at: r.get(2)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn add_question(&self, gate: i64, text: &str) -> Result<i64> {
        let n: i64 = self.conn.query_row(
            "SELECT COALESCE(MAX(n), 0) + 1 FROM questions WHERE gate_id = ?1",
            [gate],
            |r| r.get(0),
        )?;
        self.conn.execute(
            "INSERT INTO questions(gate_id, n, text, added_at) VALUES(?1, ?2, ?3, ?4)",
            params![gate, n, text, now()],
        )?;
        Ok(n)
    }

    pub fn add_note(&self, gate: i64, text: &str) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO notes(gate_id, at, text) VALUES(?1, ?2, ?3)",
            params![gate, now(), text],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn notes(&self, gate: i64) -> Result<Vec<Note>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, at, text FROM notes WHERE gate_id = ?1 ORDER BY id")?;
        let rows = stmt.query_map([gate], |r| {
            Ok(Note {
                id: r.get(0)?,
                at: r.get(1)?,
                text: r.get(2)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn note(&self, id: i64) -> Result<Note> {
        Ok(self
            .conn
            .query_row("SELECT id, at, text FROM notes WHERE id = ?1", [id], |r| {
                Ok(Note {
                    id: r.get(0)?,
                    at: r.get(1)?,
                    text: r.get(2)?,
                })
            })?)
    }

    pub fn add_file(&self, gate: i64, role: &str, path: &str, kind: &str) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO files(gate_id, role, path, kind, added_at) VALUES(?1, ?2, ?3, ?4, ?5)",
            params![gate, role, path, kind, now()],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn files(&self, gate: i64, role: Option<&str>) -> Result<Vec<FileRec>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, role, path, kind, tg_message_id FROM files
             WHERE gate_id = ?1 AND (?2 IS NULL OR role = ?2) ORDER BY id",
        )?;
        let rows = stmt.query_map(params![gate, role], |r| {
            Ok(FileRec {
                id: r.get(0)?,
                role: r.get(1)?,
                path: r.get(2)?,
                kind: r.get(3)?,
                tg_message_id: r.get(4)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn set_file_message(&self, file: i64, message_id: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE files SET tg_message_id = ?2 WHERE id = ?1",
            params![file, message_id],
        )?;
        Ok(())
    }

    pub fn add_reply(
        &self,
        gate: i64,
        source: &str,
        text: &str,
        file: Option<i64>,
        tg_message_id: Option<i64>,
    ) -> Result<Option<i64>> {
        // One reply per Telegram message: an update handled again after a failure
        // must not add the same part twice.
        if let Some(message) = tg_message_id
            && let Some(id) = self.reply_of_message(gate, message)?
        {
            return Ok(Some(id));
        }
        // Only into an open gate: a part that arrives after the close (a long
        // download meanwhile) would never reach the agent.
        let added = self.conn.execute(
            "INSERT INTO replies(gate_id, at, source, text, file_id, tg_message_id)
             SELECT ?1, ?2, ?3, ?4, ?5, ?6
             WHERE EXISTS (SELECT 1 FROM gates WHERE id = ?1 AND status = 'open')",
            params![gate, now(), source, text, file, tg_message_id],
        )?;
        Ok((added == 1).then(|| self.conn.last_insert_rowid()))
    }

    pub fn reply_of_message(&self, gate: i64, message: i64) -> Result<Option<i64>> {
        Ok(self
            .conn
            .query_row(
                "SELECT id FROM replies WHERE gate_id = ?1 AND tg_message_id = ?2",
                params![gate, message],
                |r| r.get(0),
            )
            .optional()?)
    }

    pub fn replies(&self, gate: i64) -> Result<Vec<Reply>> {
        let mut stmt = self.conn.prepare(
            "SELECT r.id, r.at, r.source, r.text, f.path FROM replies r
             LEFT JOIN files f ON f.id = r.file_id
             WHERE r.gate_id = ?1 ORDER BY r.id",
        )?;
        let rows = stmt.query_map([gate], |r| {
            Ok(Reply {
                id: r.get(0)?,
                at: r.get(1)?,
                source: r.get(2)?,
                text: r.get(3)?,
                file: r.get(4)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Close an open gate with the operator's answer. False when it was not open.
    pub fn answer_gate(&self, gate: i64, verdict: &str, source: &str, wake: bool) -> Result<bool> {
        let changed = self.conn.execute(
            "UPDATE gates SET status = 'answered', verdict = ?2, closed_at = ?3,
                    close_reason = ?4, wake_state = ?5, wake_next_at = ?3
             WHERE id = ?1 AND status = 'open'",
            params![
                gate,
                verdict,
                now(),
                source,
                if wake { "pending" } else { "none" }
            ],
        )?;
        Ok(changed == 1)
    }

    pub fn withdraw_gate(&self, gate: i64, reason: &str) -> Result<bool> {
        let changed = self.conn.execute(
            "UPDATE gates SET status = 'withdrawn', closed_at = ?2, close_reason = ?3
             WHERE id = ?1 AND status = 'open'",
            params![gate, now(), reason],
        )?;
        Ok(changed == 1)
    }

    /// Point a gate at another thread (or none) and, for an answer not delivered
    /// yet, queue the delivery anew — in one step: a wake-up claimed in between
    /// would otherwise go to the new thread twice. Whether it was queued.
    pub fn set_thread_and_rewake(&self, gate: i64, thread: Option<&str>) -> Result<bool> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "UPDATE gates SET thread_id = ?2 WHERE id = ?1",
            params![gate, thread],
        )?;
        let queued = thread.is_some()
            && tx.execute(
                "UPDATE gates SET wake_state = 'pending', wake_attempts = 0, wake_error = NULL,
                        wake_next_at = ?2, wake_gen = wake_gen + 1
                 WHERE id = ?1 AND status = 'answered' AND COALESCE(wake_state, '') <> 'done'",
                params![gate, now()],
            )? == 1;
        tx.commit()?;
        Ok(queued)
    }

    /// The operator edited a message that is part of an answer.
    pub fn update_reply_text(&self, gate: i64, message: i64, text: &str) -> Result<bool> {
        Ok(self.conn.execute(
            "UPDATE replies SET text = ?3 WHERE gate_id = ?1 AND tg_message_id = ?2",
            params![gate, message, text],
        )? > 0)
    }

    pub fn set_gate_messages(
        &self,
        gate: i64,
        anchor: i64,
        keyboard: Option<i64>,
        shown_question: i64,
        shown_note: i64,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE gates SET tg_anchor = ?2, tg_keyboard = ?3, tg_posted_at = ?4,
                    tg_shown_question = ?5, tg_shown_note = ?6
             WHERE id = ?1",
            params![gate, anchor, keyboard, now(), shown_question, shown_note],
        )?;
        Ok(())
    }

    pub fn set_build_announced(&self, gate: i64) -> Result<()> {
        self.conn
            .execute("UPDATE gates SET build_announced = 1 WHERE id = ?1", [gate])?;
        Ok(())
    }

    pub fn map_message(&self, chat: i64, message: i64, gate: i64) -> Result<()> {
        self.conn.execute(
            "INSERT OR IGNORE INTO tg_messages(chat_id, message_id, gate_id) VALUES(?1, ?2, ?3)",
            params![chat, message, gate],
        )?;
        Ok(())
    }

    pub fn gate_of_message(&self, chat: i64, message: i64) -> Result<Option<i64>> {
        Ok(self
            .conn
            .query_row(
                "SELECT gate_id FROM tg_messages WHERE chat_id = ?1 AND message_id = ?2",
                params![chat, message],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// Pending wake-ups that are due, and claimed ones whose lease ran out (the
    /// service could not record their outcome): those are tried again.
    pub fn due_wakes(&self) -> Result<Vec<Gate>> {
        let mut stmt = self.conn.prepare(
            "SELECT * FROM gates WHERE wake_state IN ('pending', 'sending') AND wake_next_at <= ?1
             ORDER BY id",
        )?;
        let rows = stmt.query_map([now()], Gate::from_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Take a pending wake-up before sending it. Written first, so that a database
    /// that cannot be written never leads to the same answer being sent again and again.
    /// The claim is a lease: if nothing is recorded within it, the wake-up becomes due again.
    pub fn claim_wake(&self, gate: i64, generation: i64) -> Result<bool> {
        let changed = self.conn.execute(
            "UPDATE gates SET wake_state = 'sending', wake_attempts = wake_attempts + 1,
                    wake_next_at = ?3
             WHERE id = ?1 AND wake_gen = ?2 AND wake_state IN ('pending', 'sending')
               AND wake_next_at <= ?4",
            params![gate, generation, crate::util::later(600), now()],
        )?;
        Ok(changed == 1)
    }

    /// A wake-up interrupted by a crash may or may not have been delivered: send it again.
    pub fn release_stale_wakes(&self) -> Result<usize> {
        Ok(self.conn.execute(
            "UPDATE gates SET wake_state = 'pending', wake_next_at = ?1 WHERE wake_state = 'sending'",
            [now()],
        )?)
    }

    /// Recorded only for the generation that was sent: a thread changed meanwhile wins.
    pub fn wake_done(&self, gate: i64, generation: i64) -> Result<()> {
        self.conn.execute(
            "UPDATE gates SET wake_state = 'done', woke_at = ?3, wake_error = NULL
             WHERE id = ?1 AND wake_gen = ?2 AND wake_state = 'sending'",
            params![gate, generation, now()],
        )?;
        Ok(())
    }

    /// `next_at = None` gives up.
    pub fn wake_failed(
        &self,
        gate: i64,
        generation: i64,
        error: &str,
        next_at: Option<&str>,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE gates SET wake_error = ?3,
                    wake_state = CASE WHEN ?4 IS NULL THEN 'failed' ELSE 'pending' END,
                    wake_next_at = COALESCE(?4, wake_next_at)
             WHERE id = ?1 AND wake_gen = ?2 AND wake_state IN ('sending', 'pending')",
            params![gate, generation, error, next_at],
        )?;
        Ok(())
    }

    // ---- builds

    #[allow(clippy::too_many_arguments)]
    pub fn insert_publish(
        &mut self,
        project: &str,
        sha: &str,
        tag: Option<&str>,
        branch: Option<&str>,
        subject: &str,
        changes: &[String],
        builds: &[NewBuild],
    ) -> Result<i64> {
        let tx = self.conn.transaction()?;
        let at = now();
        tx.execute(
            "INSERT INTO publishes(project, sha, tag, branch, subject, changes, created_at)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                project,
                sha,
                tag,
                branch,
                subject,
                serde_json::to_string(changes)?,
                at
            ],
        )?;
        let id = tx.last_insert_rowid();
        // The announcement is queued in the same transaction: a recorded build is never silent.
        tx.execute(
            "INSERT INTO outbox(kind, payload, created_at, next_at, lane)
             VALUES('publish', ?1, ?2, ?2, ?3)",
            params![serde_json::json!({"publish": id}).to_string(), at, project],
        )?;
        for b in builds {
            tx.execute(
                "INSERT INTO builds(publish_id, project, platform, file, sha, tag, stamp, size,
                                    subject, created_at)
                 VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
                 ON CONFLICT(file) DO UPDATE SET publish_id = excluded.publish_id,
                    sha = excluded.sha, size = excluded.size, pruned = 0,
                    stamp = excluded.stamp, created_at = excluded.created_at",
                params![
                    id, project, b.platform, b.file, sha, tag, b.stamp, b.size, subject, at
                ],
            )?;
        }
        tx.commit()?;
        Ok(id)
    }

    /// Register a build found on disk (reindex). False when it was known.
    #[allow(clippy::too_many_arguments)]
    pub fn adopt_build(
        &self,
        project: &str,
        platform: &str,
        file: &str,
        sha: &str,
        tag: Option<&str>,
        stamp: &str,
        size: i64,
        subject: &str,
        created_at: &str,
    ) -> Result<bool> {
        let changed = self.conn.execute(
            "INSERT OR IGNORE INTO builds(project, platform, file, sha, tag, stamp, size, subject,
                                          created_at)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                project, platform, file, sha, tag, stamp, size, subject, created_at
            ],
        )?;
        Ok(changed == 1)
    }

    /// A build file name ever recorded, pruned or not: names are never reused, so a
    /// button or an unpacked copy always means one build.
    pub fn file_known(&self, file: &str) -> Result<bool> {
        Ok(self
            .conn
            .query_row("SELECT 1 FROM builds WHERE file = ?1", [file], |_| Ok(()))
            .optional()?
            .is_some())
    }

    pub fn publish(&self, id: i64) -> Result<Publish> {
        Ok(self.conn.query_row(
            "SELECT project, sha, tag, subject, changes FROM publishes WHERE id = ?1",
            [id],
            |r| {
                let changes: String = r.get(4)?;
                Ok(Publish {
                    project: r.get(0)?,
                    sha: r.get(1)?,
                    tag: r.get(2)?,
                    subject: r.get(3)?,
                    changes: serde_json::from_str(&changes).unwrap_or_default(),
                })
            },
        )?)
    }

    pub fn last_publish_sha(&self, project: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT sha FROM publishes WHERE project = ?1 ORDER BY id DESC LIMIT 1",
                [project],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// Builds still on disk, newest first.
    pub fn builds(&self, project: &str, platform: Option<&str>) -> Result<Vec<Build>> {
        let mut stmt = self.conn.prepare(
            "SELECT * FROM builds WHERE project = ?1 AND pruned = 0
               AND (?2 IS NULL OR platform = ?2)
             ORDER BY created_at DESC, id DESC",
        )?;
        let rows = stmt.query_map(params![project, platform], Build::from_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn builds_of_publish(&self, publish: i64) -> Result<Vec<Build>> {
        let mut stmt = self
            .conn
            .prepare("SELECT * FROM builds WHERE publish_id = ?1 ORDER BY platform")?;
        let rows = stmt.query_map([publish], Build::from_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn mark_pruned(&self, build: i64) -> Result<()> {
        self.conn
            .execute("UPDATE builds SET pruned = 1 WHERE id = ?1", [build])?;
        Ok(())
    }

    // ---- outbox

    pub fn enqueue(&self, kind: &str, payload: Value) -> Result<i64> {
        let at = now();
        // The lane is the event's project: order matters within a project's topic only.
        self.conn.execute(
            "INSERT INTO outbox(kind, payload, created_at, next_at, lane) VALUES(?1, ?2, ?3, ?3,
                COALESCE((SELECT project FROM gates WHERE id = json_extract(?2, '$.gate')),
                         (SELECT project FROM publishes WHERE id = json_extract(?2, '$.publish')),
                         ''))",
            params![kind, payload.to_string(), at],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Undelivered events, oldest first, each lane (project) up to its first event
    /// still waiting for a retry: within a project delivery is strictly in order (a
    /// chat never sees a build announced before its gate), and a project whose
    /// topic is stuck does not hold up the others.
    pub fn due_outbox(&self) -> Result<Vec<OutboxItem>> {
        let mut stmt = self.conn.prepare(
            "SELECT o.id, o.kind, o.payload, o.attempts, o.lane FROM outbox o
             WHERE o.done_at IS NULL
               AND NOT EXISTS (SELECT 1 FROM outbox w
                               WHERE w.done_at IS NULL AND w.lane = o.lane AND w.id <= o.id
                                 AND w.next_at > ?1)
             ORDER BY o.id LIMIT 100",
        )?;
        let rows = stmt.query_map([now()], |r| {
            let payload: String = r.get(2)?;
            Ok(OutboxItem {
                id: r.get(0)?,
                kind: r.get(1)?,
                payload: serde_json::from_str(&payload).unwrap_or(Value::Null),
                attempts: r.get(3)?,
                lane: r.get(4)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Per lane: how many events wait, and the error of the first one, if any.
    /// Fix-ups for rows an older topside wrote into a newer schema (after a
    /// rollback, or while it still ran during an upgrade): idempotent, run by
    /// `topside serve` at start. One per column that an older topside leaves at
    /// its default.
    pub fn repair(&self) -> Result<usize> {
        self.fill_lanes()
    }

    /// Events queued without a lane (by a topside from before lanes) get their project's lane.
    pub fn fill_lanes(&self) -> Result<usize> {
        Ok(self.conn.execute(
            "UPDATE outbox SET lane = COALESCE(
                 (SELECT project FROM gates WHERE id = json_extract(outbox.payload, '$.gate')),
                 (SELECT project FROM publishes WHERE id = json_extract(outbox.payload, '$.publish')),
                 '')
             WHERE done_at IS NULL AND lane = ''",
            [],
        )?)
    }

    pub fn outbox_lanes(&self) -> Result<Vec<(String, i64, Option<String>)>> {
        let mut stmt = self.conn.prepare(
            "SELECT lane, COUNT(*),
                    (SELECT error FROM outbox f WHERE f.lane = o.lane AND f.done_at IS NULL
                     ORDER BY f.id LIMIT 1)
             FROM outbox o WHERE done_at IS NULL GROUP BY lane ORDER BY lane",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn outbox_done(&self, id: i64, note: Option<&str>) -> Result<()> {
        self.conn.execute(
            "UPDATE outbox SET done_at = ?2, error = ?3 WHERE id = ?1",
            params![id, now(), note],
        )?;
        Ok(())
    }

    /// Count the attempt and push the retry time before sending: if the send
    /// succeeds and the database then fails, the event comes back only after the
    /// backoff, not every two seconds.
    pub fn outbox_claim(&self, id: i64, next_at: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE outbox SET attempts = attempts + 1, next_at = ?2 WHERE id = ?1",
            params![id, next_at],
        )?;
        Ok(())
    }

    pub fn outbox_retry(&self, id: i64, error: &str, next_at: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE outbox SET error = ?2, next_at = ?3 WHERE id = ?1",
            params![id, error, next_at],
        )?;
        Ok(())
    }

    pub fn outbox_pending(&self) -> Result<(i64, Option<String>)> {
        Ok(self.conn.query_row(
            "SELECT COUNT(*), (SELECT error FROM outbox WHERE done_at IS NULL AND error IS NOT NULL
                               ORDER BY id DESC LIMIT 1)
             FROM outbox WHERE done_at IS NULL",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open_at(&dir.path().join("w.db")).unwrap();
        (dir, store)
    }

    fn gate(store: &mut Store, thread: Option<&str>) -> i64 {
        store
            .insert_gate(&NewGate {
                project: "my-game",
                title: "Walls",
                body: "Check the walls",
                questions: &["Comfortable?".into(), "No gaps?".into()],
                thread_id: thread,
                commit_sha: Some("abc"),
                branch: None,
                worktree: None,
                created_by: None,
            })
            .unwrap()
    }

    #[test]
    fn gate_lifecycle() {
        let (_d, mut s) = store();
        let id = gate(&mut s, Some("t-1"));
        assert_eq!(s.questions(id).unwrap().len(), 2);
        assert_eq!(s.add_question(id, "Gates?").unwrap(), 3);
        s.add_reply(id, "telegram", "1. yes", None, Some(10))
            .unwrap();
        assert!(s.answer_gate(id, "answered", "telegram", true).unwrap());
        assert!(!s.answer_gate(id, "answered", "telegram", true).unwrap());
        let g = s.gate(id).unwrap();
        assert_eq!(g.status, "answered");
        assert_eq!(g.wake_state.as_deref(), Some("pending"));
        assert_eq!(s.due_wakes().unwrap().len(), 1);
        assert!(s.claim_wake(id, g.wake_gen).unwrap());
        assert!(!s.claim_wake(id, g.wake_gen).unwrap(), "claimed once");
        s.wake_failed(id, g.wake_gen, "down", Some(&crate::util::later(60)))
            .unwrap();
        assert!(s.due_wakes().unwrap().is_empty());
        s.wake_failed(id, g.wake_gen, "down", None).unwrap();
        assert_eq!(s.gate(id).unwrap().wake_state.as_deref(), Some("failed"));
        assert!(s.set_thread_and_rewake(id, Some("t-2")).unwrap());
        assert_eq!(s.due_wakes().unwrap().len(), 1);
        let generation = s.gate(id).unwrap().wake_gen;
        assert!(s.claim_wake(id, generation).unwrap());
        s.wake_done(id, generation).unwrap();
        assert!(
            !s.set_thread_and_rewake(id, Some("t-3")).unwrap(),
            "a delivered answer is not sent again"
        );
    }

    #[test]
    fn set_thread_during_a_wake_is_not_lost() {
        let (_d, mut s) = store();
        let id = gate(&mut s, Some("x"));
        s.answer_gate(id, "answered", "telegram", true).unwrap();
        let old = s.gate(id).unwrap().wake_gen;
        assert!(s.claim_wake(id, old).unwrap());
        // The operator points the answer at another thread while thread x is being woken.
        assert!(s.set_thread_and_rewake(id, Some("y")).unwrap());
        s.wake_done(id, old).unwrap();
        let g = s.gate(id).unwrap();
        assert_eq!(
            g.wake_state.as_deref(),
            Some("pending"),
            "the new thread still gets it"
        );
        assert_eq!(g.thread_id.as_deref(), Some("y"));
    }

    #[test]
    fn interrupted_wakes_are_sent_again() {
        let (_d, mut s) = store();
        let id = gate(&mut s, Some("x"));
        s.answer_gate(id, "answered", "telegram", true).unwrap();
        assert!(s.claim_wake(id, 0).unwrap());
        assert_eq!(s.release_stale_wakes().unwrap(), 1);
        assert_eq!(s.due_wakes().unwrap().len(), 1);
    }

    #[test]
    fn withdraw_only_open() {
        let (_d, mut s) = store();
        let id = gate(&mut s, None);
        assert!(s.withdraw_gate(id, "not needed").unwrap());
        assert!(!s.answer_gate(id, "accepted", "cli", false).unwrap());
    }

    #[test]
    fn outbox_is_in_order_and_claimed_before_sending() {
        let (_d, s) = store();
        let first = s
            .enqueue("gate_opened", serde_json::json!({"gate": 1}))
            .unwrap();
        let second = s
            .enqueue("publish", serde_json::json!({"publish": 1}))
            .unwrap();
        assert_eq!(s.due_outbox().unwrap().len(), 2);
        s.outbox_claim(first, &crate::util::later(60)).unwrap();
        // The later event waits behind the one being retried.
        assert!(s.due_outbox().unwrap().is_empty());
        s.outbox_retry(first, "network", &crate::util::later(-1))
            .unwrap();
        let due = s.due_outbox().unwrap();
        assert_eq!(
            due.iter().map(|i| i.id).collect::<Vec<_>>(),
            vec![first, second]
        );
        assert_eq!(due[0].attempts, 1, "the claim counted the attempt");
        s.outbox_done(first, None).unwrap();
        s.outbox_done(second, None).unwrap();
        assert_eq!(s.outbox_pending().unwrap().0, 0);
    }

    #[test]
    fn no_reply_lands_on_a_closed_gate() {
        let (_d, mut s) = store();
        let id = gate(&mut s, None);
        assert!(
            s.add_reply(id, "telegram", "yes", None, Some(5))
                .unwrap()
                .is_some()
        );
        assert!(s.answer_gate(id, "answered", "cli", false).unwrap());
        assert_eq!(
            s.add_reply(id, "telegram", "late", None, Some(6)).unwrap(),
            None
        );
        assert_eq!(s.replies(id).unwrap().len(), 1);
    }

    #[test]
    fn events_without_a_lane_get_their_project() {
        let (_d, mut s) = store();
        let id = gate(&mut s, None);
        s.conn
            .execute(
                "INSERT INTO outbox(kind, payload, created_at, next_at, attempts)
                 VALUES('gate_opened', ?1, ?2, ?2, 0)",
                params![serde_json::json!({"gate": id}).to_string(), now()],
            )
            .unwrap();
        assert_eq!(s.fill_lanes().unwrap(), 1);
        assert_eq!(s.due_outbox().unwrap()[0].lane, "my-game");
    }

    #[test]
    fn a_newer_topside_migrating_meanwhile_keeps_its_minimum_reader() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("w.db");
        drop(Store::open_at(&path).unwrap());
        // Another (newer) topside got the lock first and migrated beyond us.
        let conn = Connection::open(&path).unwrap();
        conn.pragma_update(None, "user_version", VERSION + 1)
            .unwrap();
        conn.execute(
            "UPDATE meta SET value = ?1 WHERE key = ?2",
            params![(VERSION + 1).to_string(), MIN_READER_KEY],
        )
        .unwrap();
        assert_eq!(Store::migrate(&conn).unwrap(), VERSION + 1);
        let kept: String = conn
            .query_row(
                "SELECT value FROM meta WHERE key = ?1",
                [MIN_READER_KEY],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(kept, (VERSION + 1).to_string(), "not lowered");
        drop(conn);
        assert!(Store::open_at(&path).is_err());
    }

    #[test]
    fn a_newer_schema_that_only_adds_is_usable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("w.db");
        drop(Store::open_at(&path).unwrap());
        let conn = Connection::open(&path).unwrap();
        conn.pragma_update(None, "user_version", VERSION + 1)
            .unwrap();
        drop(conn);
        assert!(Store::open_at(&path).is_ok(), "min reader is this version");
        let conn = Connection::open(&path).unwrap();
        conn.execute(
            "UPDATE meta SET value = ?1 WHERE key = ?2",
            params![(VERSION + 1).to_string(), MIN_READER_KEY],
        )
        .unwrap();
        drop(conn);
        let e = Store::open_at(&path).err().unwrap();
        assert!(e.to_string().contains("newer topside"), "{e}");
    }

    #[test]
    fn a_stuck_project_does_not_hold_up_another() {
        let (_d, mut s) = store();
        let a = gate(&mut s, None); // project my-game
        let b = s
            .insert_gate(&NewGate {
                project: "other",
                title: "t",
                body: "",
                questions: &["q".into()],
                thread_id: None,
                commit_sha: None,
                branch: None,
                worktree: None,
                created_by: None,
            })
            .unwrap();
        let ea = s
            .enqueue("gate_opened", serde_json::json!({"gate": a}))
            .unwrap();
        let ea2 = s
            .enqueue("gate_note", serde_json::json!({"gate": a, "note": 1}))
            .unwrap();
        let eb = s
            .enqueue("gate_opened", serde_json::json!({"gate": b}))
            .unwrap();
        s.outbox_claim(ea, &crate::util::later(600)).unwrap();
        let due: Vec<i64> = s.due_outbox().unwrap().iter().map(|i| i.id).collect();
        assert_eq!(
            due,
            vec![eb],
            "my-game waits behind its head; the other project goes on"
        );
        assert!(!due.contains(&ea2));
        let lanes = s.outbox_lanes().unwrap();
        assert_eq!(lanes.len(), 2);
    }

    #[test]
    fn publish_queues_its_announcement() {
        let (_d, mut s) = store();
        let id = s
            .insert_publish(
                "my-game",
                "abc",
                None,
                None,
                "s",
                &[],
                &[NewBuild {
                    platform: "macos",
                    file: "my-game/macos/a.zip",
                    stamp: "20261107-1200",
                    size: 1,
                }],
            )
            .unwrap();
        let due = s.due_outbox().unwrap();
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].payload["publish"].as_i64(), Some(id));
    }

    #[test]
    fn builds_are_newest_first_by_time_not_by_local_stamp() {
        let (_d, s) = store();
        // Published later but with an earlier local stamp (the machine moved west).
        s.adopt_build(
            "p",
            "macos",
            "p/macos/a.zip",
            "a",
            None,
            "20261107-1400",
            1,
            "",
            "2026-11-07T10:00:00Z",
        )
        .unwrap();
        s.adopt_build(
            "p",
            "macos",
            "p/macos/b.zip",
            "b",
            None,
            "20261107-1100",
            1,
            "",
            "2026-11-07T11:00:00Z",
        )
        .unwrap();
        assert_eq!(s.builds("p", None).unwrap()[0].file, "p/macos/b.zip");
    }

    #[test]
    fn version_one_databases_are_migrated() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("w.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(&format!("BEGIN; {SCHEMA} PRAGMA user_version = 1; COMMIT;"))
                .unwrap();
        }
        let mut s = Store::open_at(&path).unwrap();
        let id = gate(&mut s, None);
        let g = s.gate(id).unwrap();
        assert_eq!(g.wake_gen, 0);
        assert!(g.tg_posted_at.is_none());
        assert!(g.tg_shown_question.is_none());
        let v: i64 = s
            .conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(v, VERSION);
    }

    #[test]
    fn concurrent_first_opens_agree() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("w.db");
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let path = path.clone();
                std::thread::spawn(move || Store::open_at(&path).map(|_| ()))
            })
            .collect();
        for h in handles {
            h.join().unwrap().unwrap();
        }
        let s = Store::open_at(&path).unwrap();
        let v: i64 = s
            .conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(v, VERSION);
    }

    #[test]
    fn a_telegram_message_is_one_reply() {
        let (_d, mut s) = store();
        let id = gate(&mut s, None);
        let a = s.add_reply(id, "telegram", "x", None, Some(77)).unwrap();
        let b = s.add_reply(id, "telegram", "x", None, Some(77)).unwrap();
        assert_eq!(a, b);
        assert_eq!(s.replies(id).unwrap().len(), 1);
        s.add_reply(id, "cli", "y", None, None).unwrap();
        s.add_reply(id, "cli", "y", None, None).unwrap();
        assert_eq!(
            s.replies(id).unwrap().len(),
            3,
            "CLI parts are not deduplicated"
        );
    }

    #[test]
    fn an_expired_wake_lease_is_due_again() {
        let (_d, mut s) = store();
        let id = gate(&mut s, Some("x"));
        s.answer_gate(id, "answered", "telegram", true).unwrap();
        assert!(s.claim_wake(id, 0).unwrap());
        assert!(s.due_wakes().unwrap().is_empty(), "leased");
        s.conn
            .execute(
                "UPDATE gates SET wake_next_at = ?1 WHERE id = ?2",
                params![crate::util::later(-1), id],
            )
            .unwrap();
        assert_eq!(s.due_wakes().unwrap().len(), 1);
        assert!(s.claim_wake(id, 0).unwrap());
    }

    #[test]
    fn message_map() {
        let (_d, mut s) = store();
        let id = gate(&mut s, None);
        s.map_message(-100, 5, id).unwrap();
        s.map_message(-100, 5, id).unwrap();
        assert_eq!(s.gate_of_message(-100, 5).unwrap(), Some(id));
        assert_eq!(s.gate_of_message(-100, 6).unwrap(), None);
    }
}
