# Phase 3b3 — GCP Rust SDK API notes (SPIKE, Task 1)

Research spike pinning the official `googleapis/google-cloud-rust` SDK crates for the
`GcsStateStore` (Task 4) and `CloudRunLauncher` / `GcpRunClient` (Task 7). **No production
code** comes from this task — only these notes. Later tasks fill their SDK calls from here.

All snippets below were confirmed against **docs.rs for the pinned versions** and a local
build experiment (see "Build verification"). Anything not fully confirmed is marked
**UNVERIFIED**.

---

## 1. Pinned versions

| crate | requested | resolved (this repo's lockfile) | role |
|-------|-----------|----------------------------------|------|
| `google-cloud-storage` | `1.14` | **1.15.0** | object IO (GCS) |
| `google-cloud-run-v2`  | `1.11` | **1.11.0** | Cloud Run Jobs/Executions |
| `google-cloud-auth`    | (transitive) | **1.13.0** | ADC — pulled in transitively |

`google-cloud-auth` does **NOT** need to be a direct dependency for the default ADC path.
`Storage::builder().build().await?` and `Jobs::builder().build().await?` use Application
Default Credentials automatically. You'd only add `google-cloud-auth` directly if you needed
to construct a `Credentials` object by hand to pass to `with_credentials(...)` — not needed
for cica's ADC-on-the-worker model. (If a later task wants explicit creds, add
`google-cloud-auth = "1.13"` then.)

### Exact `Cargo.toml` dependency lines

```toml
# GCS state store (feature "gcs")
google-cloud-storage = { version = "1.14", optional = true }

# Cloud Run launcher (feature "cloudrun")
google-cloud-run-v2 = { version = "1.11", optional = true }
```

```toml
[features]
gcs      = ["dep:google-cloud-storage"]
cloudrun = ["gcs", "dep:google-cloud-run-v2"]
```

(`1.14` / `1.11` are caret requirements; cargo resolved `1.15.0` / `1.11.0`. Feature names
are a suggestion — the plan's Task 2/9 own the final feature wiring; `cloudrun` depends on
`gcs` here purely because the launcher and store are deployed together, mirroring
`fargate = ["s3", ...]`.)

### Build verification

Added both deps behind features to this repo's `Cargo.toml`, then:

- `cargo metadata --features cloudrun` — resolved OK (toolchain **rustc 1.96.0**, edition 2024).
- `cargo build --features cloudrun` — **compiled cleanly** (`Finished dev` in ~58s).

Lockfile pulled: `google-cloud-storage 1.15.0`, `google-cloud-run-v2 1.11.0`,
`google-cloud-auth 1.13.0`, plus `google-cloud-gax 1.11.0`, `google-cloud-lro 1.8.0`,
`google-cloud-longrunning 1.11.0`, `google-cloud-wkt 1.5.0`, `google-cloud-iam-v1 1.10.0`.
**All Cargo.toml/Cargo.lock changes were reverted** after the experiment; this task commits
only this notes file.

---

## GCS

Crate: `google-cloud-storage` 1.15.0.
Source: <https://docs.rs/google-cloud-storage/latest/google_cloud_storage/>

Two clients exist:
- **`google_cloud_storage::client::Storage`** — the data-plane client (read/write object bytes).
- **`google_cloud_storage::client::StorageControl`** — the control-plane client. **`list_objects`
  and `delete_object` live here, NOT on `Storage`.** The store will need *both* clients
  (`Storage` for read/write bytes, `StorageControl` for list + delete).

Error type: **`google_cloud_storage::Error`** (alias `google_cloud_storage::Result<T>`).

`OnceCell<...>` field types: `OnceCell<google_cloud_storage::client::Storage>` and
`OnceCell<google_cloud_storage::client::StorageControl>` (or wrap both in one struct).

