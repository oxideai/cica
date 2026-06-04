//! S3-backed `StateStore` (feature `s3`).
//!
//! Mirrors `FilesystemStateStore` semantics over S3 objects keyed
//! `<prefix>/<key>/<relative-file-path>`. The AWS client is built lazily on
//! first use so `default_store` can stay synchronous.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use async_trait::async_trait;
use aws_sdk_s3::types::{Delete, ObjectIdentifier};
use tokio::sync::OnceCell;

use crate::config::S3Config;
use crate::sandbox::state::{StateStore, clear_dir};

/// `StateStore` backed by an S3 bucket. The client is built lazily on first use
/// so `default_store` can stay synchronous.
pub struct S3StateStore {
    config: S3Config,
    prefix: String, // normalized: no leading/trailing slashes
    client: OnceCell<aws_sdk_s3::Client>,
}

impl S3StateStore {
    pub fn new(config: S3Config) -> Self {
        let prefix = config
            .prefix
            .clone()
            .unwrap_or_default()
            .trim_matches('/')
            .to_string();
        Self {
            config,
            prefix,
            client: OnceCell::new(),
        }
    }

    async fn client(&self) -> Result<&aws_sdk_s3::Client> {
        self.client
            .get_or_try_init(|| async { build_client(&self.config).await })
            .await
    }
}

async fn build_client(cfg: &S3Config) -> Result<aws_sdk_s3::Client> {
    let mut loader = aws_config::defaults(aws_config::BehaviorVersion::latest());
    if let Some(region) = &cfg.region {
        loader = loader.region(aws_config::Region::new(region.clone()));
    }
    if let Some(endpoint) = &cfg.endpoint {
        loader = loader.endpoint_url(endpoint);
    }
    let shared = loader.load().await;
    let mut builder = aws_sdk_s3::config::Builder::from(&shared);
    // Path-style addressing for LocalStack/MinIO (virtual-host style needs DNS).
    if cfg.endpoint.is_some() {
        builder = builder.force_path_style(true);
    }
    Ok(aws_sdk_s3::Client::from_conf(builder.build()))
}

#[async_trait]
impl StateStore for S3StateStore {
    async fn pull(&self, key: &str, dest: &Path) -> Result<bool> {
        let client = self.client().await?;
        let bucket = &self.config.bucket;
        let prefix = dir_prefix(&self.prefix, key);

        // List all objects under the prefix (paginated).
        let mut keys: Vec<String> = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let mut req = client.list_objects_v2().bucket(bucket).prefix(&prefix);
            if let Some(t) = &token {
                req = req.continuation_token(t);
            }
            let resp = req.send().await.context("s3 list_objects_v2")?;
            for obj in resp.contents() {
                if let Some(k) = obj.key() {
                    keys.push(k.to_string());
                }
            }
            if resp.is_truncated().unwrap_or(false) {
                match resp.next_continuation_token() {
                    Some(t) => token = Some(t.to_string()),
                    None => anyhow::bail!("s3 list truncated but returned no continuation token"),
                }
            } else {
                break;
            }
        }

        if keys.is_empty() {
            return Ok(false); // absent — matches FilesystemStateStore
        }

        clear_dir(dest)?;
        for obj_key in keys {
            let rel = obj_key
                .strip_prefix(&prefix)
                .with_context(|| format!("s3 returned key outside prefix: {obj_key}"))?;
            if rel.is_empty() {
                continue;
            }
            let out = dest.join(rel);
            if let Some(parent) = out.parent() {
                fs::create_dir_all(parent)?;
            }
            let resp = client
                .get_object()
                .bucket(bucket)
                .key(&obj_key)
                .send()
                .await
                .with_context(|| format!("s3 get_object {obj_key}"))?;
            let bytes = resp
                .body
                .collect()
                .await
                .context("s3 body collect")?
                .into_bytes();
            fs::write(&out, bytes)?;
        }
        Ok(true)
    }

    async fn push(&self, src: &Path, key: &str) -> Result<()> {
        let client = self.client().await?;
        let bucket = &self.config.bucket;
        let prefix = dir_prefix(&self.prefix, key);

        // Replace semantics: delete everything currently under the prefix.
        let mut token: Option<String> = None;
        loop {
            let mut req = client.list_objects_v2().bucket(bucket).prefix(&prefix);
            if let Some(t) = &token {
                req = req.continuation_token(t);
            }
            let resp = req.send().await.context("s3 list (pre-delete)")?;
            let ids: Vec<ObjectIdentifier> = resp
                .contents()
                .iter()
                .filter_map(|obj| obj.key())
                .map(|k| ObjectIdentifier::builder().key(k).build())
                .collect::<Result<Vec<_>, _>>()
                .context("building delete identifiers")?;
            if !ids.is_empty() {
                let delete = Delete::builder()
                    .set_objects(Some(ids))
                    .build()
                    .context("building delete request")?;
                client
                    .delete_objects()
                    .bucket(bucket)
                    .delete(delete)
                    .send()
                    .await
                    .context("s3 delete_objects")?;
            }
            if resp.is_truncated().unwrap_or(false) {
                match resp.next_continuation_token() {
                    Some(t) => token = Some(t.to_string()),
                    None => anyhow::bail!("s3 list truncated but returned no continuation token"),
                }
            } else {
                break;
            }
        }

        // Upload every file under `src`, keyed by its path relative to `src`.
        for entry in walk_files(src)? {
            let rel = entry
                .strip_prefix(src)
                .expect("walk_files yields paths under src")
                .to_string_lossy()
                .replace('\\', "/");
            let body = aws_sdk_s3::primitives::ByteStream::from_path(&entry)
                .await
                .with_context(|| format!("reading {}", entry.display()))?;
            client
                .put_object()
                .bucket(bucket)
                .key(object_key(&self.prefix, key, &rel))
                .body(body)
                .send()
                .await
                .with_context(|| format!("s3 put_object {rel}"))?;
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

/// Join non-empty path segments with `/` into an S3 object key.
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
    use crate::config::S3Config;

    // Gated: only runs when CICA_S3_IT is set (the CI s3-store job / explicit local run).
    fn it_config() -> Option<S3Config> {
        std::env::var_os("CICA_S3_IT")?;
        Some(S3Config {
            bucket: std::env::var("CICA_S3_BUCKET").unwrap_or_else(|_| "cica-test".into()),
            region: Some(std::env::var("AWS_REGION").unwrap_or_else(|_| "us-east-1".into())),
            prefix: Some("it".into()),
            endpoint: Some(
                std::env::var("CICA_S3_ENDPOINT")
                    .unwrap_or_else(|_| "http://localhost:4566".into()),
            ),
        })
    }

    fn write(p: &Path, c: &str) {
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, c).unwrap();
    }

    #[tokio::test]
    async fn s3_round_trip_absent_and_replace() {
        let Some(cfg) = it_config() else {
            return;
        };
        let store = S3StateStore::new(cfg);

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
