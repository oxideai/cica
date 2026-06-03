//! A `SandboxProvider` decorator that hydrates durable state before a turn
//! and dehydrates it after, via a `StateStore`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;

use crate::config::AiBackend;
use crate::sandbox::artifacts::{ClaudeSessionArtifacts, CursorSessionArtifacts, SessionArtifacts};
use crate::sandbox::state::StateStore;
use crate::sandbox::{SandboxProvider, TurnJob, TurnResult};

/// Wraps an inner provider: pull session + memories → run → capture + push.
pub struct HydratingProvider<P: SandboxProvider> {
    inner: P,
    store: Arc<dyn StateStore>,
    claude_home: PathBuf,
    cursor_home: PathBuf,
    /// Effective working directory of the agent subprocess (used for the slug/hash).
    cwd: PathBuf,
}

impl<P: SandboxProvider> HydratingProvider<P> {
    pub fn new(
        inner: P,
        store: Arc<dyn StateStore>,
        claude_home: PathBuf,
        cursor_home: PathBuf,
        cwd: PathBuf,
    ) -> Self {
        Self {
            inner,
            store,
            claude_home,
            cursor_home,
            cwd,
        }
    }

    fn memories_dir(&self, channel: &str, user_id: &str) -> PathBuf {
        self.cwd
            .join("users")
            .join(format!("{channel}_{user_id}"))
            .join("memories")
    }

    fn staging(&self) -> PathBuf {
        std::env::temp_dir().join(format!("cica-hydrate-{}", uuid::Uuid::new_v4()))
    }
}