Docs:
- Storage client: <https://docs.rs/google-cloud-storage/latest/google_cloud_storage/client/struct.Storage.html>
- StorageControl client: <https://docs.rs/google-cloud-storage/latest/google_cloud_storage/client/struct.StorageControl.html>
- ClientBuilder: <https://docs.rs/google-cloud-storage/latest/google_cloud_storage/builder/storage/struct.ClientBuilder.html>

### Build a client with ADC (+ endpoint override for fake-gcs-server)

```rust
use google_cloud_storage::client::{Storage, StorageControl};

// ADC by default.
let storage = Storage::builder().build().await?;
let control = StorageControl::builder().build().await?;
```

Endpoint override **is supported** on the builder — `with_endpoint` exists on both
`Storage` and `StorageControl` builders (confirmed on `ClientBuilder`):

```rust
// pub fn with_endpoint<V: Into<String>>(self, v: V) -> Self
let storage = Storage::builder()
    .with_endpoint("http://localhost:4443")   // fake-gcs-server
    .build()
    .await?;
let control = StorageControl::builder()
    .with_endpoint("http://localhost:4443")
    .build()
    .await?;
```

- `ClientBuilder::build` signature: `pub async fn build(self) -> BuilderResult<Storage>`.
- `with_endpoint` and `with_credentials<V: Into<Credentials>>` confirmed on the builder.

**UNVERIFIED (test-harness detail, not a blocker for the notes):** fake-gcs-server's
compatibility with this SDK's default **gRPC/JSON transport choice** at that endpoint was not
runtime-tested in this spike. fake-gcs-server speaks the JSON/REST API; confirm the SDK is
using the REST endpoint (it historically does for the global endpoint over HTTPS) when Task 5
wires the CI `gcs-store` job. If the SDK insists on gRPC for control-plane calls, the test may
need the JSON endpoint or a real bucket. Flag for Task 4/5.

### List objects under a prefix (with pagination) — `StorageControl`

`list_objects` is on **`StorageControl`** and returns a `ListObjects` request builder.
The "bucket" is passed as the **parent** in the form `projects/_/buckets/<bucket>`.

```rust
use google_cloud_gax::paginator::ItemPaginator;  // trait for .next()

let mut items = control
    .list_objects()
    .set_parent(format!("projects/_/buckets/{bucket}"))
    .set_prefix(&prefix)            // e.g. "cica/session/abc/"
    .by_item();                    // auto-paginates; also: .by_page()
while let Some(obj) = items.next().await {
    let obj = obj?;                // google_cloud_storage::model::Object
    let key = obj.name;            // object key within the bucket, e.g. "cica/session/abc/store.db"
    // ...
}
```

