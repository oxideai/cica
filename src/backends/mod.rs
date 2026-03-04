//! AI Backend abstraction for Claude Code and Cursor CLI

pub mod claude;
pub mod cursor;

use anyhow::Result;

use crate::config::{AiBackend, Config};

#[derive(Default)]
pub struct QueryOptions {
    pub system_prompt: Option<String>,
    pub resume_session: Option<String>,
    pub cwd: Option<String>,
    pub skip_permissions: bool,
}

/// Result returned by all AI backends.
#[derive(Debug, Clone)]
pub struct QueryResult {
    pub response: String,
    pub session_id: String,
    pub duration_ms: Option<u64>,
    /// Cost of this query in USD (backend-dependent; Claude provides this, Cursor does not).
    pub cost_usd: Option<f64>,
}

/// Query the configured AI backend.
pub async fn query_with_options(prompt: &str, options: QueryOptions) -> Result<QueryResult> {
    let config = Config::load()?;

    match config.backend {
        AiBackend::Claude => query_claude(prompt, options, &config).await,
        AiBackend::Cursor => query_cursor(prompt, options, &config).await,
    }
}

async fn query_claude(
    prompt: &str,
    options: QueryOptions,
    config: &Config,
) -> Result<QueryResult> {
    let claude_options = claude::QueryOptions {
        system_prompt: options.system_prompt,
        resume_session: options.resume_session,
        cwd: options.cwd,
        skip_permissions: options.skip_permissions,
        model: config.claude.model.clone(),
    };

    claude::query_with_options(prompt, claude_options).await
}

async fn query_cursor(
    prompt: &str,
    options: QueryOptions,
    config: &Config,
) -> Result<QueryResult> {
    let cursor_options = cursor::QueryOptions {
        context: options.system_prompt,
        resume_session: options.resume_session,
        cwd: options.cwd,
        force: options.skip_permissions,
        model: config.cursor.model.clone(),
    };

    cursor::query_with_options(prompt, cursor_options).await
}

#[allow(dead_code)]
pub fn current_backend_name() -> Result<&'static str> {
    let config = Config::load()?;
    Ok(match config.backend {
        AiBackend::Claude => "Claude Code",
        AiBackend::Cursor => "Cursor CLI",
    })
}
