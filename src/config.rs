use anyhow::{Context, Result};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

// ============================================================================
// Paths
// ============================================================================

/// All paths used by Cica
pub struct Paths {
    pub base: PathBuf,
    pub config_file: PathBuf,
    pub pairing_file: PathBuf,
    pub memory_dir: PathBuf,
    pub skills_dir: PathBuf,
    // Internal paths (hidden from user)
    pub internal_dir: PathBuf,
    pub deps_dir: PathBuf,
    pub bun_dir: PathBuf,
    pub java_dir: PathBuf,
    pub signal_cli_dir: PathBuf,
    pub claude_code_dir: PathBuf,
    pub claude_home: PathBuf,
    pub signal_data_dir: PathBuf,
    // Cursor CLI paths
    pub cursor_cli_dir: PathBuf,
    pub cursor_home: PathBuf,
    // Audit database
    pub audit_db: PathBuf,
}

/// Get all Cica paths
pub fn paths() -> Result<Paths> {
    let base = ProjectDirs::from("", "", "cica")
        .map(|dirs| dirs.config_dir().to_path_buf())
        .context("Could not determine config directory")?;

    let internal_dir = base.join("internal");
    let deps_dir = internal_dir.join("deps");

    Ok(Paths {
        config_file: base.join("config.toml"),
        pairing_file: base.join("pairing.json"),
        memory_dir: base.join("memory"),
        skills_dir: base.join("skills"),
        // Internal paths
        internal_dir: internal_dir.clone(),
        deps_dir: deps_dir.clone(),
        bun_dir: deps_dir.join("bun"),
        java_dir: deps_dir.join("java"),
        signal_cli_dir: deps_dir.join("signal-cli"),
        claude_code_dir: deps_dir.join("claude-code"),
        claude_home: internal_dir.join("claude-home"),
        signal_data_dir: internal_dir.join("signal-data"),
        // Cursor CLI paths
        cursor_cli_dir: deps_dir.join("cursor-cli"),
        cursor_home: internal_dir.join("cursor-home"),
        // Audit database
        audit_db: base.join("audit.db"),
        base,
    })
}

impl Paths {
    /// Create all necessary directories and default files
    pub fn ensure_dirs(&self) -> Result<()> {
        std::fs::create_dir_all(&self.base)?;
        std::fs::create_dir_all(&self.memory_dir)?;
        std::fs::create_dir_all(&self.skills_dir)?;
        std::fs::create_dir_all(&self.deps_dir)?;
        std::fs::create_dir_all(&self.claude_home)?;

        // Create default PERSONA.md if it doesn't exist
        let persona_path = self.base.join("PERSONA.md");
        if !persona_path.exists() {
            let content = r#"# PERSONA.md - Persona & Boundaries

## Tone & Style
- Keep replies concise and direct.
- Ask clarifying questions when needed.
- Be helpful but honest about limitations.

## Capabilities
You are a personal assistant running on the user's machine. You can:
- Answer questions and have conversations
- Help with writing, brainstorming, and thinking through problems

You do NOT have direct access to:
- Calendars, email, or external services
- The user's files or system (unless given explicit access)
- Real-time information

## Skills
When the user asks for something you can't do directly, suggest creating a **skill** for it.
Skills are custom extensions that live in the skills/ folder. Each skill has:
- A SKILL.md file describing what it does
- Optional scripts to execute actions

Example: "I can't access your calendar directly, but we could create a calendar skill that connects to your calendar service. Want me to help set that up?"
"#;
            std::fs::write(&persona_path, content)?;
        }

        Ok(())
    }
}

fn default_true() -> bool {
    true
}

// ============================================================================
// Config Types
// ============================================================================

/// Which AI backend to use
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum AiBackend {
    #[default]
    Claude,
    Cursor,
}

/// Which durable state store to use (none = all-local, today's behavior).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum StoreKind {
    Filesystem,
    S3,
}

/// Where a turn executes (none/local = in-process; subprocess = one-shot worker).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ProviderKind {
    Local,
    Subprocess,
    Docker,
}

