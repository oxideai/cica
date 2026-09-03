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
use serde::{Deserialize, Serialize};
use tokio::io::AsyncReadExt;
use tokio::sync::OnceCell;
use tracing::warn;
use uuid::Uuid;

use crate::atomic::Staging;
use crate::config::S3Config;
use crate::sandbox::state::StateStore;

#[derive(Deserialize, Serialize)]
struct Manifest {
    #[serde(rename = "gen")]
    generation: String,
    files: Vec<String>,
}

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
        let current = format!("{prefix}current");
        let manifest = match client
            .get_object()
            .bucket(bucket)
            .key(&current)
            .send()
            .await
        {
            Ok(response) => {
                let bytes = response
                    .body
                    .collect()
                    .await
                    .context("s3 current body collect")?
                    .into_bytes();
                Some(serde_json::from_slice::<Manifest>(&bytes).context("parsing s3 current")?)
            }
            Err(error) => {
                if error
                    .as_service_error()
                    .is_some_and(|service| service.is_no_such_key())
                {
                    None
                } else {
                    return Err(error).context("s3 get current");
                }
            }
        };

        let (generation, files) = if let Some(manifest) = manifest {
            (Some(manifest.generation), manifest.files)
        } else {
            let keys = list_keys(client, bucket, &prefix).await?;
            let files = keys
                .into_iter()
                .filter_map(|object| {
                    object.strip_prefix(&prefix).and_then(|rel| {
                        (!rel.is_empty() && rel != "current" && !rel.starts_with("gen/"))
                            .then(|| rel.to_string())
                    })
                })
                .collect::<Vec<_>>();
            if files.is_empty() {
                return Ok(false);
            }
            (None, files)
        };

        let staging = Staging::beside(dest)?;
        for rel in files {
            if rel.split('/').any(|segment| segment == "..") {
                anyhow::bail!("s3 object key escapes dest: {rel}");
            }
            let object = match &generation {
                Some(generation) => format!("{prefix}gen/{generation}/{rel}"),
                None => format!("{prefix}{rel}"),
            };
            download_file(client, bucket, &object, &staging.path().join(&rel)).await?;
        }
        staging.commit()?;
        Ok(true)
    }

    async fn push(&self, src: &Path, key: &str) -> Result<()> {
        let client = self.client().await?;
        let bucket = &self.config.bucket;
        let prefix = dir_prefix(&self.prefix, key);
        let generation = Uuid::new_v4().to_string();
        let mut files = Vec::new();
        for entry in walk_files(src)? {
            let rel = entry
                .strip_prefix(src)
                .expect("walk_files yields paths under src")
                .to_string_lossy()
                .replace('\\', "/");
            let obj = object_key(&self.prefix, key, &format!("gen/{generation}/{rel}"));
            upload_file(client, bucket, &obj, &entry)
                .await
                .with_context(|| format!("s3 upload {rel}"))?;
            files.push(rel);
        }
        let current = object_key(&self.prefix, key, "current");
        let body = serde_json::to_vec(&Manifest {
            generation: generation.clone(),
            files,
        })?;
        client
            .put_object()
            .bucket(bucket)
            .key(&current)
            .body(ByteStream::from(body))
            .send()
            .await
            .context("s3 put current")?;
        let generation_prefix = format!("{prefix}gen/{generation}/");
        if let Err(error) = delete_prefix(client, bucket, &prefix, |object| {
            object == current || object.starts_with(&generation_prefix)
        })
        .await
        {
            warn!("failed to prune old S3 state under {prefix}: {error}");
        }
        Ok(())
    }

    async fn delete(&self, key: &str) -> Result<()> {
        let client = self.client().await?;
        delete_prefix(
            client,
            &self.config.bucket,
            &dir_prefix(&self.prefix, key),
            |_| false,
        )
        .await
    }
}

async fn list_keys(client: &aws_sdk_s3::Client, bucket: &str, prefix: &str) -> Result<Vec<String>> {
    let mut keys = Vec::new();
    let mut token = None;
    loop {
        let mut request = client.list_objects_v2().bucket(bucket).prefix(prefix);
        if let Some(value) = &token {
            request = request.continuation_token(value);
        }
        let response = request.send().await.context("s3 list_objects_v2")?;
        keys.extend(
            response
                .contents()
                .iter()
                .filter_map(|object| object.key().map(str::to_string)),
        );
        if response.is_truncated().unwrap_or(false) {
            token = Some(
                response
                    .next_continuation_token()
                    .context("s3 list truncated but returned no continuation token")?
                    .to_string(),
            );
        } else {
            break;
        }
    }
    Ok(keys)
}

async fn download_file(
    client: &aws_sdk_s3::Client,
    bucket: &str,
    object: &str,
    dest: &Path,
) -> Result<()> {
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    let response = client
        .get_object()
        .bucket(bucket)
        .key(object)
        .send()
        .await
        .with_context(|| format!("s3 get_object {object}"))?;
    let bytes = response
        .body
        .collect()
        .await
        .context("s3 body collect")?
        .into_bytes();
    fs::write(dest, bytes)?;
    Ok(())
}

