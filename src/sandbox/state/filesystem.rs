//! Filesystem-backed `StateStore` (Phase 2; also useful for homelab/dev).

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;
use async_trait::async_trait;

use crate::atomic::Staging;
use crate::sandbox::state::{StateStore, copy_dir_all, safe_join};

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
    async fn get_record(&self, key: &str) -> Result<Option<Vec<u8>>> {
        let path = safe_join(&self.root, key)?;
        match fs::read(path) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    async fn put_record(&self, key: &str, bytes: &[u8]) -> Result<()> {
        crate::atomic::write(&safe_join(&self.root, key)?, bytes)?;
        Ok(())
    }

    async fn delete_record(&self, key: &str) -> Result<()> {
        match fs::remove_file(safe_join(&self.root, key)?) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            result => Ok(result?),
        }
    }

    async fn pull(&self, key: &str, dest: &Path) -> Result<bool> {
        let src = safe_join(&self.root, key)?;
        if !src.exists() {
            return Ok(false);
        }
        let staging = Staging::beside(dest)?;
        copy_dir_all(&src, staging.path())?;
        staging.commit()?;
        Ok(true)
    }

    async fn push(&self, src: &Path, key: &str) -> Result<()> {
        let dst = safe_join(&self.root, key)?;
        let staging = Staging::beside(&dst)?;
        copy_dir_all(src, staging.path())?;
        staging.commit()?;
        Ok(())
    }

    async fn delete(&self, key: &str) -> Result<()> {
        let dst = safe_join(&self.root, key)?;
        match fs::remove_dir_all(dst) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            result => Ok(result?),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::state::contract;

    fn write(path: &Path, contents: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    #[tokio::test]
    async fn push_then_pull_round_trips() {
        let root = tempfile::tempdir().unwrap();
        let store = FilesystemStateStore::new(root.path().to_path_buf());
        contract::push_then_pull_round_trips(&store, "round-trip")
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn push_smaller_tree_removes_stale_files() {
        let root = tempfile::tempdir().unwrap();
        let store = FilesystemStateStore::new(root.path().to_path_buf());
        contract::push_smaller_tree_removes_stale_files(&store, "smaller")
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn push_empty_tree_reads_present_and_empty() {
        let root = tempfile::tempdir().unwrap();
        let store = FilesystemStateStore::new(root.path().to_path_buf());
        contract::push_empty_tree_reads_present_and_empty(&store, "empty")
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn pull_absent_key_leaves_dest_untouched() {
        let root = tempfile::tempdir().unwrap();
        let store = FilesystemStateStore::new(root.path().to_path_buf());
        contract::pull_absent_key_leaves_dest_untouched(&store, "absent")
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn pull_replaces_dest_whole() {
        let root = tempfile::tempdir().unwrap();
        let store = FilesystemStateStore::new(root.path().to_path_buf());
        contract::pull_replaces_dest_whole(&store, "replace")
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn push_failure_leaves_prior_contents() {
        let root = tempfile::tempdir().unwrap();
        let store = FilesystemStateStore::new(root.path().to_path_buf());
        contract::push_failure_leaves_prior_contents(&store, "push-failure")
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn delete_makes_key_absent() {
        let root = tempfile::tempdir().unwrap();
        let store = FilesystemStateStore::new(root.path().to_path_buf());
        contract::delete_makes_key_absent(&store, "delete")
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn delete_absent_key_is_ok() {
        let root = tempfile::tempdir().unwrap();
        let store = FilesystemStateStore::new(root.path().to_path_buf());
        contract::delete_absent_key_is_ok(&store, "absent-delete")
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn pull_failure_leaves_prior_local_contents() {
        let root = tempfile::tempdir().unwrap();
        let store = FilesystemStateStore::new(root.path().to_path_buf());
        let first = tempfile::tempdir().unwrap();
        write(&first.path().join("good.txt"), "good");
        store.push(first.path(), "broken").await.unwrap();
        let dest_parent = tempfile::tempdir().unwrap();
        let dest = dest_parent.path().join("dest");
        store.pull("broken", &dest).await.unwrap();
        write(&root.path().join("broken/new.txt"), "new");
        std::os::unix::fs::symlink("./missing", root.path().join("broken/zz-broken")).unwrap();

        assert!(store.pull("broken", &dest).await.is_err());
        assert_eq!(fs::read_to_string(dest.join("good.txt")).unwrap(), "good");
        assert!(!dest.join("new.txt").exists());
    }

    macro_rules! record_contract_test {
        ($name:ident) => {
            #[tokio::test]
            async fn $name() {
                let root = tempfile::tempdir().unwrap();
                let store = FilesystemStateStore::new(root.path().to_path_buf());
                contract::$name(&store, concat!("records/", stringify!($name)))
                    .await
                    .unwrap();
            }
        };
    }

    record_contract_test!(record_round_trip);
    record_contract_test!(record_absent_is_none);
    record_contract_test!(record_overwrite_replaces);
    record_contract_test!(record_delete_makes_absent);
    record_contract_test!(record_delete_absent_is_ok);
}