/// S3 state-store settings (used when `store = "s3"`). Credentials come from the
/// standard AWS provider chain (env / instance role), never config.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct S3Config {
    /// Bucket name (required).
    pub bucket: String,
    /// AWS region; falls back to the default chain when unset.
    #[serde(default)]
    pub region: Option<String>,
    /// Optional key namespace within the bucket.
    #[serde(default)]
    pub prefix: Option<String>,
    /// Optional endpoint override (LocalStack / MinIO / testing).
    #[serde(default)]
    pub endpoint: Option<String>,
}

/// Distributed-deployment configuration. All optional; absent = single-box.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DeploymentConfig {
    /// State store backend. `None` disables hydration (default).
    #[serde(default)]
    pub store: Option<StoreKind>,
    /// Filesystem store root. Defaults to `internal/state-store` when unset.
    #[serde(default)]
    pub state_path: Option<String>,
    /// Turn execution mode. `None` (or `Local`) = in-process (default).
    #[serde(default)]
    pub provider: Option<ProviderKind>,
    /// Worker image for `provider = "docker"` (default `cica-worker:latest`).
    #[serde(default)]
    pub docker_image: Option<String>,
    /// S3 store settings (used when `store = "s3"`).
    #[serde(default)]
    pub s3: Option<S3Config>,
}

/// Root configuration
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Config {
    #[serde(default)]
    pub channels: ChannelsConfig,

    #[serde(default)]
    pub claude: ClaudeConfig,

    #[serde(default)]
    pub cursor: CursorConfig,

    /// Which AI backend to use (claude or cursor)
    #[serde(default)]
    pub backend: AiBackend,

    /// Distributed-deployment settings (state store, etc.)
    #[serde(default)]
    pub deployment: DeploymentConfig,

    /// Enable audit logging of conversations and system events (default: true)
    #[serde(default = "default_true")]
    pub audit: bool,

    /// Global onboarding prompt (can be overridden per channel)
    pub onboarding_prompt: Option<String>,
}

/// All channel configurations
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ChannelsConfig {
    pub telegram: Option<TelegramConfig>,
    pub signal: Option<SignalConfig>,
    pub slack: Option<SlackConfig>,
}

/// Telegram-specific configuration
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TelegramConfig {
    #[serde(default)]
    pub bot_token: String,
    #[serde(default)]
    pub auto_approve: bool,
    #[serde(default)]
    pub shared_identity: bool,
    pub onboarding_prompt: Option<String>,
}

impl TelegramConfig {
    pub fn new(bot_token: String) -> Self {
        Self {
            bot_token,
            ..Default::default()
        }
    }
}

/// Signal-specific configuration
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SignalConfig {
    #[serde(default)]
    pub phone_number: String,
    #[serde(default)]
    pub auto_approve: bool,
    #[serde(default)]
    pub shared_identity: bool,
    pub onboarding_prompt: Option<String>,
}

impl SignalConfig {
    pub fn new(phone_number: String) -> Self {
        Self {
            phone_number,
            ..Default::default()
        }
    }
}

/// Slack-specific configuration
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SlackConfig {
    #[serde(default)]
    pub bot_token: String,
    #[serde(default)]
    pub app_token: String,
    #[serde(default)]
    pub auto_approve: bool,
    #[serde(default)]
    pub shared_identity: bool,
    pub onboarding_prompt: Option<String>,
    /// Allow Slack to unfurl (preview) links in bot messages (default: false)
    #[serde(default)]
    pub unfurl_links: bool,
}

impl SlackConfig {
    pub fn new(bot_token: String, app_token: String) -> Self {
        Self {
            bot_token,
            app_token,
            ..Default::default()
        }
    }
}

/// Channel settings relevant to pairing/onboarding
#[derive(Debug, Clone, Default)]
pub struct ChannelSettings {
    pub auto_approve: bool,
    pub shared_identity: bool,
    pub onboarding_prompt: Option<String>,
}

