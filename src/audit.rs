//! Audit log for conversation persistence and system event tracking.
//!
//! Stores all user↔assistant exchanges and key system events in a SQLite
//! database (`audit.db`), separate from the vector-search `memory.db`.
//!
//! All public functions are fire-and-forget: failures are logged via `warn!`
//! but never block message delivery.

use anyhow::{Context, Result, anyhow};
use rusqlite::Connection;

use crate::backends::TokenUsage;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use tracing::warn;

static DB: Mutex<Option<Connection>> = Mutex::new(None);
struct Settings {
    db_path: PathBuf,
    enabled: bool,
}
static SETTINGS: OnceLock<Settings> = OnceLock::new();

pub fn init(db_path: PathBuf, enabled: bool) {
    let _ = SETTINGS.set(Settings { db_path, enabled });
}

fn with_db<F, R>(f: F) -> Result<R>
where
    F: FnOnce(&Connection) -> Result<R>,
{
    let mut guard = DB
        .lock()
        .map_err(|e| anyhow::anyhow!("Audit DB lock poisoned: {}", e))?;

    if guard.is_none() {
        *guard = Some(open_db()?);
    }

    f(guard.as_ref().unwrap())
}

fn open_db() -> Result<Connection> {
    let path = &SETTINGS
        .get()
        .ok_or_else(|| anyhow!("audit not initialised"))?
        .db_path;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let conn = Connection::open(path)
        .with_context(|| format!("Failed to open audit database: {:?}", path))?;

    conn.execute_batch("PRAGMA journal_mode=WAL;")?;
    prepare_schema(&conn)?;

    Ok(conn)
}

fn prepare_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS messages (
            id              INTEGER PRIMARY KEY,
            channel         TEXT NOT NULL,
            user_id         TEXT NOT NULL,
            session_id      TEXT,
            user_message    TEXT NOT NULL,
            assistant_response TEXT NOT NULL,
            duration_ms     INTEGER,
            cost_usd        REAL,
            input_tokens    INTEGER,
            output_tokens   INTEGER,
            cached_input_tokens INTEGER,
            error           INTEGER NOT NULL DEFAULT 0,
            created_at      TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_messages_channel_user
            ON messages(channel, user_id);
        CREATE INDEX IF NOT EXISTS idx_messages_created_at
            ON messages(created_at);

        CREATE TABLE IF NOT EXISTS events (
            id          INTEGER PRIMARY KEY,
            event_type  TEXT NOT NULL,
            channel     TEXT,
            user_id     TEXT,
            detail      TEXT,
            created_at  TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_events_type
            ON events(event_type);
        CREATE INDEX IF NOT EXISTS idx_events_created_at
            ON events(created_at);",
    )?;

    for (column, ty) in [
        ("cost_usd", "REAL"),
        ("input_tokens", "INTEGER"),
        ("output_tokens", "INTEGER"),
        ("cached_input_tokens", "INTEGER"),
    ] {
        let present: bool = conn
            .prepare("SELECT COUNT(*) FROM pragma_table_info('messages') WHERE name = ?1")?
            .query_row([column], |row| row.get::<_, i64>(0))
            .map(|n| n > 0)
            .unwrap_or(false);
        if !present {
            conn.execute_batch(&format!("ALTER TABLE messages ADD COLUMN {column} {ty};"))?;
        }
    }

    Ok(())
}

fn is_enabled() -> bool {
    SETTINGS.get().is_some_and(|settings| settings.enabled)
}

/// One user↔assistant exchange, as written to the audit log.
pub struct MessageRecord<'a> {
    pub channel: &'a str,
    pub user_id: &'a str,
    pub user_message: &'a str,
    pub assistant_response: &'a str,
    pub session_id: Option<&'a str>,
    pub duration_ms: Option<u64>,
    pub cost_usd: Option<f64>,
    pub tokens: Option<TokenUsage>,
    pub is_error: bool,
}