- `set_parent<T: Into<String>>` (**required**), `set_prefix<T: Into<String>>`.
- Pagination: `.by_item()` (per-object stream, `ItemPaginator`) or `.by_page()`
  (`Paginator<ListObjectsResponse, _>`); both auto-follow `next_page_token`. Single-shot
  `.send().await -> Result<ListObjectsResponse>` also exists if you want to handle page
  tokens manually (mirrors the S3 store's manual loop).
- Item type: `google_cloud_storage::model::Object`; the key is the **`name: String`** field
  (object path relative to the bucket — exactly the S3 `key` analogue).

Source: <https://docs.rs/google-cloud-storage/latest/google_cloud_storage/builder/storage_control/struct.ListObjects.html>,
Object model: <https://docs.rs/google-cloud-storage/latest/google_cloud_storage/model/struct.Object.html>

### Download an object's bytes — `Storage::read_object`

`read_object` returns a `ReadObject` builder; `.send()` yields a streaming
`ReadObjectResponse`. There is **no one-shot `all_bytes()` helper** — collect the chunks:

```rust
let mut reader = storage.read_object(bucket, object_name).send().await?;
let mut buf = Vec::new();
while let Some(chunk) = reader.next().await.transpose()? {   // chunk: bytes::Bytes
    buf.extend_from_slice(&chunk);
}
// buf now holds the full object
```

- `read_object<B, O>(&self, bucket: B, object: O) -> ReadObject` — `bucket` here is the bare
  bucket name (data-plane), `object` is the object's `name`.
- `.send().await -> Result<ReadObjectResponse>`; response streams `Result<Bytes>` via `.next()`.

Source: <https://docs.rs/google-cloud-storage/latest/google_cloud_storage/builder/storage/struct.ReadObject.html>

### Upload a file, incl. the large-file path — `Storage::write_object`

**The client handles resumable uploads automatically — we do NOT hand-roll multipart like the
S3 store had to.** A `tokio::fs::File` is a valid payload directly, and the builder picks
resumable vs. single-shot based on a configurable threshold.

```rust
let payload = tokio::fs::File::open(local_path).await?;
let _obj: google_cloud_storage::model::Object = storage
    .write_object(bucket, object_name, payload)
    .send_unbuffered()      // File implements Seek → no buffering needed
    .await?;
```

- `write_object<B, O, T, P>(&self, bucket: B, object: O, payload: T) -> WriteObject<P, S>`.
- Terminal methods: **`send_unbuffered()`** (payload implements `Seek`, e.g. a `File` or a
  byte buffer) and **`send_buffered()`** (streaming source without `Seek`). Both return
  `Result<Object>`.
- Resumable handling: *"The library automatically selects resumable uploads when the payload
  is equal to or larger than this option"* — tuned via `ClientBuilder::with_resumable_upload_threshold(usize)`
  and `with_resumable_upload_buffer_size(usize)`. So large session transcripts go up via
  resumable uploads with no extra code. **This removes the entire `upload_parts` /
  `create_multipart_upload` complexity the S3 store needed.**
- Payload types confirmed in docs: string literals, `tokio::fs::File`, and any
  `StreamingSource` impl.

Source: <https://docs.rs/google-cloud-storage/latest/google_cloud_storage/builder/storage/struct.WriteObject.html>,
builder/threshold: <https://docs.rs/google-cloud-storage/latest/google_cloud_storage/builder/storage/struct.ClientBuilder.html>

### Delete (for the push() replace-semantics pre-delete) — `StorageControl::delete_object`

```rust
control.delete_object()
    .set_bucket(format!("projects/_/buckets/{bucket}"))  // UNVERIFIED exact setter name
    .set_object(object_name)                              // UNVERIFIED exact setter name
    .send().await?;
```

`delete_object()` exists on `StorageControl` and returns a `DeleteObject` builder.
**UNVERIFIED:** the exact setter names/parents on `DeleteObject` were not pulled in this
spike (confirm `set_bucket` vs. `set_parent` and `set_object` for Task 4). There is no batch
delete equivalent to S3's `delete_objects`; the store will delete per-object in a loop over
the `list_objects` results.

---

## Cloud Run

Crate: `google-cloud-run-v2` 1.11.0.
Source: <https://docs.rs/google-cloud-run-v2/latest/google_cloud_run_v2/>

Clients (module `google_cloud_run_v2::client`):
- **`google_cloud_run_v2::client::Jobs`** — `run_job`.
- **`google_cloud_run_v2::client::Executions`** — `get_execution`, `cancel_execution`.

Error type: **`google_cloud_run_v2::Error`** (alias `google_cloud_run_v2::Result`).
LRO infra comes from `google-cloud-lro` (`Poller`, `PollingResult`) and
`google-cloud-longrunning` (`model::Operation`).

`OnceCell<...>` field types: `OnceCell<google_cloud_run_v2::client::Jobs>` and
`OnceCell<google_cloud_run_v2::client::Executions>`.

### Build the clients with ADC

```rust
use google_cloud_run_v2::client::{Jobs, Executions};

let jobs = Jobs::builder().build().await?;          // ADC by default
let executions = Executions::builder().build().await?;
```

Both builders expose `with_endpoint` / `with_credentials` like the GCS builder, if ever needed.

Source: <https://docs.rs/google-cloud-run-v2/latest/google_cloud_run_v2/client/struct.Jobs.html>,
<https://docs.rs/google-cloud-run-v2/latest/google_cloud_run_v2/client/struct.Executions.html>

### RunJob with container arg overrides

Models (note the nested module paths):
- `google_cloud_run_v2::model::run_job_request::Overrides`
- `google_cloud_run_v2::model::run_job_request::overrides::ContainerOverride`

`ContainerOverride` fields: `name: String`, `args: Vec<String>`, `env: Vec<EnvVar>`,
`clear_args: bool`. `Overrides` fields: `container_overrides: Vec<ContainerOverride>`,
`task_count: i32`, `timeout: Option<Duration>`.

```rust
use google_cloud_run_v2::model::run_job_request::{Overrides, overrides::ContainerOverride};

let overrides = Overrides::new().set_container_overrides([
    ContainerOverride::new()
        .set_name(container_name)                       // worker container name
        .set_args(vec!["worker".into(),
                       "--turn".into(),
                       turn_id.into()]),
]);

let job_name = format!("projects/{project}/locations/{region}/jobs/{job}");
let operation = jobs
    .run_job()
    .set_name(job_name)
    .set_overrides(overrides)
    .send()                       // <-- returns the raw Operation, does NOT await the LRO
    .await?;
```

- `run_job() -> RunJob`; `set_name<T: Into<String>>` (required),
  `set_overrides<T: Into<Overrides>>`.
- **UNVERIFIED (cosmetic):** `ContainerOverride::new()`/`set_name`/`set_args` builder-setter
  names are inferred from the SDK's uniform `set_<field>` convention and the `Overrides::new()`
  example in docs; the build experiment compiled the crate but did not exercise these exact
  setters. If a setter name differs, fields are public so direct struct construction
  (`ContainerOverride { name, args, ..Default::default() }`) is the fallback.

Sources: RunJob builder <https://docs.rs/google-cloud-run-v2/latest/google_cloud_run_v2/builder/jobs/struct.RunJob.html>,
Overrides <https://docs.rs/google-cloud-run-v2/latest/google_cloud_run_v2/model/run_job_request/struct.Overrides.html>,
ContainerOverride (Rust struct mirror) <https://mechiru.github.io/google-api-proto/google_api_proto/google/cloud/run/v2/run_job_request/overrides/struct.ContainerOverride.html>

### *** Getting the execution resource name WITHOUT awaiting the LRO ***  (most important finding)

**YES — surfaced cleanly, with one caveat about which call to use.**

`RunJob` has two terminal paths:
- **`send(self) -> Result<google_cloud_longrunning::model::Operation>`** — fires the request and
  returns the **raw `Operation` immediately, without polling to completion.**
- `poller(self) -> impl Poller<Execution, Execution>` — convenience poller;
  `.until_done().await` blocks until the execution finishes.

For cica's controller we want the execution name up front (to then poll `GetExecution`
ourselves on our own cadence, matching the Fargate state machine). Use **`send()`** and read
the operation's metadata.

