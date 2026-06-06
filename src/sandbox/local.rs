//! Local-subprocess sandbox provider (Phase 1: today's behavior).

use anyhow::Result;
use async_trait::async_trait;

use crate::backends::{self, QueryResult};
use crate::sandbox::{SandboxProvider, TurnJob, TurnResult};

/// Runs an agent turn in a local subprocess (today's behavior).
#[derive(Default)]
pub struct LocalProcessProvider;

impl LocalProcessProvider {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl SandboxProvider for LocalProcessProvider {
    async fn run_turn(&self, job: TurnJob) -> Result<TurnResult> {
        let options = job_to_query_options(&job);
        let qr = backends::query_with_options(&job.prompt, options).await?;
        Ok(turn_result_from_query(qr))
    }
}

fn job_to_query_options(job: &TurnJob) -> backends::QueryOptions {
    backends::QueryOptions {
        system_prompt: job.system_prompt.clone(),
        resume_session: job.resume_session.clone(),
        cwd: job.cwd.clone(),
        skip_permissions: job.skip_permissions,
    }
}

pub(crate) fn turn_result_from_query(qr: QueryResult) -> TurnResult {
    TurnResult {
        response: qr.response,
        backend_session_id: qr.session_id,
        cost_usd: qr.cost_usd,
        duration_ms: qr.duration_ms,
    }
}

pub fn query_result_from_turn(tr: TurnResult) -> QueryResult {
    QueryResult {
        response: tr.response,
        session_id: tr.backend_session_id,
        duration_ms: tr.duration_ms,
        cost_usd: tr.cost_usd,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AiBackend;

    fn sample_job() -> TurnJob {
        TurnJob {
            session_id: "telegram:42".into(),
            channel: "telegram".into(),
            user_id: "42".into(),
            prompt: "hello".into(),
            system_prompt: Some("ctx".into()),
            resume_session: Some("sess-1".into()),
            cwd: Some("/tmp/work".into()),
            skip_permissions: true,
            backend: AiBackend::Claude,
            model: Some("claude-opus-4-6".into()),
        }
    }

    #[test]
    fn job_maps_to_query_options() {
        let job = sample_job();
        let opts = job_to_query_options(&job);
        assert_eq!(opts.system_prompt.as_deref(), Some("ctx"));
        assert_eq!(opts.resume_session.as_deref(), Some("sess-1"));
        assert_eq!(opts.cwd.as_deref(), Some("/tmp/work"));
        assert!(opts.skip_permissions);
    }

    #[test]
    fn query_result_maps_to_turn_result() {
        let qr = QueryResult {
            response: "hi".into(),
            session_id: "sess-9".into(),
            duration_ms: Some(123),
            cost_usd: Some(0.5),
        };
        let tr = turn_result_from_query(qr);
        assert_eq!(tr.response, "hi");
        assert_eq!(tr.backend_session_id, "sess-9");
        assert_eq!(tr.duration_ms, Some(123));
        assert_eq!(tr.cost_usd, Some(0.5));
    }

    #[test]
    fn turn_result_maps_back_to_query_result() {
        let tr = TurnResult {
            response: "yo".into(),
            backend_session_id: "sess-3".into(),
            cost_usd: None,
            duration_ms: None,
        };
        let qr = query_result_from_turn(tr);
        assert_eq!(qr.response, "yo");
        assert_eq!(qr.session_id, "sess-3");
    }

    #[test]
    fn provider_is_constructible_and_object_safe() {
        let p = LocalProcessProvider::new();
        let _boxed: Box<dyn crate::sandbox::SandboxProvider> = Box::new(p);
    }
}
