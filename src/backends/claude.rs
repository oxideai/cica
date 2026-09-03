//! Claude Code integration

use anyhow::{Result, anyhow, bail};
use serde::Deserialize;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::Stdio;
use tokio::process::Command;
use tracing::{debug, info, warn};

use crate::backends::{QueryOptions, QueryResult};
use crate::config::{ClaudeConfig, Paths};
use crate::setup;

pub const MODELS: &[(&str, &str)] = &[
    ("claude-opus-4-6", "Claude Opus 4.6"),
    ("claude-opus-4-5", "Claude Opus 4.5"),
    ("claude-sonnet-4-5", "Claude Sonnet 4.5"),
];

#[derive(Debug, Deserialize)]
struct ClaudeResponse {
    #[serde(rename = "type")]
    response_type: String,
    result: Option<String>,
    session_id: Option<String>,
    duration_ms: Option<u64>,
    total_cost_usd: Option<f64>,
    /// Keyed by the concrete model ID served; the only place an alias like "opus" resolves.
    #[serde(rename = "modelUsage", default)]
    model_usage: Option<serde_json::Map<String, serde_json::Value>>,
}

fn served_models(usage: &Option<serde_json::Map<String, serde_json::Value>>) -> Option<String> {
    let usage = usage.as_ref()?;
    if usage.is_empty() {
        return None;
    }
    let mut ids: Vec<&str> = usage.keys().map(String::as_str).collect();
    ids.sort_unstable();
    Some(ids.join(", "))
}

// Claude Code's wording: "No conversation found with session ID: <uuid>".
fn is_missing_conversation(stderr: &str) -> bool {
    stderr.to_lowercase().contains("no conversation found")
}

fn config_relative_path(paths: &Paths, value: &str) -> std::path::PathBuf {
    let path = std::path::Path::new(value);
    if path.is_relative() {
        paths.config_file.parent().unwrap_or(&paths.base).join(path)
    } else {
        path.to_path_buf()
    }
}

pub async fn query_with_options(
    claude: &ClaudeConfig,
    paths: &Paths,
    prompt: &str,
    options: QueryOptions,
) -> Result<QueryResult> {
    let use_vertex = claude.use_vertex;
    let vertex_project_id = claude.vertex_project_id.as_deref();
    let credential = claude.api_key.as_deref();

    if use_vertex {
        let project_id = vertex_project_id
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow!("Vertex AI is enabled but no project ID is set. Run `cica init` to configure Vertex AI."))?;
        debug!("Using Vertex AI project: {}", project_id);
    } else {
        credential.ok_or_else(|| {
            anyhow!("No credential configured. Run `cica init` to set up Claude.")
        })?;
    }

    let claude_code = setup::find_claude_code(paths)
        .ok_or_else(|| anyhow!("Claude Code not found. Run `cica init` to set up Claude."))?;

    let (program, prefix_args): (PathBuf, Vec<PathBuf>) = match &claude_code {
        setup::ClaudeCode::Native(exe) => (exe.clone(), Vec::new()),
        setup::ClaudeCode::Script(js) => {
            let bun = setup::find_bun(paths)
                .ok_or_else(|| anyhow!("Bun not found. Run `cica init` to set up Claude."))?;
            (bun, vec![PathBuf::from("run"), js.clone()])
        }
    };

    match options.model.as_deref() {
        Some(model) => info!("Claude model requested: {}", model),
        None => info!("Claude model requested: none configured, using the CLI default"),
    }

    info!("Querying Claude: {}", prompt);
    debug!("Using claude_code: {:?}", claude_code);

    let build_command = |resume_session: Option<&str>| {
        let mut cmd = Command::new(&program);
        cmd.args(&prefix_args)
            .args(["-p", "--output-format", "json"])
            .env("HOME", &paths.claude_home);

        if options.skip_permissions {
            cmd.arg("--dangerously-skip-permissions");
        }

        if let Some(ref system_prompt) = options.system_prompt {
            if resume_session.is_none() {
                cmd.args(["--system-prompt", system_prompt]);
            } else {
                cmd.args(["--append-system-prompt", system_prompt]);
            }
        }

        if let Some(session_id) = resume_session {
            cmd.args(["--resume", session_id]);
        }

        if let Some(ref model) = options.model {
            cmd.args(["--model", model]);
        }

        cmd.current_dir(&paths.base);
        cmd.kill_on_drop(true);
        cmd.as_std_mut().process_group(0);

        cmd.arg(prompt);

        if use_vertex {
            cmd.env("CLAUDE_CODE_USE_VERTEX", "1");
            cmd.env(
                "ANTHROPIC_VERTEX_PROJECT_ID",
                vertex_project_id.unwrap_or(""),
            );
            cmd.env(
                "CLOUD_ML_REGION",
                claude.vertex_region.as_deref().unwrap_or("europe-west1"),
            );
            // Long-lived auth: service account key file (recommended for servers; no gcloud expiry)
            if let Some(ref cred_path) = claude.vertex_credentials_path {
                let abs = config_relative_path(paths, cred_path);
                if abs.exists() {
                    cmd.env("GOOGLE_APPLICATION_CREDENTIALS", &abs);
                }
            }
            // Otherwise Vertex uses gcloud ADC or existing GOOGLE_APPLICATION_CREDENTIALS env
        } else if let Some(cred) = credential {
            match setup::detect_credential_type(cred) {
                setup::CredentialType::ApiKey => {
                    cmd.env("ANTHROPIC_API_KEY", cred);
                }
                setup::CredentialType::OAuthToken => {
                    cmd.env("CLAUDE_CODE_OAUTH_TOKEN", cred);
                    cmd.env("ANTHROPIC_OAUTH_TOKEN", cred);
                }
            }
        }

        cmd
    };

    let mut resume = options.resume_session.clone();
    let output = loop {
        let mut command = build_command(resume.as_deref());
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let child = command.spawn()?;
        let mut group =
            crate::backends::ProcessGroupGuard::new(child.id().expect("spawned child has pid"));
        let output = child.wait_with_output().await?;
        group.disarm();

        if output.status.success() {
            break output;
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        if let Some(lost) = resume.take().filter(|_| is_missing_conversation(&stderr)) {
            warn!(
                "Session {} no longer exists; starting a fresh session and losing its history",
                lost
            );
            continue;
        }

        warn!("Claude CLI failed. stdout: {}", stdout);
        warn!("Claude CLI failed. stderr: {}", stderr);
        bail!(
            "Claude CLI failed (exit {:?}): {}{}",
            output.status.code(),
            stderr,
            if stderr.is_empty() { &stdout } else { "" }
        );
    };

    let stdout = String::from_utf8_lossy(&output.stdout);

    debug!("Claude raw output: {}", stdout);

    for line in stdout.lines() {
        if line.trim().is_empty() {
            continue;
        }

        let Ok(response) = serde_json::from_str::<ClaudeResponse>(line) else {
            continue;
        };

        if response.response_type == "result"
            && let Some(result) = response.result
        {
            info!(
                "Claude response received ({}ms, ${:.4}, served by {})",
                response.duration_ms.unwrap_or(0),
                response.total_cost_usd.unwrap_or(0.0),
                served_models(&response.model_usage).unwrap_or_else(|| "unreported".into())
            );
            return Ok(QueryResult {
                response: result,
                session_id: response.session_id.unwrap_or_default(),
                duration_ms: response.duration_ms,
                cost_usd: response.total_cost_usd,
            });
        }
    }

    Err(anyhow!("No result found in Claude output"))
}

