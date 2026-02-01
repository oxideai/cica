//! Cursor CLI integration

use anyhow::{Result, anyhow, bail};
use serde::Deserialize;
use std::path::Path;
use std::process::Stdio;
use tokio::process::Command;
use tracing::{debug, info, warn};

use crate::config::{self, Config};
use crate::setup;

/// Password for the sandboxed keychain (not secret - just for isolation)
const KEYCHAIN_PASSWORD: &str = "cica";

/// Default model to use if none specified
const DEFAULT_MODEL: &str = "opus-4.5";

/// Response event from Cursor CLI in stream-json format
#[derive(Debug, Deserialize)]
struct CursorEvent {
    #[serde(rename = "type")]
    event_type: String,
    /// For "result" events
    result: Option<String>,
    /// Session ID (present in most events)
    session_id: Option<String>,
    /// For "result" events
    duration_ms: Option<u64>,
    /// For error detection
    is_error: Option<bool>,
}

/// Options for querying Cursor
#[derive(Default)]
pub struct QueryOptions {
    /// Context to prepend to the message
    pub context: Option<String>,
    /// Resume an existing session by ID
    pub resume_session: Option<String>,
    /// Working directory for Cursor
    pub cwd: Option<String>,
    /// Model to use
    pub model: Option<String>,
    /// Skip confirmation prompts
    pub force: bool,
}

/// Query Cursor with a prompt and return the response
#[allow(dead_code)]
pub async fn query(prompt: &str) -> Result<String> {
    let (result, _) = query_with_options(prompt, QueryOptions::default()).await?;
    Ok(result)
}

/// Query Cursor with options and return (response, session_id)
pub async fn query_with_options(prompt: &str, options: QueryOptions) -> Result<(String, String)> {
    let config = Config::load()?;
    let paths = config::paths()?;

    // Get API key
    let api_key = config.cursor.api_key.ok_or_else(|| {
        anyhow!("No Cursor API key configured. Run `cica init` to set up Cursor.")
    })?;

    // Get Cursor CLI path
    let cursor_cli = setup::find_cursor_cli()
        .ok_or_else(|| anyhow!("Cursor CLI not found. Run `cica init` to set up Cursor."))?;

    // Build the actual prompt - prepend context if provided
    let full_prompt = match &options.context {
        Some(context) => format!("<context>\n{}\n</context>\n\n{}", context, prompt),
        None => prompt.to_string(),
    };

    info!("Querying Cursor: {}", prompt);
    debug!("Using cursor_cli: {:?}", cursor_cli);

    // Ensure sandboxed keychain exists and is unlocked (macOS)
    ensure_keychain(&paths.cursor_home).await?;

    let mut cmd = Command::new(&cursor_cli);
    cmd.args(["-p", "--output-format", "stream-json"])
        .args(["--api-key", &api_key])
        .env("HOME", &paths.cursor_home);

    // Force mode for automated flows
    if options.force {
        cmd.arg("--force");
    }

    // Model selection
    let model = options
        .model
        .or(config.cursor.model)
        .unwrap_or_else(|| DEFAULT_MODEL.to_string());
    cmd.args(["--model", &model]);

    // Resume existing session if provided
    if let Some(ref session_id) = options.resume_session {
        cmd.args(["--resume", session_id]);
    }

    // Set working directory
    if let Some(ref cwd) = options.cwd {
        cmd.current_dir(cwd);
    } else {
        cmd.current_dir(&paths.base);
    }

    // Add the prompt
    cmd.arg(&full_prompt);

    let output = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    if !output.status.success() {
        warn!("Cursor CLI failed. stdout: {}", stdout);
        warn!("Cursor CLI failed. stderr: {}", stderr);
        bail!(
            "Cursor CLI failed (exit {:?}): {}{}",
            output.status.code(),
            stderr,
            if stderr.is_empty() { &stdout } else { "" }
        );
    }

    debug!("Cursor raw output: {}", stdout);

    // Parse the stream-json response
    let mut final_result = None;
    let mut final_session_id = None;

    for line in stdout.lines() {
        if line.trim().is_empty() {
            continue;
        }

        let Ok(event) = serde_json::from_str::<CursorEvent>(line) else {
            continue;
        };

        // Track session_id from any event
        if event.session_id.is_some() {
            final_session_id = event.session_id.clone();
        }

        // Look for result event
        if event.event_type == "result" {
            if event.is_error == Some(true) {
                bail!("Cursor returned an error");
            }
            if let Some(result) = event.result {
                info!(
                    "Cursor response received ({}ms)",
                    event.duration_ms.unwrap_or(0)
                );
                final_result = Some(result);
            }
        }
    }

    match final_result {
        Some(result) => Ok((result, final_session_id.unwrap_or_default())),
        None => Err(anyhow!("No result found in Cursor output")),
    }
}

/// Ensure the sandboxed keychain exists and is unlocked (macOS only)
#[cfg(target_os = "macos")]
async fn ensure_keychain(cursor_home: &Path) -> Result<()> {
    let keychain_dir = cursor_home.join("Library/Keychains");
    let keychain_path = keychain_dir.join("login.keychain-db");

    std::fs::create_dir_all(&keychain_dir)?;

    // Create keychain if it doesn't exist
    if !keychain_path.exists() {
        debug!("Creating sandboxed keychain at {:?}", keychain_path);
        let output = std::process::Command::new("security")
            .args([
                "create-keychain",
                "-p",
                KEYCHAIN_PASSWORD,
                keychain_path.to_str().unwrap(),
            ])
            .output()?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            // Ignore "already exists" errors
            if !stderr.contains("already exists") {
                warn!("Failed to create keychain: {}", stderr);
            }
        }
    }

    // Unlock the keychain
    debug!("Unlocking sandboxed keychain");
    let output = std::process::Command::new("security")
        .args([
            "unlock-keychain",
            "-p",
            KEYCHAIN_PASSWORD,
            keychain_path.to_str().unwrap(),
        ])
        .output()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        warn!("Failed to unlock keychain: {}", stderr);
    }

    // Set keychain settings to not auto-lock
    let _ = std::process::Command::new("security")
        .args(["set-keychain-settings", keychain_path.to_str().unwrap()])
        .output();

    Ok(())
}

/// No-op on non-macOS platforms
#[cfg(not(target_os = "macos"))]
async fn ensure_keychain(_cursor_home: &Path) -> Result<()> {
    Ok(())
}