impl Config {
    pub fn channel_settings(&self, channel: &str) -> ChannelSettings {
        let global_prompt = self.onboarding_prompt.clone();

        match channel {
            "telegram" => self
                .channels
                .telegram
                .as_ref()
                .map(|c| ChannelSettings {
                    auto_approve: c.auto_approve,
                    shared_identity: c.shared_identity,
                    onboarding_prompt: c.onboarding_prompt.clone().or(global_prompt.clone()),
                })
                .unwrap_or_default(),
            "signal" => self
                .channels
                .signal
                .as_ref()
                .map(|c| ChannelSettings {
                    auto_approve: c.auto_approve,
                    shared_identity: c.shared_identity,
                    onboarding_prompt: c.onboarding_prompt.clone().or(global_prompt.clone()),
                })
                .unwrap_or_default(),
            "slack" => self
                .channels
                .slack
                .as_ref()
                .map(|c| ChannelSettings {
                    auto_approve: c.auto_approve,
                    shared_identity: c.shared_identity,
                    onboarding_prompt: c.onboarding_prompt.clone().or(global_prompt.clone()),
                })
                .unwrap_or_default(),
            _ => ChannelSettings::default(),
        }
    }
}

/// Claude configuration
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ClaudeConfig {
    /// Anthropic API key or OAuth token (used when not using Vertex AI)
    pub api_key: Option<String>,
    /// Model to use: an alias ("sonnet", "opus") or full model ID from the API (e.g. "claude-sonnet-4-5-20250929")
    pub model: Option<String>,
    /// Use Google Vertex AI instead of Anthropic API
    #[serde(default)]
    pub use_vertex: bool,
    /// GCP project ID for Vertex AI (required when use_vertex is true)
    pub vertex_project_id: Option<String>,
    /// GCP region for Vertex AI (e.g. "europe-west1", "us-east5"). Defaults to "europe-west1" if unset.
    pub vertex_region: Option<String>,
    /// Path to GCP service account JSON key file (long-lived auth; recommended for servers).
    /// When set, GOOGLE_APPLICATION_CREDENTIALS is set for Claude so gcloud login is not needed.
    pub vertex_credentials_path: Option<String>,
}

/// Cursor CLI configuration
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CursorConfig {
    /// Cursor API key (from dashboard)
    pub api_key: Option<String>,
    /// Model to use (default: claude-sonnet-4-20250514)
    pub model: Option<String>,
}

// ============================================================================
// Config Operations
// ============================================================================

impl Config {
    /// Load config from the standard location
    pub fn load() -> Result<Self> {
        let path = paths()?.config_file;

        let content = std::fs::read_to_string(&path)
            .with_context(|| format!("Could not read config file: {:?}", path))?;

        let mut config: Config = toml::from_str(&content)
            .with_context(|| format!("Could not parse config file: {:?}", path))?;
        config.apply_env_overlay();
        Ok(config)
    }

    /// Overlay credential secrets from the process environment onto the loaded
    /// config. Lets cloud workers receive secrets via env (Secrets Manager →
    /// task env) instead of baking them into config.toml or the state store.
    pub(crate) fn apply_env_overlay(&mut self) {
        self.overlay_secrets_from(|k| std::env::var(k).ok());
    }

    /// Env overlay core, parameterized by a lookup so it is testable without
    /// touching the global process environment.
    fn overlay_secrets_from(&mut self, get: impl Fn(&str) -> Option<String>) {
        if let Some(v) = get("CICA_CURSOR_API_KEY") {
            self.cursor.api_key = Some(v);
        }
        if let Some(v) = get("CICA_CLAUDE_API_KEY") {
            self.claude.api_key = Some(v);
        }
    }

    /// Save config to the standard location
    pub fn save(&self) -> Result<()> {
        let paths = paths()?;
        paths.ensure_dirs()?;

        let content = toml::to_string_pretty(self)?;
        std::fs::write(&paths.config_file, content)?;

        Ok(())
    }

    /// Check if config file exists
    pub fn exists() -> Result<bool> {
        Ok(paths()?.config_file.exists())
    }

    /// Get list of configured channel names
    pub fn configured_channels(&self) -> Vec<&'static str> {
        let mut channels = Vec::new();

        if self.channels.telegram.is_some() {
            channels.push("telegram");
        }
        if self.channels.signal.is_some() {
            channels.push("signal");
        }
        if self.channels.slack.is_some() {
            channels.push("slack");
        }

