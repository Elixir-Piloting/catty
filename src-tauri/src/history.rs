//! Persistent command history with prefix matching (most-recently-used first).
//! Backed by SQLite in the app data directory.

use parking_lot::Mutex;
use rusqlite::{params, Connection};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

pub struct History {
    conn: Mutex<Connection>,
}

impl History {
    /// Open (or create) the history database at `path`.
    pub fn open(path: PathBuf) -> rusqlite::Result<Self> {
        let conn = Connection::open(path)?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS commands (
                id          INTEGER PRIMARY KEY AUTOINCREMENT,
                command     TEXT NOT NULL UNIQUE,
                last_used_at INTEGER NOT NULL,
                use_count   INTEGER NOT NULL DEFAULT 1
            );
            CREATE INDEX IF NOT EXISTS idx_commands_command ON commands(command);",
        )?;
        Ok(Self { conn: Mutex::new(conn) })
    }

    /// Record (or bump) a command. Empty / whitespace-only commands are ignored.
    pub fn add(&self, command: &str) {
        let trimmed = command.trim();
        if trimmed.is_empty() {
            return;
        }
        let now = now_ms();
        let conn = self.conn.lock();
        let _ = conn.execute(
            "INSERT INTO commands (command, last_used_at, use_count)
             VALUES (?1, ?2, 1)
             ON CONFLICT(command) DO UPDATE SET
                last_used_at = ?2,
                use_count    = use_count + 1",
            params![trimmed, now],
        );
    }

    /// Return the most-recently-used command that starts with `prefix` but is
    /// not exactly equal to it. Returns the full command (caller strips prefix
    /// to obtain the ghost-text suffix).
    pub fn match_prefix(&self, prefix: &str) -> Option<String> {
        if prefix.is_empty() {
            return None;
        }
        let conn = self.conn.lock();
        // Escape special LIKE chars in the prefix FIRST, then append the
        // unescaped `%` wildcard. (Escaping after appending would escape
        // the wildcard itself and match literally "prefix%", not "prefix*".)
        let like = format!("{}%", escape_like(prefix));
        conn.query_row(
            "SELECT command FROM commands
             WHERE command LIKE ?1 ESCAPE '\\'
               AND command != ?2
             ORDER BY last_used_at DESC
             LIMIT 1",
            params![like, prefix],
            |row| row.get::<_, String>(0),
        )
        .ok()
    }
}

fn escape_like(s: &str) -> String {
    s.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_")
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Shared, cloneable handle (the inner Connection is behind a Mutex).
pub type SharedHistory = Arc<History>;
