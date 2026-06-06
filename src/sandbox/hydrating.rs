//! A `SandboxProvider` decorator that hydrates durable state before a turn
//! and dehydrates it after, via a `StateStore`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use tracing::warn;

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

        // Skills: published, read-only — pull the current set so the agent can
        // read/execute them. Absence (router hasn't synced yet) is fine.
        let _ = self.store.pull("skills", &self.cwd.join("skills")).await;

        // --- Run ---
        let result = self.inner.run_turn(job).await?;

        // --- Dehydrate (best-effort) ---
        // Persisting session/memory must NOT drop the turn's reply: the worker
        // returns `result` to the router *after* this, so a failed push here
        // (e.g. a slow S3 upload timing out) would otherwise lose the answer
        // entirely. Log and continue; the worst case is a degraded resume.
        if !result.backend_session_id.is_empty() {
            let bid = &result.backend_session_id;
            let staging = self.staging();
            match artifacts.capture(home, bid, &staging) {
                Ok(true) => {
                    if let Err(e) = self.store.push(&staging, &format!("session/{bid}")).await {
                        warn!("failed to persist session {bid} (reply still delivered): {e}");
                    }
                }
                Ok(false) => {}
                Err(e) => warn!("failed to capture session {bid} artifacts: {e}"),
            }
            let _ = std::fs::remove_dir_all(&staging);
        }
        if mem_dir.exists()
            && let Err(e) = self.store.push(&mem_dir, &mem_key).await
        {
            warn!("failed to persist memories (reply still delivered): {e}");
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

    /// A store whose `push` always fails (simulates a stalled S3 upload),
    /// while `pull` is a no-op. Used to prove dehydration is best-effort.
    struct FailingPushStore;

    #[async_trait]
    impl StateStore for FailingPushStore {
        async fn pull(&self, _key: &str, _dest: &Path) -> Result<bool> {
            Ok(false)
        }
        async fn push(&self, _src: &Path, _key: &str) -> Result<()> {
            anyhow::bail!("simulated S3 put timeout")
        }
    }

    #[tokio::test]
    async fn dehydrate_failure_does_not_drop_the_reply() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().join("cwd");
        // Seed a memory so the dehydrate path attempts a (failing) push.
        write(&cwd.join("users/telegram_1/memories/m.md"), "note");
        let hp = HydratingProvider::new(
            StubProvider {
                session_id: String::new(),
                seen: Mutex::new(None),
            },
            Arc::new(FailingPushStore),
            tmp.path().join("claude"),
            tmp.path().join("cursor"),
            cwd,
        );
        // The push fails, but the turn's reply must still be returned.
        let result = hp
            .run_turn(job(None))
            .await
            .expect("reply delivered despite a dehydration push failure");
        assert_eq!(result.response, "ok");
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
    async fn hydrate_pulls_published_skills() {
        let tmp = tempfile::tempdir().unwrap();

        // Seed the store's "skills" prefix with one skill.
        let seed = tmp.path().join("seed");
        write(&seed.join("foo/SKILL.md"), "name: foo");
        let store = Arc::new(FilesystemStateStore::new(tmp.path().join("store")));
        store.push(&seed, "skills").await.unwrap();

        // cwd stands in for /data/cica; skills land in cwd/skills.
        let cwd = tmp.path().join("cwd");
        std::fs::create_dir_all(&cwd).unwrap();

        let hp = HydratingProvider::new(
            // Empty session id => no dehydrate/push-back, keeps the test focused.
            StubProvider {
                session_id: String::new(),
                seen: Mutex::new(None),
            },
            store,
            tmp.path().join("claude"),
            tmp.path().join("cursor"),
            cwd.clone(),
        );

        hp.run_turn(job(None)).await.unwrap();

        assert!(cwd.join("skills/foo/SKILL.md").exists());
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
