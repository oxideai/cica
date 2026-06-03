//! Durable state storage for sessions and memories.
//!
//! Phase 2 provides only `FilesystemStateStore`. Later phases add
//! feature-gated S3/GCS backends behind the same `StateStore` trait.

pub mod filesystem;

pub use filesystem::FilesystemStateStore;

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Result, bail};
use async_trait::async_trait;

use crate::config::{Config, StoreKind};

/// A durable store of directory trees, addressed by string keys.
///
/// Keys may contain `/` to namespace entries (e.g. `session/<id>`).
#[async_trait]
pub trait StateStore: Send + Sync {
    /// Replace `dest`'s contents with what is stored under `key`.
    /// Returns `false` (and leaves `dest` untouched) if `key` is absent.
    async fn pull(&self, key: &str, dest: &Path) -> Result<bool>;
    /// Store the contents of `src` under `key`, replacing any prior contents.
    async fn push(&self, src: &Path, key: &str) -> Result<()>;
}

/// The filesystem path of the state store: `[deployment].state_path` if set,
/// else `internal/state-store`. Shared so the `FilesystemStateStore` and the
/// Docker host-mount always agree on the same directory.
pub fn resolved_state_path(config: &Config) -> Result<PathBuf> {
    match &config.deployment.state_path {
        Some(p) => Ok(PathBuf::from(p)),
        None => Ok(crate::config::paths()?.internal_dir.join("state-store")),
    }
}

/// Build the configured store, or `None` if deployment.store is unset.
pub fn default_store(config: &Config) -> Result<Option<Arc<dyn StateStore>>> {
    match config.deployment.store {
        None => Ok(None),
        Some(StoreKind::Filesystem) => Ok(Some(Arc::new(FilesystemStateStore::new(
            resolved_state_path(config)?,
        )))),
    }
}

/// Join `key` onto `root`, rejecting `..` and normalizing each segment to
/// path-safe characters. Prevents traversal outside `root`.
pub(crate) fn safe_join(root: &Path, key: &str) -> Result<PathBuf> {
    let mut out = root.to_path_buf();
    for segment in key.split('/') {
        if segment.is_empty() || segment == "." {
            continue;
        }
        if segment == ".." {
            bail!("invalid state key segment: ..");
        }
        let safe: String = segment
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        out.push(safe);
    }
    Ok(out)
}

/// Remove all entries inside `dir` (leaving `dir` itself), creating it if absent.
pub(crate) fn clear_dir(dir: &Path) -> Result<()> {
    if dir.exists() {
        for entry in fs::read_dir(dir)? {
            let path = entry?.path();
            if path.is_dir() {
                fs::remove_dir_all(&path)?;
            } else {
                fs::remove_file(&path)?;
            }
        }
    } else {
        fs::create_dir_all(dir)?;
    }
    Ok(())
}

/// Copy a single path (file or directory) from `src` to `dst`.
pub(crate) fn copy_path(src: &Path, dst: &Path) -> Result<()> {
    if src.is_dir() {
        copy_dir_all(src, dst)
    } else {
        if let Some(parent) = dst.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(src, dst)?;
        Ok(())
    }
}

/// Recursively copy the contents of directory `src` into `dst`.
pub(crate) fn copy_dir_all(src: &Path, dst: &Path) -> Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if from.is_dir() {
            copy_dir_all(&from, &to)?;
        } else {
            fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_join_rejects_traversal() {
        let root = Path::new("/tmp/store");
        assert!(safe_join(root, "../escape").is_err());
        let ok = safe_join(root, "session/abc-123").unwrap();
        assert_eq!(ok, Path::new("/tmp/store/session/abc-123"));
    }

    #[test]
    fn safe_join_sanitizes_segments() {
        let root = Path::new("/tmp/store");
        let p = safe_join(root, "mem/telegram:42").unwrap();
        assert_eq!(p, Path::new("/tmp/store/mem/telegram_42"));
    }

    #[test]
    fn default_store_none_when_unconfigured() {
        let cfg = Config::default();
        assert!(default_store(&cfg).unwrap().is_none());
    }

    #[test]
    fn default_store_some_for_filesystem() {
        let mut cfg = Config::default();
        cfg.deployment.store = Some(StoreKind::Filesystem);
        cfg.deployment.state_path = Some("/tmp/cica-state-test".to_string());
        assert!(default_store(&cfg).unwrap().is_some());
    }
}