The metadata `Any` for RunJob is a `google.cloud.run.v2.Execution` message (Cloud Run sets the
Execution as the operation metadata). So:

```rust
let op = jobs.run_job().set_name(job_name).set_overrides(overrides).send().await?;
// op: google_cloud_longrunning::model::Operation
//   op.name    : String        — the LRO name (NOT the execution name)
//   op.metadata: Option<Any>   — decodes to model::Execution (has .name = execution resource name)
//   op.done    : bool
//   op.result  : Option<operation::Result>  (Error | Response)

let exec_name: String = {
    let any = op.metadata.as_ref()
        .context("run_job operation had no metadata")?;
    let exec: google_cloud_run_v2::model::Execution = any.to_msg()?;  // see UNVERIFIED below
    exec.name   // "projects/{p}/locations/{region}/jobs/{job}/executions/{exec}"
};
```

Caveat / **the cleaner, recommended path** — use the **poller's first in-progress poll**, which
hands you the typed metadata directly with no manual `Any` decode:

```rust
use google_cloud_lro::{Poller, PollingResult};

let mut poller = jobs.run_job().set_name(job_name).set_overrides(overrides).poller();
let exec_name = match poller.poll().await {
    // For RunJob, M (metadata) == Execution, so m.name is the execution resource name,
    // available on the FIRST poll without waiting for the job to finish.
    Some(PollingResult::InProgress(Some(exec))) => exec.name,
    Some(PollingResult::Completed(res)) => res?.name,        // finished instantly (rare)
    Some(PollingResult::InProgress(None)) => anyhow::bail!("no metadata on first poll"),
    Some(PollingResult::PollingError(e)) => return Err(e.into()),
    None => anyhow::bail!("poller returned no result"),
};
// Then drop the poller and switch to our own GetExecution loop on exec_name.
```

