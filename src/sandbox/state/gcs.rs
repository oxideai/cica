//! GCS-backed `StateStore` (feature `gcs`).
//!
//! Mirrors `FilesystemStateStore` (and the sibling `S3StateStore`) semantics
//! over GCS objects keyed `<prefix>/<key>/<relative-file-path>` — the object-key
//! scheme is byte-for-byte identical to the S3 store. Two GCS clients are built
//! lazily on first use so `default_store` can stay synchronous: `Storage` for
//! reading/writing object bytes, and `StorageControl` for listing and deleting.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use async_trait::async_trait;
use google_cloud_storage::client::{Storage, StorageControl};
use tokio::sync::OnceCell;

use crate::config::GcsConfig;
use crate::sandbox::state::{StateStore, clear_dir};

/// Anonymous (no-auth) credentials for the emulator/endpoint path.
fn anonymous_credentials() -> google_cloud_auth::credentials::Credentials {
    google_cloud_auth::credentials::anonymous::Builder::new().build()
}

pub struct GcsStateStore {
    config: GcsConfig,
    prefix: String, // normalized: no leading/trailing slashes
    // Two lazily-built clients: data plane (Storage) and control plane
    // (StorageControl). Built on first use so construction stays cheap and
    // network-free (`default_store` is synchronous and must not connect).
    storage: OnceCell<Storage>,
    control: OnceCell<StorageControl>,
}

impl GcsStateStore {
    pub fn new(config: GcsConfig) -> Self {
        let prefix = config
            .prefix
            .clone()
            .unwrap_or_default()
            .trim_matches('/')
            .to_string();
        Self {
            config,
            prefix,
            storage: OnceCell::new(),
            control: OnceCell::new(),
        }
    }

    async fn storage(&self) -> Result<&Storage> {
        self.storage
            .get_or_try_init(|| async {
                let mut builder = Storage::builder();
                if let Some(endpoint) = &self.config.endpoint {
                    // An endpoint override means a local emulator (fake-gcs-server)
                    // where ADC doesn't apply — use anonymous credentials so the
                    // client doesn't try to mint a token. Real GCS (no endpoint)
                    // keeps the default ADC path.
                    builder = builder
                        .with_endpoint(endpoint.clone())
                        .with_credentials(anonymous_credentials());
                }
                builder.build().await.context("building GCS Storage client")
            })
            .await
    }

    async fn control(&self) -> Result<&StorageControl> {
        self.control
            .get_or_try_init(|| async {
                let mut builder = StorageControl::builder();
                if let Some(endpoint) = &self.config.endpoint {
                    builder = builder
                        .with_endpoint(endpoint.clone())
                        .with_credentials(anonymous_credentials());
                }
                builder
                    .build()
                    .await
                    .context("building GCS StorageControl client")
            })
            .await
    }

    /// List every object name under `prefix` (paginated via `next_page_token`,
    /// mirroring the S3 store's manual continuation loop).
    async fn list_keys(&self, prefix: &str) -> Result<Vec<String>> {
        let control = self.control().await?;
        let parent = format!("projects/_/buckets/{}", self.config.bucket);
        let mut keys: Vec<String> = Vec::new();
        let mut page_token = String::new();
        loop {
            let mut req = control
                .list_objects()
                .set_parent(&parent)
                .set_prefix(prefix);
            if !page_token.is_empty() {
                req = req.set_page_token(&page_token);
            }
            let resp = req.send().await.context("gcs list_objects")?;
            for obj in resp.objects {
                keys.push(obj.name);
            }
            if resp.next_page_token.is_empty() {
                break;
            }
            page_token = resp.next_page_token;
        }
        Ok(keys)
    }
}

