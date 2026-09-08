//! Local-subprocess sandbox provider (Phase 1: today's behavior).

use anyhow::Result;
use async_trait::async_trait;

use crate::backends::{self, QueryResult};
use crate::config::{Config, Paths};
use crate::sandbox::{SandboxProvider, TurnJob, TurnResult};

/// Runs an agent turn in a local subprocess (today's behavior).
pub struct LocalProcessProvider {
    config: Config,
    paths: Paths,
}

impl LocalProcessProvider {
    pub fn new(config: Config, paths: Paths) -> Self {
        Self { config, paths }
    }
}

#[async_trait]
impl SandboxProvider for LocalProcessProvider {
    async fn run_turn(&self, job: TurnJob) -> Result<TurnResult> {
        // Make sure the per-user memories dir exists so the agent can write into it.
        let dir = crate::memory::memories_dir(&self.paths, &job.channel, &job.user_id);
        let _ = std::fs::create_dir_all(&dir);
        let options = job_to_query_options(&self.paths, &job);
        let qr = backends::query_with_options(
            job.backend,
            &self.config,
            &self.paths,
            &job.prompt,
            options,
        )
        .await?;
        Ok(turn_result_from_query(qr))
    }
}

/// Resolve `{MEMORIES_DIR}` in the system prompt to the given local memories
/// path. Token absent → prompt returned unchanged; `None` prompt → `None`.
fn substitute_token(system_prompt: Option<&str>, memories_dir: &std::path::Path) -> Option<String> {
    let sp = system_prompt?;
    Some(sp.replace(
        crate::memory::MEMORIES_DIR_TOKEN,
        &memories_dir.to_string_lossy(),
    ))
}

fn job_to_query_options(paths: &Paths, job: &TurnJob) -> backends::QueryOptions {
    let dir = crate::memory::memories_dir(paths, &job.channel, &job.user_id);
    let system_prompt = substitute_token(job.system_prompt.as_deref(), &dir);
    backends::QueryOptions {
        system_prompt,
        resume_session: job.resume_session.clone(),
        skip_permissions: job.skip_permissions,
        model: job.model.clone(),
    }
}

pub(crate) fn turn_result_from_query(qr: QueryResult) -> TurnResult {
    TurnResult {
        response: qr.response,
        backend_session_id: qr.session_id,
        cost_usd: qr.cost_usd,
        duration_ms: qr.duration_ms,
        tokens: qr.tokens,
        // A local run writes files where the channel can already read them.
        produced_files: Vec::new(),
    }
}