- `Poller::poll(&mut self) -> impl Future<Output = Option<PollingResult<R, M>>>`.
- `PollingResult` variants: **`InProgress(M)`**, **`Completed(Result<R>)`**, **`PollingError(E)`**.
  Confirmed variant names from the `google-cloud-lro` docs. For `run_job`, both `R` and `M` are
  `Execution` (poller type is `impl Poller<Execution, Execution>`), and the docs state the
  metadata carries partial progress including the execution identity.

**UNVERIFIED but high-confidence:**
1. The exact `Any` decode helper name — docs show `metadata: Option<Any>` (from
   `google_cloud_wkt`) with no documented typed getter. The decode method is likely
   `Any::to_msg::<Execution>()` / `try_into` on `google_cloud_wkt::Any`; **confirm the exact
   method name in Task 7** (search `google_cloud_wkt::Any` on docs.rs). This is why the
   **poller-`poll()` path above is recommended** — it sidesteps manual `Any` decoding entirely.
2. That RunJob's metadata message is `Execution` specifically (vs. an empty/`OperationMetadata`).
   This is true for the proto API surface and consistent with `poller()` being typed
   `Poller<Execution, Execution>`; treat as confirmed-by-types but verify the first `poll()`
   actually yields `InProgress(Some(exec))` against a real project in Task 7.

**Bottom line for the controller:** RunJob does NOT force you to await the LRO to get the
execution name. Use `poller().poll()` once, read `InProgress(Execution).name`, then poll
`GetExecution` on your own schedule. No plan change required.

### GetExecution — terminal vs. running, success vs. failure

`Executions::get_execution() -> GetExecution`; `.set_name(<execution resource name>).send().await
-> Result<Execution>`.

```rust
let exec = executions.get_execution().set_name(&exec_name).send().await?;
```

`Execution` status fields (`google_cloud_run_v2::model::Execution`):

| field | type | meaning |
|-------|------|---------|
| `name` | `String` | execution resource name |
| `completion_time` | `Option<Timestamp>` | **Some ⇒ terminal.** None ⇒ still running. |
| `start_time` | `Option<Timestamp>` | when it began running |
| `running_count` | `i32` | actively running tasks |
| `succeeded_count` | `i32` | tasks that reached Succeeded |
| `failed_count` | `i32` | tasks that reached Failed |
| `cancelled_count` | `i32` | tasks that reached Cancelled |
| `reconciling` | `bool` | reconciliation still in progress |
| `conditions` | `Vec<Condition>` | readiness + **failure detail** |