async fn delete_prefix(
    client: &aws_sdk_s3::Client,
    bucket: &str,
    prefix: &str,
    keep: impl Fn(&str) -> bool,
) -> Result<()> {
    for chunk in list_keys(client, bucket, prefix).await?.chunks(1000) {
        let ids = chunk
            .iter()
            .filter(|key| !keep(key))
            .map(|key| ObjectIdentifier::builder().key(key).build())
            .collect::<Result<Vec<_>, _>>()
            .context("building delete identifiers")?;
        if ids.is_empty() {
            continue;
        }
        let delete = Delete::builder()
            .set_objects(Some(ids))
            .build()
            .context("building delete request")?;
        let response = client
            .delete_objects()
            .bucket(bucket)
            .delete(delete)
            .send()
            .await
            .context("s3 delete_objects")?;
        if !response.errors().is_empty() {
            anyhow::bail!(
                "s3 delete_objects partial failure: {} error(s)",
                response.errors().len()
            );
        }
    }
    Ok(())
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
    use crate::sandbox::state::contract;

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

    macro_rules! contract_test {
        ($name:ident, $contract:ident) => {
            #[tokio::test]
            async fn $name() {
                let Some(cfg) = it_config() else {
                    return;
                };
                let store = S3StateStore::new(cfg);
                let key = format!("contract/{}/{}", stringify!($name), Uuid::new_v4());
                contract::$contract(&store, &key).await.unwrap();
                store.delete(&key).await.unwrap();
            }
        };
    }

    contract_test!(push_then_pull_round_trips, push_then_pull_round_trips);
    contract_test!(
        push_smaller_tree_removes_stale_files,
        push_smaller_tree_removes_stale_files
    );
    contract_test!(
        push_empty_tree_reads_present_and_empty,
        push_empty_tree_reads_present_and_empty
    );
    contract_test!(
        pull_absent_key_leaves_dest_untouched,
        pull_absent_key_leaves_dest_untouched
    );
    contract_test!(pull_replaces_dest_whole, pull_replaces_dest_whole);
    contract_test!(
        push_failure_leaves_prior_contents,
        push_failure_leaves_prior_contents
    );
    contract_test!(delete_makes_key_absent, delete_makes_key_absent);
    contract_test!(delete_absent_key_is_ok, delete_absent_key_is_ok);

    #[tokio::test]
    async fn pull_failure_leaves_prior_local_contents() {
        let Some(cfg) = it_config() else {
            return;
        };
        let store = S3StateStore::new(cfg);
        let key = format!("pull-failure/{}", Uuid::new_v4());
        let first = tempfile::tempdir().unwrap();
        write(&first.path().join("good.txt"), "good");
        store.push(first.path(), &key).await.unwrap();
        let dest_parent = tempfile::tempdir().unwrap();
        let dest = dest_parent.path().join("dest");
        store.pull(&key, &dest).await.unwrap();
        let second = tempfile::tempdir().unwrap();
        write(&second.path().join("x.txt"), "x");
        write(&second.path().join("y.txt"), "y");
        store.push(second.path(), &key).await.unwrap();
        let client = store.client().await.unwrap();
        let prefix = dir_prefix(&store.prefix, &key);
        let current = client
            .get_object()
            .bucket(&store.config.bucket)
            .key(format!("{prefix}current"))
            .send()
            .await
            .unwrap();
        let manifest: Manifest =
            serde_json::from_slice(&current.body.collect().await.unwrap().into_bytes()).unwrap();
        client
            .delete_object()
            .bucket(&store.config.bucket)
            .key(format!("{prefix}gen/{}/y.txt", manifest.generation))
            .send()
            .await
            .unwrap();

        assert!(store.pull(&key, &dest).await.is_err());
        assert_eq!(fs::read_to_string(dest.join("good.txt")).unwrap(), "good");
        assert!(!dest.join("x.txt").exists());
        store.delete(&key).await.unwrap();
    }

    #[tokio::test]
    async fn pull_reads_legacy_flat_layout() {
        let Some(cfg) = it_config() else {
            return;
        };
        let store = S3StateStore::new(cfg);
        let key = format!("legacy/{}", Uuid::new_v4());
        let client = store.client().await.unwrap();
        let prefix = dir_prefix(&store.prefix, &key);
        for (rel, contents) in [("a.txt", "a"), ("sub/b.txt", "b")] {
            client
                .put_object()
                .bucket(&store.config.bucket)
                .key(format!("{prefix}{rel}"))
                .body(ByteStream::from(contents.as_bytes().to_vec()))
                .send()
                .await
                .unwrap();
        }
        let parent = tempfile::tempdir().unwrap();
        let dest = parent.path().join("dest");
        assert!(store.pull(&key, &dest).await.unwrap());
        assert_eq!(fs::read_to_string(dest.join("a.txt")).unwrap(), "a");
        assert_eq!(fs::read_to_string(dest.join("sub/b.txt")).unwrap(), "b");

        let src = tempfile::tempdir().unwrap();
        write(&src.path().join("new.txt"), "new");
        store.push(src.path(), &key).await.unwrap();
        let keys = list_keys(client, &store.config.bucket, &prefix)
            .await
            .unwrap();
        assert!(
            keys.iter()
                .any(|object| object == &format!("{prefix}current"))
        );
        assert!(keys.iter().all(|object| {
            let rel = object.strip_prefix(&prefix).unwrap();
            rel == "current" || rel.starts_with("gen/")
        }));
        store.delete(&key).await.unwrap();
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