pub fn query_result_from_turn(tr: TurnResult) -> QueryResult {
    QueryResult {
        response: tr.response,
        session_id: tr.backend_session_id,
        duration_ms: tr.duration_ms,
        cost_usd: tr.cost_usd,
        tokens: tr.tokens,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backends::TokenUsage;
    use crate::config::AiBackend;
    use std::path::Path;

    fn sample_job() -> TurnJob {
        TurnJob {
            channel: "telegram".into(),
            user_id: "42".into(),
            affinity: crate::sandbox::Affinity::Chat {
                channel: "telegram".into(),
                user: "42".into(),
            },
            session_persistence: crate::sandbox::SessionPersistence::Resume,
            prompt: "hello".into(),
            system_prompt: Some("ctx".into()),
            resume_session: Some("sess-1".into()),
            skip_permissions: true,
            backend: AiBackend::Claude,
            model: Some("claude-opus-4-6".into()),
            attachments: Vec::new(),
        }
    }

    #[test]
    fn job_maps_to_query_options() {
        let (_temp, paths) = crate::config::test_paths();
        let job = sample_job();
        let opts = job_to_query_options(&paths, &job);
        assert_eq!(opts.system_prompt.as_deref(), Some("ctx"));
        assert_eq!(opts.resume_session.as_deref(), Some("sess-1"));
        assert!(opts.skip_permissions);
        assert_eq!(opts.model.as_deref(), Some("claude-opus-4-6"));
    }

    #[test]
    fn query_result_maps_to_turn_result() {
        let qr = QueryResult {
            response: "hi".into(),
            session_id: "sess-9".into(),
            duration_ms: Some(123),
            cost_usd: Some(0.5),
            tokens: Some(TokenUsage {
                input: 20,
                output: 4,
                cache_read: 100,
                cache_write: 50,
            }),
        };
        let tr = turn_result_from_query(qr);
        assert_eq!(tr.response, "hi");
        assert_eq!(tr.backend_session_id, "sess-9");
        assert_eq!(tr.duration_ms, Some(123));
        assert_eq!(tr.cost_usd, Some(0.5));
        assert_eq!(
            tr.tokens,
            Some(TokenUsage {
                input: 20,
                output: 4,
                cache_read: 100,
                cache_write: 50,
            })
        );
    }

    #[test]
    fn turn_result_maps_back_to_query_result() {
        let tr = TurnResult {
            response: "yo".into(),
            backend_session_id: "sess-3".into(),
            cost_usd: None,
            duration_ms: None,
            produced_files: Vec::new(),
            tokens: None,
        };
        let qr = query_result_from_turn(tr);
        assert_eq!(qr.response, "yo");
        assert_eq!(qr.session_id, "sess-3");
    }

    #[test]
    fn provider_is_constructible_and_object_safe() {
        let (_temp, paths) = crate::config::test_paths();
        let p = LocalProcessProvider::new(Config::default(), paths);
        let _boxed: Box<dyn crate::sandbox::SandboxProvider> = Box::new(p);
    }

    #[test]
    fn substitutes_memories_token_when_present() {
        let out = substitute_token(
            Some("save to {MEMORIES_DIR}/x.md please"),
            Path::new("/data/cica/users/telegram_1/memories"),
        );
        assert_eq!(
            out.as_deref(),
            Some("save to /data/cica/users/telegram_1/memories/x.md please")
        );
    }

    #[test]
    fn leaves_prompt_unchanged_when_token_absent() {
        let out = substitute_token(Some("no token here"), Path::new("/m"));
        assert_eq!(out.as_deref(), Some("no token here"));
    }

    #[test]
    fn none_prompt_stays_none() {
        let out = substitute_token(None, Path::new("/m"));
        assert_eq!(out, None);
    }

    #[test]
    fn job_options_substitutes_memories_token() {
        let (_temp, paths) = crate::config::test_paths();
        let mut job = sample_job();
        job.system_prompt = Some("write to {MEMORIES_DIR}/notes.md".into());
        let opts = job_to_query_options(&paths, &job);
        let sp = opts.system_prompt.unwrap();
        assert!(!sp.contains("{MEMORIES_DIR}"));
        assert!(sp.contains("/notes.md"));
    }

    #[tokio::test]
    async fn job_backend_selects_cursor_on_a_claude_config() {
        let (_temp, paths) = crate::config::test_paths();
        let mut cfg = Config {
            backend: AiBackend::Claude,
            ..Default::default()
        };
        cfg.claude.api_key = Some("k".into());
        cfg.cursor.api_key = None;
        let provider = LocalProcessProvider::new(cfg.clone(), paths);
        let mut job = TurnJob::new(
            &cfg,
            "telegram",
            "1",
            crate::sandbox::Affinity::Chat {
                channel: "telegram".into(),
                user: "1".into(),
            },
            "hi".into(),
            None,
            None,
        );
        job.backend = AiBackend::Cursor;
        let error = provider.run_turn(job).await.unwrap_err().to_string();
        assert!(error.contains("No Cursor API key configured"));
        assert!(!error.contains("Claude"));
    }

    #[tokio::test]
    async fn job_backend_selects_claude_on_a_cursor_config() {
        let (_temp, paths) = crate::config::test_paths();
        let mut cfg = Config {
            backend: AiBackend::Cursor,
            ..Default::default()
        };
        cfg.cursor.api_key = Some("k".into());
        cfg.claude.api_key = None;
        let provider = LocalProcessProvider::new(cfg.clone(), paths);
        let mut job = TurnJob::new(
            &cfg,
            "telegram",
            "1",
            crate::sandbox::Affinity::Chat {
                channel: "telegram".into(),
                user: "1".into(),
            },
            "hi".into(),
            None,
            None,
        );
        job.backend = AiBackend::Claude;
        let error = provider.run_turn(job).await.unwrap_err().to_string();
        assert!(error.contains("No credential configured"));
    }

    #[test]
    fn job_model_reaches_query_options() {
        let (_temp, paths) = crate::config::test_paths();
        let mut job = sample_job();
        job.model = Some("claude-opus-4-6".into());
        assert_eq!(
            job_to_query_options(&paths, &job).model.as_deref(),
            Some("claude-opus-4-6")
        );
        job.model = None;
        assert_eq!(job_to_query_options(&paths, &job).model, None);
    }
}