Recommended terminal/success logic (mirrors Fargate's STOPPED + exit-code check):

```rust
let terminal = exec.completion_time.is_some();
if terminal {
    let ok = exec.succeeded_count > 0 && exec.failed_count == 0
             && exec.cancelled_count == 0;
    // ... ok ? Ok(()) : Err(failure_message)
}
```

**Where the failure message lives:** in `conditions`. Each
`google_cloud_run_v2::model::Condition` has:
- `r#type: String` (e.g. `"Completed"` / `"ResourcesAvailable"`),
- `state: condition::State` — enum `google_cloud_run_v2::model::condition::State` with
  variants incl. `ConditionPending`, `ConditionReconciling`, `ConditionFailed`
  (and a success state),
- **`message: String`** — human-readable detail (the failure reason text),
- `reasons: Option<Reasons>`, `last_transition_time: Option<Timestamp>`.

So on failure, pull the message from the condition whose `state == State::ConditionFailed`
(typically `r#type == "Completed"`):

```rust
let msg = exec.conditions.iter()
    .find(|c| c.state == google_cloud_run_v2::model::condition::State::ConditionFailed)
    .map(|c| c.message.clone())
    .unwrap_or_default();
```

**UNVERIFIED (minor):** the exact success-state enum variant name (e.g.
`State::ConditionSucceeded`) — docs.rs surfaced `ConditionPending/Reconciling/Failed` in the
excerpt; confirm the success variant in Task 7. Using `completion_time` + the count fields for
the success decision (above) avoids depending on the enum variant name, so this is non-blocking.

Sources: Execution <https://docs.rs/google-cloud-run-v2/latest/google_cloud_run_v2/model/struct.Execution.html>,
Condition <https://docs.rs/google-cloud-run-v2/latest/google_cloud_run_v2/model/struct.Condition.html>,
Executions client <https://docs.rs/google-cloud-run-v2/latest/google_cloud_run_v2/client/struct.Executions.html>

### CancelExecution (best-effort stop on timeout)

`Executions::cancel_execution() -> CancelExecution`. It is **also an LRO** (returns an
Operation / has a poller), but for a best-effort cancel we just fire `send()` and ignore the
returned operation (parity with Fargate's best-effort `stop_task`):

```rust
let _ = executions
    .cancel_execution()
    .set_name(&exec_name)
    .send()                 // returns Operation immediately; don't await completion
    .await;                 // log-and-ignore on error
```

- `set_name<T: Into<String>>`; `.send().await -> Result<Operation>`; `.poller()` also exists
  (`.poller().until_done().await -> Result<Execution>`) if a synchronous cancel is ever wanted.

Source: <https://docs.rs/google-cloud-run-v2/latest/google_cloud_run_v2/client/struct.Executions.html>

---

## Summary for later tasks

- **Versions:** `google-cloud-storage = "1.14"` (→1.15.0), `google-cloud-run-v2 = "1.11"`
  (→1.11.0), both `optional = true`. `google-cloud-auth` is transitive — not a direct dep.
  Verified building against rustc 1.96.0 / edition 2024 in this repo.
- **GCS:** two clients — `Storage` (read/write bytes) + `StorageControl` (list/delete).
  `with_endpoint` supports fake-gcs-server. **Resumable uploads are automatic** — no manual
  multipart; a `tokio::fs::File` is a direct payload via `write_object(...).send_unbuffered()`.
  Object key = `Object.name`.
- **Cloud Run:** `RunJob::send()` returns the raw LRO `Operation` immediately; the typed
  execution name is available from the **first `poller().poll()`** as
  `PollingResult::InProgress(Execution).name` — **no need to await the job to completion.**
  Poll `GetExecution`: terminal ⇔ `completion_time.is_some()`; success ⇔ `succeeded_count>0 &&
  failed_count==0 && cancelled_count==0`; failure message in `conditions[].message` where
  `state == ConditionFailed`. `CancelExecution` for best-effort timeout stop.
- **Open items for Task 7 (all non-blocking, marked UNVERIFIED above):** exact `Any`-decode
  helper name (sidestepped by the poller path); exact `DeleteObject` setter names; the
  success-state enum variant name; ContainerOverride builder-setter names; and a runtime check
  that fake-gcs-server speaks the SDK's chosen transport.