#[async_trait]
impl<P: SandboxProvider> SandboxProvider for HydratingProvider<P> {
    async fn run_turn(&self, job: TurnJob) -> Result<TurnResult> {
        let mem_key = format!("mem/{}_{}", job.channel, job.user_id);
        let mem_dir = self.memories_dir(&job.channel, &job.user_id);

        // Select the backend's artifact handler and HOME dir.
        let (artifacts, home): (Box<dyn SessionArtifacts + Send>, &Path) = match job.backend {
            AiBackend::Claude => (Box::new(ClaudeSessionArtifacts), self.claude_home.as_path()),
            AiBackend::Cursor => (Box::new(CursorSessionArtifacts), self.cursor_home.as_path()),
        };

        // --- Hydrate ---
        if let Some(bid) = &job.resume_session {
            let staging = self.staging();
            if self.store.pull(&format!("session/{bid}"), &staging).await? {
                artifacts.restore(home, &self.cwd, bid, &staging)?;
            }
            let _ = std::fs::remove_dir_all(&staging);
        }
        // Memories: pull is authoritative when present; absent = keep local.
        let _ = self.store.pull(&mem_key, &mem_dir).await?;

        // --- Run ---
        let result = self.inner.run_turn(job).await?;

        // --- Dehydrate ---
        if !result.backend_session_id.is_empty() {
            let bid = &result.backend_session_id;
            let staging = self.staging();
            if artifacts.capture(home, bid, &staging)? {
                self.store.push(&staging, &format!("session/{bid}")).await?;
            }
            let _ = std::fs::remove_dir_all(&staging);
        }
        if mem_dir.exists() {
            self.store.push(&mem_dir, &mem_key).await?;
        }

        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::sync::Mutex;

    use crate::sandbox::state::FilesystemStateStore;

    /// Inner provider that records the job and returns a fixed session id.
    struct StubProvider {
        session_id: String,
        seen: Mutex<Option<TurnJob>>,
    }

    #[async_trait]
    impl SandboxProvider for StubProvider {
        async fn run_turn(&self, job: TurnJob) -> Result<TurnResult> {
            *self.seen.lock().unwrap() = Some(job);
            Ok(TurnResult {
                response: "ok".into(),
                backend_session_id: self.session_id.clone(),
                cost_usd: None,
                duration_ms: None,
            })
        }
    }

    fn job(resume: Option<&str>) -> TurnJob {
        TurnJob {
            session_id: "telegram:1".into(),
            channel: "telegram".into(),
            user_id: "1".into(),
            prompt: "hi".into(),
            system_prompt: None,
            resume_session: resume.map(|s| s.to_string()),
            cwd: None,
            skip_permissions: true,
            backend: AiBackend::Claude,
            model: None,
        }
    }

    fn write(path: &Path, contents: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }

    #[tokio::test]
    async fn dehydrate_captures_and_pushes_result_session() {
        let store_root = tempfile::tempdir().unwrap();
        let claude_home = tempfile::tempdir().unwrap();
        let cursor_home = tempfile::tempdir().unwrap();
        let base = tempfile::tempdir().unwrap();
        let store = Arc::new(FilesystemStateStore::new(store_root.path().to_path_buf()));

        let id = "sess-new";
        let slug = crate::sandbox::artifacts::claude_project_slug(base.path());
        write(
            &claude_home
                .path()
                .join(".claude")
                .join("projects")
                .join(&slug)
                .join(format!("{id}.jsonl")),
            "turn1\n",
        );

        let inner = StubProvider {
            session_id: id.into(),
            seen: Mutex::new(None),
        };
        let hp = HydratingProvider::new(
            inner,
            store.clone(),
            claude_home.path().to_path_buf(),
            cursor_home.path().to_path_buf(),
            base.path().to_path_buf(),
        );
        hp.run_turn(job(None)).await.unwrap();

        let dest = tempfile::tempdir().unwrap();
        assert!(
            store
                .pull(&format!("session/{id}"), dest.path())
                .await
                .unwrap()
        );
        assert_eq!(
            std::fs::read_to_string(dest.path().join("transcript.jsonl")).unwrap(),
            "turn1\n"
        );
    }

    #[tokio::test]
    async fn hydrate_restores_resumed_session() {
        let store_root = tempfile::tempdir().unwrap();
        let claude_home = tempfile::tempdir().unwrap();
        let cursor_home = tempfile::tempdir().unwrap();
        let base = tempfile::tempdir().unwrap();
        let store = Arc::new(FilesystemStateStore::new(store_root.path().to_path_buf()));

        let id = "sess-old";
        let staged = tempfile::tempdir().unwrap();
        write(&staged.path().join("transcript.jsonl"), "history\n");
        store
            .push(staged.path(), &format!("session/{id}"))
            .await
            .unwrap();

        let inner = StubProvider {
            session_id: id.into(),
            seen: Mutex::new(None),
        };
        let hp = HydratingProvider::new(
            inner,
            store,
            claude_home.path().to_path_buf(),
            cursor_home.path().to_path_buf(),
            base.path().to_path_buf(),
        );
        hp.run_turn(job(Some(id))).await.unwrap();

        let slug = crate::sandbox::artifacts::claude_project_slug(base.path());
        let restored = claude_home
            .path()
            .join(".claude")
            .join("projects")
            .join(&slug)
            .join(format!("{id}.jsonl"));
        assert_eq!(std::fs::read_to_string(restored).unwrap(), "history\n");
    }

    #[tokio::test]
    async fn memories_round_trip() {
        let store_root = tempfile::tempdir().unwrap();
        let claude_home = tempfile::tempdir().unwrap();
        let cursor_home = tempfile::tempdir().unwrap();
        let base = tempfile::tempdir().unwrap();
        let store = Arc::new(FilesystemStateStore::new(store_root.path().to_path_buf()));

        let mem_dir = base
            .path()
            .join("users")
            .join("telegram_1")
            .join("memories");
        write(&mem_dir.join("note.md"), "remember this");

        let inner = StubProvider {
            session_id: String::new(),
            seen: Mutex::new(None),
        };
        let hp = HydratingProvider::new(
            inner,
            store.clone(),
            claude_home.path().to_path_buf(),
            cursor_home.path().to_path_buf(),
            base.path().to_path_buf(),
        );
        hp.run_turn(job(None)).await.unwrap();

        let dest = tempfile::tempdir().unwrap();
        assert!(store.pull("mem/telegram_1", dest.path()).await.unwrap());
        assert_eq!(
            std::fs::read_to_string(dest.path().join("note.md")).unwrap(),
            "remember this"
        );
    }

    #[tokio::test]
    async fn cursor_job_captures_session_to_store() {
        let store_root = tempfile::tempdir().unwrap();
        let claude_home = tempfile::tempdir().unwrap();
        let cursor_home = tempfile::tempdir().unwrap();
        let base = tempfile::tempdir().unwrap();
        let store = Arc::new(FilesystemStateStore::new(store_root.path().to_path_buf()));

        // The (stub) cursor turn "produced" a session db on local disk.
        let id = "cursor-sess-1";
        let hash = "deadbeef";
        write(
            &cursor_home
                .path()
                .join(".cursor")
                .join("chats")
                .join(hash)
                .join(id)
                .join("store.db"),
            "CURSORDB",
        );

        let inner = StubProvider {
            session_id: id.into(),
            seen: Mutex::new(None),
        };
        let hp = HydratingProvider::new(
            inner,
            store.clone(),
            claude_home.path().to_path_buf(),
            cursor_home.path().to_path_buf(),
            base.path().to_path_buf(),
        );
        let mut j = job(None);
        j.backend = crate::config::AiBackend::Cursor;
        hp.run_turn(j).await.unwrap();

        let dest = tempfile::tempdir().unwrap();
        assert!(
            store
                .pull(&format!("session/{id}"), dest.path())
                .await
                .unwrap()
        );
        assert_eq!(
            std::fs::read_to_string(dest.path().join(hash).join("store.db")).unwrap(),
            "CURSORDB"
        );
    }
}
