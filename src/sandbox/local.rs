//! Local-subprocess sandbox provider (Phase 1: today's behavior).

use anyhow::Result;
use async_trait::async_trait;

use crate::backends::{self, QueryResult};
use crate::sandbox::{SandboxProvider, TurnJob, TurnResult};

/// Runs an agent turn in a local subprocess (today's behavior).
pub struct LocalProcessProvider;

impl LocalProcessProvider {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl SandboxProvider for LocalProcessProvider {
    async fn run_turn(&self, _job: TurnJob) -> Result<TurnResult> {
        todo!("LocalProcessProvider::run_turn implemented in a later phase")
    }
}

/// Map a `TurnJob` to the backend-agnostic `QueryOptions`.
fn job_to_query_options(job: &TurnJob) -> backends::QueryOptions {
    backends::QueryOptions {
        system_prompt: job.system_prompt.clone(),
        resume_session: job.resume_session.clone(),
        cwd: job.cwd.clone(),
        skip_permissions: job.skip_permissions,
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
}
