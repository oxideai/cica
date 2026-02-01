//! AI Backend abstraction for Claude Code and Cursor CLI

use anyhow::Result;

use crate::config::{AiBackend, Config};
use crate::{claude, cursor};

/// Options for querying an AI backend
#[derive(Default)]
pub struct QueryOptions {
    /// System prompt / context to use
    pub system_prompt: Option<String>,
    /// Resume an existing session by ID
    pub resume_session: Option<String>,
    /// Working directory
    pub cwd: Option<String>,
    /// Skip permission/confirmation prompts
    pub skip_permissions: bool,
}

/// Query the configured AI backend with options
/// Returns (response, session_id)
pub async fn query_with_options(prompt: &str, options: QueryOptions) -> Result<(String, String)> {
    let config = Config::load()?;

    match config.backend {
        AiBackend::Claude => query_claude(prompt, options).await,
        AiBackend::Cursor => query_cursor(prompt, options).await,
    }
}

/// Query Claude Code
async fn query_claude(prompt: &str, options: QueryOptions) -> Result<(String, String)> {
    let claude_options = claude::QueryOptions {
        system_prompt: options.system_prompt,
        resume_session: options.resume_session,
        cwd: options.cwd,
        skip_permissions: options.skip_permissions,
    };

    claude::query_with_options(prompt, claude_options).await
}

/// Query Cursor CLI
async fn query_cursor(prompt: &str, options: QueryOptions) -> Result<(String, String)> {
    let cursor_options = cursor::QueryOptions {
        context: options.system_prompt,
        resume_session: options.resume_session,
        cwd: options.cwd,
        force: options.skip_permissions,
        model: None,
    };

    cursor::query_with_options(prompt, cursor_options).await
}

/// Get the name of the currently configured backend
#[allow(dead_code)]
pub fn current_backend_name() -> Result<&'static str> {
    let config = Config::load()?;
    Ok(match config.backend {
        AiBackend::Claude => "Claude Code",
        AiBackend::Cursor => "Cursor CLI",
    })
}