/// Log a user↔assistant message exchange. Errors are swallowed.
pub fn log_message(record: MessageRecord) {
    if !is_enabled() {
        return;
    }
    if let Err(e) = with_db(|conn| insert_message(conn, record)) {
        warn!("Failed to log audit message: {}", e);
    }
}

fn insert_message(conn: &Connection, record: MessageRecord) -> Result<()> {
    conn.execute(
        "INSERT INTO messages (channel, user_id, session_id, user_message, assistant_response,
            duration_ms, cost_usd, input_tokens, output_tokens, cached_input_tokens, error, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, datetime('now'))",
        rusqlite::params![
            record.channel,
            record.user_id,
            record.session_id,
            record.user_message,
            record.assistant_response,
            record.duration_ms.map(|d| d as i64),
            record.cost_usd,
            record.tokens.map(|t| t.input as i64),
            record.tokens.map(|t| t.output as i64),
            record.tokens.map(|t| t.cached_input as i64),
            record.is_error as i32,
        ],
    )?;
    Ok(())
}

/// Log a system event. Errors are swallowed.
pub fn log_event(
    event_type: &str,
    channel: Option<&str>,
    user_id: Option<&str>,
    detail: Option<&str>,
) {
    if !is_enabled() {
        return;
    }
    if let Err(e) = log_event_inner(event_type, channel, user_id, detail) {
        warn!("Failed to log audit event: {}", e);
    }
}

fn log_event_inner(
    event_type: &str,
    channel: Option<&str>,
    user_id: Option<&str>,
    detail: Option<&str>,
) -> Result<()> {
    with_db(|conn| {
        conn.execute(
            "INSERT INTO events (event_type, channel, user_id, detail, created_at)
             VALUES (?1, ?2, ?3, ?4, datetime('now'))",
            rusqlite::params![event_type, channel, user_id, detail],
        )?;
        Ok(())
    })
}

/// Query per-user usage stats from the audit database.
pub fn get_usage(channel: &str, user_id: &str) -> Result<(u64, Option<f64>)> {
    with_db(|conn| {
        let (count, total_cost): (u64, Option<f64>) = conn.query_row(
            "SELECT COUNT(*), SUM(cost_usd) FROM messages WHERE channel = ?1 AND user_id = ?2 AND error = 0",
            rusqlite::params![channel, user_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        Ok((count, total_cost))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backends::TokenUsage;

    #[test]
    fn an_existing_database_gains_the_token_columns() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE messages (
                id INTEGER PRIMARY KEY,
                channel TEXT NOT NULL,
                user_id TEXT NOT NULL,
                session_id TEXT,
                user_message TEXT NOT NULL,
                assistant_response TEXT NOT NULL,
                duration_ms INTEGER,
                error INTEGER NOT NULL DEFAULT 0,
                created_at TEXT NOT NULL
            );",
        )
        .unwrap();

        prepare_schema(&conn).unwrap();

        let columns: Vec<String> = conn
            .prepare("SELECT name FROM pragma_table_info('messages')")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        for column in [
            "cost_usd",
            "input_tokens",
            "output_tokens",
            "cached_input_tokens",
        ] {
            assert!(columns.contains(&column.to_string()), "missing {column}");
        }
    }

    #[test]
    fn a_logged_message_keeps_its_token_counts() {
        let conn = Connection::open_in_memory().unwrap();
        prepare_schema(&conn).unwrap();

        insert_message(
            &conn,
            MessageRecord {
                channel: "slack",
                user_id: "U1",
                user_message: "hi",
                assistant_response: "hello",
                session_id: Some("s-1"),
                duration_ms: Some(1200),
                cost_usd: Some(0.5),
                tokens: Some(TokenUsage {
                    input: 20,
                    output: 4,
                    cached_input: 150,
                }),
                is_error: false,
            },
        )
        .unwrap();

        let row: (i64, i64, i64) = conn
            .query_row(
                "SELECT input_tokens, output_tokens, cached_input_tokens FROM messages",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(row, (20, 4, 150));
    }
}