        channels
    }

    /// Check if Claude is configured (Anthropic API key or Vertex AI)
    pub fn is_claude_configured(&self) -> bool {
        if self.claude.use_vertex {
            self.claude
                .vertex_project_id
                .as_ref()
                .is_some_and(|s| !s.is_empty())
        } else {
            self.claude.api_key.is_some()
        }
    }

    /// Check if Cursor is configured
    pub fn is_cursor_configured(&self) -> bool {
        self.cursor.api_key.is_some()
    }

    /// Check if the selected backend is configured
    pub fn is_backend_configured(&self) -> bool {
        match self.backend {
            AiBackend::Claude => self.is_claude_configured(),
            AiBackend::Cursor => self.is_cursor_configured(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deployment_defaults_to_no_store() {
        let cfg = Config::default();
        assert!(cfg.deployment.store.is_none());
    }

    #[test]
    fn deployment_parses_filesystem_store() {
        let toml = r#"
            backend = "claude"
            [deployment]
            store = "filesystem"
            state_path = "/tmp/cica-state"
        "#;
        let cfg: Config = toml::from_str(toml).unwrap();
        assert_eq!(cfg.deployment.store, Some(StoreKind::Filesystem));
        assert_eq!(
            cfg.deployment.state_path.as_deref(),
            Some("/tmp/cica-state")
        );
    }

    #[test]
    fn provider_defaults_to_none() {
        let cfg = Config::default();
        assert!(cfg.deployment.provider.is_none());
    }

    #[test]
    fn provider_parses_subprocess() {
        let toml = r#"
            [deployment]
            provider = "subprocess"
            store = "filesystem"
        "#;
        let cfg: Config = toml::from_str(toml).unwrap();
        assert_eq!(cfg.deployment.provider, Some(ProviderKind::Subprocess));
    }

    #[test]
    fn provider_parses_docker_with_image() {
        let toml = r#"
            [deployment]
            provider = "docker"
            store = "filesystem"
            docker_image = "cica-worker:dev"
        "#;
        let cfg: Config = toml::from_str(toml).unwrap();
        assert_eq!(cfg.deployment.provider, Some(ProviderKind::Docker));
        assert_eq!(
            cfg.deployment.docker_image.as_deref(),
            Some("cica-worker:dev")
        );
    }

    #[test]
    fn store_parses_s3() {
        let toml = r#"
            [deployment]
            store = "s3"
        "#;
        let cfg: Config = toml::from_str(toml).unwrap();
        assert_eq!(cfg.deployment.store, Some(StoreKind::S3));
    }

    #[test]
    fn deployment_s3_section_parses() {
        let toml = r#"
            [deployment]
            [deployment.s3]
            bucket = "cica-state"
            region = "eu-west-1"
            prefix = "cica"
            endpoint = "http://localhost:4566"
        "#;
        let cfg: Config = toml::from_str(toml).unwrap();
        let s3 = cfg.deployment.s3.unwrap();
        assert_eq!(s3.bucket, "cica-state");
        assert_eq!(s3.region.as_deref(), Some("eu-west-1"));
        assert_eq!(s3.prefix.as_deref(), Some("cica"));
        assert_eq!(s3.endpoint.as_deref(), Some("http://localhost:4566"));
    }

    #[test]
    fn env_overlay_sets_cursor_and_claude_keys() {
        let mut cfg = Config::default();
        assert!(cfg.cursor.api_key.is_none());
        let env = |k: &str| match k {
            "CICA_CURSOR_API_KEY" => Some("cur-secret".to_string()),
            "CICA_CLAUDE_API_KEY" => Some("claude-secret".to_string()),
            _ => None,
        };
        cfg.overlay_secrets_from(env);
        assert_eq!(cfg.cursor.api_key.as_deref(), Some("cur-secret"));
        assert_eq!(cfg.claude.api_key.as_deref(), Some("claude-secret"));
    }

    #[test]
    fn env_overlay_leaves_config_value_when_env_absent() {
        let mut cfg = Config::default();
        cfg.cursor.api_key = Some("from-file".into());
        cfg.overlay_secrets_from(|_| None);
        assert_eq!(cfg.cursor.api_key.as_deref(), Some("from-file"));
    }
}
