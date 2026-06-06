//! S3-backed `StateStore` (feature `s3`).
//!
//! Mirrors `FilesystemStateStore` semantics over S3 objects keyed
//! `<prefix>/<key>/<relative-file-path>`. The AWS client is built lazily on
//! first use so `default_store` can stay synchronous.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use async_trait::async_trait;
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::types::{CompletedMultipartUpload, CompletedPart, Delete, ObjectIdentifier};
use tokio::io::AsyncReadExt;
use tokio::sync::OnceCell;

use crate::config::S3Config;
use crate::sandbox::state::{StateStore, clear_dir};

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
    // Don't auto-attach CRC32 checksums. The SDK default ("when_supported") adds one
    // to upload_part but not to create_multipart_upload, so the parts' checksum type
    // never matches the upload's — real S3 tolerates it, S3-compatible servers reject
    // it ("Checksum Type mismatch"). We don't rely on checksums, so opt out entirely.
    builder = builder
        .request_checksum_calculation(aws_sdk_s3::config::RequestChecksumCalculation::WhenRequired);
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
            // Bucket contents are only ever written by `push` (keys derived from
            // real filesystem paths), but guard the join anyway so a stray `..`
            // object key can't escape `dest` — parity with the filesystem store.
            if rel.split('/').any(|seg| seg == "..") {
                anyhow::bail!("s3 object key escapes dest: {obj_key}");
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
                let deleted = client
                    .delete_objects()
                    .bucket(bucket)
                    .delete(delete)
                    .send()
                    .await
                    .context("s3 delete_objects")?;
                // delete_objects returns 200 even on per-key failures; they
                // surface in `errors()`, so an unchecked call could leave stale
                // objects under the key and silently break replace semantics.
                let errs = deleted.errors();
                if !errs.is_empty() {
                    anyhow::bail!("s3 delete_objects partial failure: {} error(s)", errs.len());
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

        for entry in walk_files(src)? {
            let rel = entry
                .strip_prefix(src)
                .expect("walk_files yields paths under src")
                .to_string_lossy()
                .replace('\\', "/");
            let obj = object_key(&self.prefix, key, &rel);
            upload_file(client, bucket, &obj, &entry)
                .await
                .with_context(|| format!("s3 upload {rel}"))?;
        }
        Ok(())
    }
}

/// Threshold/part size for switching to multipart, mirroring the AWS CLI default.
const MULTIPART_THRESHOLD: u64 = 8 * 1024 * 1024;
const MULTIPART_PART_SIZE: u64 = 8 * 1024 * 1024;

/// Upload one file: a single PUT for small files, multipart for large ones.
///
/// A single large `put_object` is fragile — a mid-upload stall makes S3 idle-close
/// the socket (`RequestTimeout`), losing the whole upload (this is what killed large
/// session-transcript pushes). Multipart sends small, independently-retried parts,
/// so big files go up reliably.
async fn upload_file(
    client: &aws_sdk_s3::Client,
    bucket: &str,
    key: &str,
    path: &Path,
) -> Result<()> {
    let len = fs::metadata(path)
        .with_context(|| format!("stat {}", path.display()))?
        .len();

    if len <= MULTIPART_THRESHOLD {
        let body = ByteStream::from_path(path)
            .await
            .with_context(|| format!("reading {}", path.display()))?;
        client
            .put_object()
            .bucket(bucket)
            .key(key)
            .body(body)
            .send()
            .await
            .context("s3 put_object")?;
        return Ok(());
    }

    let created = client
        .create_multipart_upload()
        .bucket(bucket)
        .key(key)
        .send()
        .await
        .context("s3 create_multipart_upload")?;
    let upload_id = created
        .upload_id()
        .context("create_multipart_upload returned no upload_id")?
        .to_string();

    match upload_parts(client, bucket, key, &upload_id, path, len).await {
        Ok(completed) => client
            .complete_multipart_upload()
            .bucket(bucket)
            .key(key)
            .upload_id(&upload_id)
            .multipart_upload(completed)
            .send()
            .await
            .context("s3 complete_multipart_upload")
            .map(|_| ()),
        Err(e) => {
            // Best-effort: don't leave billable orphaned parts behind on failure.
            let _ = client
                .abort_multipart_upload()
                .bucket(bucket)
                .key(key)
                .upload_id(&upload_id)
                .send()
                .await;
            Err(e)
        }
    }
}

async fn upload_parts(
    client: &aws_sdk_s3::Client,
    bucket: &str,
    key: &str,
    upload_id: &str,
    path: &Path,
    len: u64,
) -> Result<CompletedMultipartUpload> {
    let mut file = tokio::fs::File::open(path)
        .await
        .with_context(|| format!("open {}", path.display()))?;
    let mut parts = Vec::new();
    let mut part_number: i32 = 1;
    let mut remaining = len;
    while remaining > 0 {
        let chunk = std::cmp::min(MULTIPART_PART_SIZE, remaining) as usize;
        // Buffer each part: a plain Content-Length body, not a streamed/chunked
        // one (which S3-compatible servers can reject mid-send). 8 MiB peak.
        let mut buf = vec![0u8; chunk];
        file.read_exact(&mut buf)
            .await
            .with_context(|| format!("reading part {part_number} of {}", path.display()))?;
        let resp = client
            .upload_part()
            .bucket(bucket)
            .key(key)
            .upload_id(upload_id)
            .part_number(part_number)
            .body(ByteStream::from(buf))
            .send()
            .await
            .with_context(|| format!("s3 upload_part {part_number}"))?;
        parts.push(
            CompletedPart::builder()
                .set_e_tag(resp.e_tag().map(String::from))
                .part_number(part_number)
                .build(),
        );
        remaining -= chunk as u64;
        part_number += 1;
    }
    Ok(CompletedMultipartUpload::builder()
        .set_parts(Some(parts))
        .build())
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

    #[tokio::test]
    async fn s3_multipart_round_trip_large_file() {
        let Some(cfg) = it_config() else {
            return;
        };
        let store = S3StateStore::new(cfg);

        // > the 8 MiB multipart threshold, so push() takes the multipart path.
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
