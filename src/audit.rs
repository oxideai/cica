//! Audit log for conversation persistence and system event tracking.
//!
//! Stores all user↔assistant exchanges and key system events in a SQLite
//! database (`audit.db`), separate from the vector-search `memory.db`.
//!
//! All public functions are fire-and-forget: failures are logged via `warn!`
//! but never block message delivery.

use anyhow::{Context, Result};
use rusqlite::Connection;
use std::sync::Mutex;
use tracing::warn;

use crate::config::{self, Config};

// ============================================================================
// Global connection (same pattern as memory.rs)
// ============================================================================

static DB: Mutex<Option<Connection>> = Mutex::new(None);

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
    let path = config::paths()?.audit_db;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let conn = Connection::open(&path)
        .with_context(|| format!("Failed to open audit database: {:?}", path))?;

    conn.execute_batch("PRAGMA journal_mode=WAL;")?;

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

    // Add cost_usd column if missing (migration for existing databases)
    let has_cost_usd: bool = conn
        .prepare("SELECT COUNT(*) FROM pragma_table_info('messages') WHERE name='cost_usd'")?
        .query_row([], |row| row.get::<_, i64>(0))
        .map(|n| n > 0)
        .unwrap_or(false);
    if !has_cost_usd {
        conn.execute_batch("ALTER TABLE messages ADD COLUMN cost_usd REAL;")?;
    }

    Ok(conn)
}

/// Check if audit logging is enabled in the config.
fn is_enabled() -> bool {
    Config::load().map(|c| c.audit).unwrap_or(true)
}

// ============================================================================
// Public API
// ============================================================================

/// Log a user↔assistant message exchange. Errors are swallowed.
#[allow(clippy::too_many_arguments)]
pub fn log_message(
    channel: &str,
    user_id: &str,
    user_message: &str,
    assistant_response: &str,
    session_id: Option<&str>,
    duration_ms: Option<u64>,
    cost_usd: Option<f64>,
    is_error: bool,
) {
    if !is_enabled() {
        return;
    }
    if let Err(e) = log_message_inner(
        channel,
        user_id,
        user_message,
        assistant_response,
        session_id,
        duration_ms,
        cost_usd,
        is_error,
    ) {
        warn!("Failed to log audit message: {}", e);
    }
}

#[allow(clippy::too_many_arguments)]
fn log_message_inner(
    channel: &str,
    user_id: &str,
    user_message: &str,
    assistant_response: &str,
    session_id: Option<&str>,
    duration_ms: Option<u64>,
    cost_usd: Option<f64>,
    is_error: bool,
) -> Result<()> {
    with_db(|conn| {
        conn.execute(
            "INSERT INTO messages (channel, user_id, session_id, user_message, assistant_response, duration_ms, cost_usd, error, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, datetime('now'))",
            rusqlite::params![
                channel,
                user_id,
                session_id,
                user_message,
                assistant_response,
                duration_ms.map(|d| d as i64),
                cost_usd,
                is_error as i32,
            ],
        )?;
        Ok(())
    })
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