#[async_trait]
impl StateStore for GcsStateStore {
    async fn pull(&self, key: &str, dest: &Path) -> Result<bool> {
        let prefix = dir_prefix(&self.prefix, key);
        let keys = self.list_keys(&prefix).await?;

        if keys.is_empty() {
            return Ok(false); // absent — matches FilesystemStateStore
        }

        let storage = self.storage().await?;
        let bucket = &self.config.bucket;

        clear_dir(dest)?;
        for obj_key in keys {
            let rel = obj_key
                .strip_prefix(&prefix)
                .with_context(|| format!("gcs returned key outside prefix: {obj_key}"))?;
            if rel.is_empty() {
                continue;
            }
            // Bucket contents are only ever written by `push` (keys derived from
            // real filesystem paths), but guard the join anyway so a stray `..`
            // object key can't escape `dest` — parity with the filesystem store.
            if rel.split('/').any(|seg| seg == "..") {
                anyhow::bail!("gcs object key escapes dest: {obj_key}");
            }
            let out = dest.join(rel);
            if let Some(parent) = out.parent() {
                fs::create_dir_all(parent)?;
            }
            // No one-shot helper: collect the streamed chunks into a buffer.
            let mut reader = storage
                .read_object(bucket, &obj_key)
                .send()
                .await
                .with_context(|| format!("gcs read_object {obj_key}"))?;
            let mut buf = Vec::new();
            while let Some(chunk) = reader.next().await {
                let chunk = chunk.with_context(|| format!("gcs read chunk {obj_key}"))?;
                buf.extend_from_slice(&chunk);
            }
            fs::write(&out, buf)?;
        }
        Ok(true)
    }

    async fn push(&self, src: &Path, key: &str) -> Result<()> {
        let prefix = dir_prefix(&self.prefix, key);

        // Replace semantics: delete everything currently under the prefix.
        // No batch delete on GCS — remove per-object in a loop.
        let stale = self.list_keys(&prefix).await?;
        if !stale.is_empty() {
            let control = self.control().await?;
            let bucket = format!("projects/_/buckets/{}", self.config.bucket);
            for obj_key in stale {
                control
                    .delete_object()
                    .set_bucket(&bucket)
                    .set_object(&obj_key)
                    .send()
                    .await
                    .with_context(|| format!("gcs delete_object {obj_key}"))?;
            }
        }

        let storage = self.storage().await?;
        let bucket = &self.config.bucket;
        for entry in walk_files(src)? {
            let rel = entry
                .strip_prefix(src)
                .expect("walk_files yields paths under src")
                .to_string_lossy()
                .replace('\\', "/");
            let obj = object_key(&self.prefix, key, &rel);
            // Resumable uploads are automatic: a `tokio::fs::File` is a direct
            // payload and the client switches to resumable for large files, so
            // there's no manual multipart logic (unlike the S3 store).
            let payload = tokio::fs::File::open(&entry)
                .await
                .with_context(|| format!("open {}", entry.display()))?;
            storage
                .write_object(bucket, &obj, payload)
                .send_unbuffered()
                .await
                .with_context(|| format!("gcs write_object {rel}"))?;
        }
        Ok(())
    }
}

/// Recursively collect all file paths under `dir` (empty if `dir` is absent).
fn walk_files(dir: &Path) -> Result<Vec<std::path::PathBuf>> {
    let mut out = Vec::new();
    if !dir.is_dir() {
        return Ok(out);
    }
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            out.extend(walk_files(&path)?);
        } else {
            out.push(path);
        }
    }
    Ok(out)
}

/// Join non-empty path segments with `/` into a GCS object key.
/// `object_key("cica", "session/abc", "store.db") == "cica/session/abc/store.db"`
/// `object_key("", "session/abc", "x") == "session/abc/x"`
fn object_key(prefix: &str, key: &str, rel: &str) -> String {
    [prefix, key, rel]
        .iter()
        .filter(|s| !s.is_empty())
        .copied()
        .collect::<Vec<_>>()
        .join("/")
}

/// The list prefix (with trailing slash) for all objects under `key`.
/// `dir_prefix("cica", "session/abc") == "cica/session/abc/"`
fn dir_prefix(prefix: &str, key: &str) -> String {
    let base = [prefix, key]
        .iter()
        .filter(|s| !s.is_empty())
        .copied()
        .collect::<Vec<_>>()
        .join("/");
    format!("{base}/")
}

