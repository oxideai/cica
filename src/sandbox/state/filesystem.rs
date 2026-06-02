//! Filesystem-backed `StateStore` (Phase 2; also useful for homelab/dev).

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;
use async_trait::async_trait;

use crate::sandbox::state::{StateStore, clear_dir, copy_dir_all, safe_join};

/// Stores each key as a directory tree under `root`.
pub struct FilesystemStateStore {
    root: PathBuf,
}

impl FilesystemStateStore {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }
}

#[async_trait]
impl StateStore for FilesystemStateStore {
    async fn pull(&self, key: &str, dest: &Path) -> Result<bool> {
        let src = safe_join(&self.root, key)?;
        if !src.exists() {
            return Ok(false);
        }
        clear_dir(dest)?;
        copy_dir_all(&src, dest)?;
        Ok(true)
    }

    async fn push(&self, src: &Path, key: &str) -> Result<()> {
        let dst = safe_join(&self.root, key)?;
        if dst.exists() {
            fs::remove_dir_all(&dst)?;
        }
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent)?;
        }
        copy_dir_all(src, &dst)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, contents: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    #[tokio::test]
    async fn pull_absent_key_returns_false() {
        let root = tempfile::tempdir().unwrap();
        let dest = tempfile::tempdir().unwrap();
        let store = FilesystemStateStore::new(root.path().to_path_buf());
        assert!(!store.pull("session/missing", dest.path()).await.unwrap());
    }

    #[tokio::test]
    async fn push_then_pull_round_trips_nested_tree() {
        let root = tempfile::tempdir().unwrap();
        let src = tempfile::tempdir().unwrap();
        write(&src.path().join("a.txt"), "alpha");
        write(&src.path().join("sub/b.txt"), "beta");

        let store = FilesystemStateStore::new(root.path().to_path_buf());
        store.push(src.path(), "session/x").await.unwrap();

        let dest = tempfile::tempdir().unwrap();
        assert!(store.pull("session/x", dest.path()).await.unwrap());
        assert_eq!(fs::read_to_string(dest.path().join("a.txt")).unwrap(), "alpha");
        assert_eq!(fs::read_to_string(dest.path().join("sub/b.txt")).unwrap(), "beta");
    }

    #[tokio::test]
    async fn push_overwrites_prior_contents() {
        let root = tempfile::tempdir().unwrap();
        let store = FilesystemStateStore::new(root.path().to_path_buf());

        let src1 = tempfile::tempdir().unwrap();
        write(&src1.path().join("old.txt"), "old");
        store.push(src1.path(), "k").await.unwrap();

        let src2 = tempfile::tempdir().unwrap();
        write(&src2.path().join("new.txt"), "new");
        store.push(src2.path(), "k").await.unwrap();

        let dest = tempfile::tempdir().unwrap();
        store.pull("k", dest.path()).await.unwrap();
        assert!(!dest.path().join("old.txt").exists());
        assert_eq!(fs::read_to_string(dest.path().join("new.txt")).unwrap(), "new");
    }
}