#[cfg(test)]
mod tests {
    use super::{ClaudeResponse, config_relative_path, is_missing_conversation, served_models};

    #[test]
    fn vertex_credentials_resolve_from_config_directory() {
        let mut paths = crate::config::Paths::for_base(std::path::PathBuf::from("/worker"));
        paths.config_file = std::path::PathBuf::from("/router/config.toml");
        assert_eq!(
            config_relative_path(&paths, "credentials.json"),
            std::path::PathBuf::from("/router/credentials.json")
        );
    }

    const RESULT_ENVELOPE: &str = r#"{"type":"result","subtype":"success","result":"ok",
        "session_id":"s-1","duration_ms":5840,"total_cost_usd":0.0417,
        "modelUsage":{"claude-opus-4-6":{"inputTokens":12,"outputTokens":3}}}"#;

    #[test]
    fn parses_the_served_model_from_the_result_envelope() {
        let parsed: ClaudeResponse = serde_json::from_str(RESULT_ENVELOPE).unwrap();
        assert_eq!(
            served_models(&parsed.model_usage).as_deref(),
            Some("claude-opus-4-6")
        );
    }

    #[test]
    fn served_models_lists_every_model_a_turn_billed() {
        let parsed: ClaudeResponse = serde_json::from_str(
            r#"{"type":"result","modelUsage":{"claude-opus-4-6":{},"claude-haiku-4-5":{}}}"#,
        )
        .unwrap();
        assert_eq!(
            served_models(&parsed.model_usage).as_deref(),
            Some("claude-haiku-4-5, claude-opus-4-6")
        );
    }

    #[test]
    fn a_response_without_model_usage_still_parses() {
        let parsed: ClaudeResponse =
            serde_json::from_str(r#"{"type":"result","result":"ok","session_id":"s-1"}"#).unwrap();
        assert_eq!(parsed.result.as_deref(), Some("ok"));
        assert!(served_models(&parsed.model_usage).is_none());
    }

    #[test]
    fn an_empty_model_usage_map_reports_nothing() {
        let parsed: ClaudeResponse =
            serde_json::from_str(r#"{"type":"result","modelUsage":{}}"#).unwrap();
        assert!(served_models(&parsed.model_usage).is_none());
    }

    #[test]
    fn detects_a_missing_conversation() {
        assert!(is_missing_conversation(
            "No conversation found with session ID: b1623d31-e974-4d04-a3ea-36493ce262f3"
        ));
    }

    #[test]
    fn detection_is_case_insensitive() {
        assert!(is_missing_conversation(
            "no conversation found with session id: abc"
        ));
    }

    #[test]
    fn leaves_unrelated_failures_alone() {
        for stderr in [
            "Invalid API key",
            "rate limit exceeded",
            "session ID is malformed",
            "Error: connection reset by peer",
            "",
        ] {
            assert!(
                !is_missing_conversation(stderr),
                "should not match: {stderr}"
            );
        }
    }
}