#[cfg(test)]
mod it_tests {
    use std::fs;
    use std::path::Path;

    use super::*;
    use crate::config::GcsConfig;

    // Gated: only runs when CICA_GCS_IT is set (the CI gcs-store job / explicit
    // local run against fake-gcs-server or real GCS).
    fn it_config() -> Option<GcsConfig> {
        std::env::var_os("CICA_GCS_IT")?;
        Some(GcsConfig {
            bucket: std::env::var("CICA_GCS_BUCKET").unwrap_or_else(|_| "cica-test".into()),
            prefix: Some("it".into()),
            endpoint: Some(
                std::env::var("CICA_GCS_ENDPOINT")
                    .unwrap_or_else(|_| "http://localhost:4443".into()),
            ),
        })
    }

    fn write(p: &Path, c: &str) {
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, c).unwrap();
    }

    #[tokio::test]
    async fn gcs_round_trip_absent_and_replace() {
        let Some(cfg) = it_config() else {
            return;
        };
        let store = GcsStateStore::new(cfg);

        // absent → false
        let d0 = tempfile::tempdir().unwrap();
        assert!(!store.pull("session/none", d0.path()).await.unwrap());

        // push a nested tree, pull it back byte-for-byte
        let src = tempfile::tempdir().unwrap();
        write(&src.path().join("a.txt"), "alpha");
        write(&src.path().join("sub/b.txt"), "beta");
        store.push(src.path(), "session/x").await.unwrap();

        let d1 = tempfile::tempdir().unwrap();
        assert!(store.pull("session/x", d1.path()).await.unwrap());
        assert_eq!(
            fs::read_to_string(d1.path().join("a.txt")).unwrap(),
            "alpha"
        );
        assert_eq!(
            fs::read_to_string(d1.path().join("sub/b.txt")).unwrap(),
            "beta"
        );

        // push replaces prior contents
        let src2 = tempfile::tempdir().unwrap();
        write(&src2.path().join("new.txt"), "new");
        store.push(src2.path(), "session/x").await.unwrap();
        let d2 = tempfile::tempdir().unwrap();
        store.pull("session/x", d2.path()).await.unwrap();
        assert!(!d2.path().join("a.txt").exists());
        assert_eq!(
            fs::read_to_string(d2.path().join("new.txt")).unwrap(),
            "new"
        );
    }

    #[tokio::test]
    async fn gcs_resumable_round_trip_large_file() {
        let Some(cfg) = it_config() else {
            return;
        };
        let store = GcsStateStore::new(cfg);

        // Large enough to exercise the automatic resumable-upload path.
        let big = vec![7u8; 20 * 1024 * 1024];
        let src = tempfile::tempdir().unwrap();
        fs::write(src.path().join("big.bin"), &big).unwrap();
        store.push(src.path(), "session/big").await.unwrap();

        let d = tempfile::tempdir().unwrap();
        assert!(store.pull("session/big", d.path()).await.unwrap());
        assert_eq!(fs::read(d.path().join("big.bin")).unwrap(), big);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn object_key_joins_and_skips_empty_prefix() {
        assert_eq!(
            object_key("cica", "session/abc", "store.db"),
            "cica/session/abc/store.db"
        );
        assert_eq!(
            object_key("", "session/abc", "store.db"),
            "session/abc/store.db"
        );
        assert_eq!(object_key("cica", "mem/u1", "a/b.md"), "cica/mem/u1/a/b.md");
    }

    #[test]
    fn dir_prefix_has_trailing_slash() {
        assert_eq!(dir_prefix("cica", "session/abc"), "cica/session/abc/");
        assert_eq!(dir_prefix("", "session/abc"), "session/abc/");
    }

    #[test]
    fn rel_is_object_key_minus_dir_prefix() {
        let p = dir_prefix("cica", "session/abc");
        let k = object_key("cica", "session/abc", "sub/store.db");
        assert_eq!(k.strip_prefix(&p), Some("sub/store.db"));
    }
}
