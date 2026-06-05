# Skills Delivery Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Deliver published skills from a private git repo onto both the router and the ephemeral workers, kept in sync and refreshed periodically, with zero cloud-specific code in cica.

**Architecture:** The router runs a `tokio` task that shallow-clones the skills repo on an interval, mirrors the tree to the existing `StateStore` under the `"skills"` key, and atomically swaps it into `skills_dir` for its own `discover_skills`. Each worker's `HydratingProvider` pulls `"skills"` from the store before a turn (read-only). All cloud-specifics (the git token, which `StateStore` impl) are already abstracted behind env + the `StateStore` trait.

**Tech Stack:** Rust (tokio, serde, anyhow), shell-out `git` with `GIT_ASKPASS`; AWS CDK (TypeScript) for the sprout deployment wiring.

**Spec:** `docs/superpowers/specs/2026-06-05-skills-delivery-design.md`

---

## File Structure

**cica:**
- `src/config.rs` — add `SkillsConfig` + `Config.skills: Option<SkillsConfig>`.
- `src/skills_sync.rs` (new) — `sync_once`, `run_sync_loop`, git clone via `GIT_ASKPASS`, atomic swap. Sibling module to the existing `src/skills.rs`.
- `src/main.rs` — declare `mod skills_sync;`.
- `src/sandbox/hydrating.rs` — pull `"skills"` before the turn.
- `src/cmd/run.rs` — spawn the sync loop when `[skills]` is set.

**sprout:**
- `lib/router-stack.ts` — `sprout/skills-git-token` secret, role read grant, install `git`+`awscli`, fetch token → `/etc/cica.env`, inject into the systemd unit.
- `test/router-stack.test.ts` (create if absent) — synth assertions.
- `RUNBOOK.md` — `[skills]` config block + populate-secret step.

---

### Task 1: `[skills]` config section

**Files:**
- Modify: `src/config.rs` (add struct + field near `DeploymentConfig` ~line 227, and the field on `Config` ~line 247; tests in the `#[cfg(test)] mod tests`)

- [ ] **Step 1: Write the failing tests**

Add to the `#[cfg(test)] mod tests` block in `src/config.rs`:

```rust
#[test]
fn parses_skills_section() {
    let toml = r#"
backend = "claude"
[skills]
repo = "https://github.com/root-global/ai-skills"
ref = "main"
"#;
    let cfg: Config = toml::from_str(toml).unwrap();
    let s = cfg.skills.expect("skills present");
    assert_eq!(s.repo, "https://github.com/root-global/ai-skills");
    assert_eq!(s.git_ref, "main");
    assert_eq!(s.refresh_secs, 600);
}

#[test]
fn skills_absent_is_none() {
    let cfg: Config = toml::from_str(r#"backend = "claude""#).unwrap();
    assert!(cfg.skills.is_none());
}

#[test]
fn skills_defaults_applied() {
    let cfg: Config = toml::from_str("[skills]\nrepo = \"x\"\n").unwrap();
    let s = cfg.skills.unwrap();
    assert_eq!(s.git_ref, "main");
    assert_eq!(s.refresh_secs, 600);
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib config::tests::parses_skills_section config::tests::skills_absent_is_none config::tests::skills_defaults_applied`
Expected: FAIL — `no field 'skills' on Config` / `cannot find type SkillsConfig`.

- [ ] **Step 3: Add the struct + field**

In `src/config.rs`, add after the `DeploymentConfig` struct (after ~line 227):

```rust
fn default_skills_ref() -> String {
    "main".to_string()
}
fn default_skills_refresh_secs() -> u64 {
    600
}

/// Skills git-sync settings (router-side). When present, the router periodically
/// pulls `repo` at `ref` into the skills directory and mirrors it to the state
/// store under "skills" for workers to hydrate. The git credential is read from
/// the `CICA_SKILLS_GIT_TOKEN` env var, never from config.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillsConfig {
    /// Git repository URL (required), e.g. https://github.com/root-global/ai-skills.
    pub repo: String,
    /// Branch, tag, or sha to check out.
    #[serde(default = "default_skills_ref", rename = "ref")]
    pub git_ref: String,
    /// Seconds between re-pulls.
    #[serde(default = "default_skills_refresh_secs")]
    pub refresh_secs: u64,
}
```

Add the field to `Config` (after the `deployment` field, ~line 247):

```rust
    /// Skills git-sync settings (router-side). Absent = no skills sync.
    #[serde(default)]
    pub skills: Option<SkillsConfig>,
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib config::tests::parses_skills_section config::tests::skills_absent_is_none config::tests::skills_defaults_applied`
Expected: PASS (3 tests).

- [ ] **Step 5: Commit**

```bash
git add src/config.rs
git commit -m "feat(config): add [skills] git-sync section"
```

---

### Task 2: `skills_sync` module — clone, mirror, atomic swap

**Files:**
- Create: `src/skills_sync.rs`
- Modify: `src/main.rs` (add `mod skills_sync;`)

- [ ] **Step 1: Declare the module**

In `src/main.rs`, add alongside the other `mod` lines (near `mod skills;` at line 12):

```rust
mod skills_sync;
```

- [ ] **Step 2: Write the module with failing tests**

Create `src/skills_sync.rs`:

```rust
//! Periodic git-sync of the skills repo (router-side).
//!
//! Clones the configured repo, mirrors the working tree to the `StateStore`
//! under "skills" (so ephemeral workers can hydrate it), and atomically swaps
//! it into the skills directory for the router's own `discover_skills`. On any
//! failure the existing skills directory is left untouched (last-good wins).

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use tracing::{info, warn};
use uuid::Uuid;

use crate::config::SkillsConfig;
use crate::sandbox::state::StateStore;

/// Clone `cfg.repo`@`cfg.git_ref`, mirror to `store` (if any) under "skills",
/// and atomically replace `skills_dir`. Leaves `skills_dir` untouched on error.
pub async fn sync_once(
    cfg: &SkillsConfig,
    store: Option<&dyn StateStore>,
    skills_dir: &Path,
) -> Result<()> {
    let parent = skills_dir
        .parent()
        .ok_or_else(|| anyhow!("skills_dir has no parent: {}", skills_dir.display()))?;
    std::fs::create_dir_all(parent)?;

    // Clone into a sibling temp dir so the final rename is same-filesystem.
    let tmp = parent.join(format!("skills.tmp-{}", Uuid::new_v4()));
    let result = build_and_swap(cfg, store, &tmp, skills_dir).await;
    if result.is_err() {
        let _ = std::fs::remove_dir_all(&tmp);
    }
    result
}

async fn build_and_swap(
    cfg: &SkillsConfig,
    store: Option<&dyn StateStore>,
    tmp: &Path,
    skills_dir: &Path,
) -> Result<()> {
    clone_repo(cfg, tmp)?;
    // Drop git internals: we neither mirror nor serve them.
    let _ = std::fs::remove_dir_all(tmp.join(".git"));

    if let Some(store) = store {
        store.push(tmp, "skills").await?;
    }

    // Atomic swap: move the old tree aside, move the new one in, delete the old.
    let parent = skills_dir.parent().unwrap();
    let backup = parent.join(format!("skills.old-{}", Uuid::new_v4()));
    if skills_dir.exists() {
        std::fs::rename(skills_dir, &backup)?;
    }
    std::fs::rename(tmp, skills_dir)?;
    let _ = std::fs::remove_dir_all(&backup);
    Ok(())
}

fn clone_repo(cfg: &SkillsConfig, dest: &Path) -> Result<()> {
    let askpass = write_askpass()?;
    let res = clone_with_askpass(cfg, dest, &askpass);
    let _ = std::fs::remove_file(&askpass);
    res
}

fn clone_with_askpass(cfg: &SkillsConfig, dest: &Path, askpass: &Path) -> Result<()> {
    let dest_s = dest.to_string_lossy().to_string();
    let run = |args: &[&str]| -> Result<bool> {
        let status = Command::new("git")
            .args(args)
            .env("GIT_ASKPASS", askpass)
            .env("GIT_TERMINAL_PROMPT", "0")
            .status()?;
        Ok(status.success())
    };

    // Shallow clone of a branch or tag (the common case).
    if run(&[
        "clone", "--depth", "1", "--branch", &cfg.git_ref, &cfg.repo, &dest_s,
    ])? {
        return Ok(());
    }
    // Fallback (e.g. `ref` is a sha that --branch can't take): full clone + checkout.
    let _ = std::fs::remove_dir_all(dest);
    if !run(&["clone", &cfg.repo, &dest_s])? {
        bail!("git clone failed for {}", cfg.repo);
    }
    if !run(&["-C", &dest_s, "checkout", &cfg.git_ref])? {
        bail!("git checkout {} failed", cfg.git_ref);
    }
    Ok(())
}

/// A one-shot `GIT_ASKPASS` helper that echoes `$CICA_SKILLS_GIT_TOKEN`. The
/// token is inherited from this process's env, so it never lands in argv or in
/// `.git/config`.
fn write_askpass() -> Result<PathBuf> {
    let path = std::env::temp_dir().join(format!("cica-askpass-{}.sh", Uuid::new_v4()));
    std::fs::write(&path, "#!/bin/sh\nprintf '%s' \"$CICA_SKILLS_GIT_TOKEN\"\n")?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))?;
    Ok(path)
}

/// Sync now, then every `refresh_secs`. Logs and keeps last-good on failure.
pub async fn run_sync_loop(
    cfg: SkillsConfig,
    store: Option<Arc<dyn StateStore>>,
    skills_dir: PathBuf,
) {
    let mut ticker = tokio::time::interval(Duration::from_secs(cfg.refresh_secs.max(1)));
    loop {
        ticker.tick().await; // fires immediately on the first call
        match sync_once(&cfg, store.as_deref(), &skills_dir).await {
            Ok(()) => info!("skills synced from {} ({})", cfg.repo, cfg.git_ref),
            Err(e) => warn!("skills sync failed (keeping last-good): {e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::state::FilesystemStateStore;

    fn make_fixture_repo(dir: &Path) {
        std::fs::create_dir_all(dir.join("myskill")).unwrap();
        std::fs::write(dir.join("myskill/SKILL.md"), "name: myskill").unwrap();
        let git = |args: &[&str]| {
            let ok = Command::new("git")
                .args(args)
                .current_dir(dir)
                .status()
                .unwrap()
                .success();
            assert!(ok, "git {args:?} failed");
        };
        git(&["init", "-b", "main"]);
        git(&["-c", "user.email=t@t", "-c", "user.name=t", "add", "."]);
        git(&["-c", "user.email=t@t", "-c", "user.name=t", "commit", "-m", "init"]);
    }

    fn cfg(repo: &Path) -> SkillsConfig {
        SkillsConfig {
            repo: repo.to_string_lossy().to_string(),
            git_ref: "main".to_string(),
            refresh_secs: 600,
        }
    }

    #[tokio::test]
    async fn sync_clones_into_skills_dir_and_store() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        make_fixture_repo(&repo);

        let skills_dir = tmp.path().join("data/skills");
        let store = FilesystemStateStore::new(tmp.path().join("store"));

        sync_once(&cfg(&repo), Some(&store), &skills_dir)
            .await
            .unwrap();

        // Landed in the skills dir, without git internals.
        assert!(skills_dir.join("myskill/SKILL.md").exists());
        assert!(!skills_dir.join(".git").exists());

        // Mirrored to the store under "skills".
        let verify = tmp.path().join("verify");
        assert!(store.pull("skills", &verify).await.unwrap());
        assert!(verify.join("myskill/SKILL.md").exists());
    }

    #[tokio::test]
    async fn sync_failure_keeps_last_good() {
        let tmp = tempfile::tempdir().unwrap();
        let skills_dir = tmp.path().join("data/skills");
        std::fs::create_dir_all(skills_dir.join("existing")).unwrap();
        std::fs::write(skills_dir.join("existing/SKILL.md"), "old").unwrap();

        let bogus = SkillsConfig {
            repo: tmp.path().join("nope").to_string_lossy().to_string(),
            git_ref: "main".to_string(),
            refresh_secs: 600,
        };
        assert!(sync_once(&bogus, None, &skills_dir).await.is_err());

        // Untouched.
        assert_eq!(
            std::fs::read_to_string(skills_dir.join("existing/SKILL.md")).unwrap(),
            "old"
        );
    }
}
```

- [ ] **Step 3: Run the tests to verify they fail, then pass**

Run: `cargo test --lib skills_sync::tests`
Expected: compiles and PASS (2 tests). (Requires `git` on PATH — present in dev/CI.)

- [ ] **Step 4: Commit**

```bash
git add src/main.rs src/skills_sync.rs
git commit -m "feat(skills): git-sync module (clone, mirror to store, atomic swap)"
```

---

### Task 3: Sync loop wiring into the router

**Files:**
- Modify: `src/skills_sync.rs` (add a loop test)
- Modify: `src/cmd/run.rs:43-47` (spawn after memory indexing / cron start)

- [ ] **Step 1: Write the failing loop test**

Add to the `tests` module in `src/skills_sync.rs`:

```rust
#[tokio::test]
async fn loop_syncs_immediately_then_can_abort() {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    make_fixture_repo(&repo);
    let skills_dir = tmp.path().join("data/skills");

    // Long interval — we only want to observe the immediate first tick.
    let mut c = cfg(&repo);
    c.refresh_secs = 3600;
    let handle = tokio::spawn(run_sync_loop(c, None, skills_dir.clone()));

    let mut landed = false;
    for _ in 0..100 {
        if skills_dir.join("myskill/SKILL.md").exists() {
            landed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    handle.abort();
    assert!(landed, "loop did not sync on the first tick");
}
```

- [ ] **Step 2: Run it to verify it passes**

Run: `cargo test --lib skills_sync::tests::loop_syncs_immediately_then_can_abort`
Expected: PASS (the loop + `sync_once` already exist from Task 2).

- [ ] **Step 3: Spawn the loop in the router**

In `src/cmd/run.rs`, after `index_all_user_memories();` and the cron-service start (after line 46), add:

```rust
    // Skills git-sync (router-side): keep skills_dir + the state store's "skills"
    // prefix fresh from the configured repo. No-op when [skills] is unset.
    if let Some(skills_cfg) = config.skills.clone() {
        match crate::config::paths() {
            Ok(paths) => {
                let store = crate::sandbox::state::default_store(&config)
                    .ok()
                    .flatten();
                tokio::spawn(crate::skills_sync::run_sync_loop(
                    skills_cfg,
                    store,
                    paths.skills_dir,
                ));
                info!("Skills sync started");
            }
            Err(e) => warn!("Failed to resolve paths for skills sync: {}", e),
        }
    }
```

- [ ] **Step 4: Verify the crate builds**

Run: `cargo build`
Expected: builds clean (no warnings about unused `skills_sync`).

- [ ] **Step 5: Commit**

```bash
git add src/skills_sync.rs src/cmd/run.rs
git commit -m "feat(skills): spawn the router skills-sync loop on startup"
```

---

### Task 4: Worker hydrates skills before the turn

**Files:**
- Modify: `src/sandbox/hydrating.rs:77` (before `// --- Run ---`)

- [ ] **Step 1: Write the failing test**

Add to the `#[cfg(test)] mod tests` block in `src/sandbox/hydrating.rs`:

```rust
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
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test --lib sandbox::hydrating::tests::hydrate_pulls_published_skills`
Expected: FAIL — `cwd/skills/foo/SKILL.md` does not exist (no skills pull yet).

- [ ] **Step 3: Add the skills pull**

In `src/sandbox/hydrating.rs`, in `run_turn`, immediately before the `// --- Run ---` comment (line 77):

```rust
        // Skills: published, read-only — pull the current set so the agent can
        // read/execute them. Absence (router hasn't synced yet) is fine.
        let _ = self.store.pull("skills", &self.cwd.join("skills")).await;
```

- [ ] **Step 4: Run it to verify it passes**

Run: `cargo test --lib sandbox::hydrating::tests::hydrate_pulls_published_skills`
Expected: PASS.

- [ ] **Step 5: Run the full hydrating suite (no regressions)**

Run: `cargo test --lib sandbox::hydrating`
Expected: PASS (all existing tests + the new one).

- [ ] **Step 6: Commit**

```bash
git add src/sandbox/hydrating.rs
git commit -m "feat(skills): hydrate published skills into the worker before a turn"
```

---

### Task 5: sprout — provide the git token to the router

**Files:**
- Modify: `/Users/dcvz/Github/sprout/lib/router-stack.ts`
- Create/Modify: `/Users/dcvz/Github/sprout/test/router-stack.test.ts`

- [ ] **Step 1: Write the failing synth test**

Create `/Users/dcvz/Github/sprout/test/router-stack.test.ts` (or add the tests if it exists):

```typescript
import * as cdk from "aws-cdk-lib";
import { Template, Match } from "aws-cdk-lib/assertions";
import { SproutFleetStack } from "../lib/fleet-stack";
import { SproutRouterStack } from "../lib/router-stack";

function synth() {
  const app = new cdk.App({ context: { efsFileSystemId: "fs-deadbeef" } });
  const env = { account: "974767452524", region: "eu-central-1" };
  const fleet = new SproutFleetStack(app, "SproutFleetStack", { env });
  const router = new SproutRouterStack(app, "SproutRouterStack", { env, fleet });
  return Template.fromStack(router);
}

test("creates the skills git-token secret", () => {
  const t = synth();
  t.hasResourceProperties("AWS::SecretsManager::Secret", {
    Name: "sprout/skills-git-token",
  });
});

test("router role can read a secret", () => {
  const t = synth();
  t.hasResourceProperties("AWS::IAM::Policy", {
    PolicyDocument: {
      Statement: Match.arrayWith([
        Match.objectLike({
          Action: Match.arrayWith(["secretsmanager:GetSecretValue"]),
        }),
      ]),
    },
  });
});
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cd /Users/dcvz/Github/sprout && pnpm test -- router-stack`
Expected: FAIL — no `sprout/skills-git-token` secret / no `secretsmanager:GetSecretValue`.

- [ ] **Step 3: Add the secret + grant + user-data wiring**

In `/Users/dcvz/Github/sprout/lib/router-stack.ts`:

Add the import near the top (after line 4):

```typescript
import * as secretsmanager from "aws-cdk-lib/aws-secretsmanager";
```

After the `role` is created and granted (after line 85, `props.fleet.stateBucket.grantReadWrite(role);`), add:

```typescript
    // Read-only GitHub token for cloning the private ai-skills repo. Operator
    // fills the value (a fine-grained token: repo root-global/ai-skills,
    // contents:read). Fetched at service start into /etc/cica.env (below) and
    // read by cica as CICA_SKILLS_GIT_TOKEN.
    const skillsGitToken = new secretsmanager.Secret(this, "SkillsGitToken", {
      secretName: "sprout/skills-git-token",
      description: "GitHub token (read-only) for cloning root-global/ai-skills",
      removalPolicy: cdk.RemovalPolicy.RETAIN,
    });
    skillsGitToken.grantRead(role);
```

In the `userData.addCommands(...)` block, change the apt install line (line 90) to add `git` and `awscli`:

```typescript
      "DEBIAN_FRONTEND=noninteractive apt-get install -y nfs-common curl git awscli",
```

Immediately before the `cat > /etc/systemd/system/cica.service` command (before line 101), add the token-fetch script:

```typescript
      // Fetch the skills git token into an EnvironmentFile at boot/start. Runs
      // as root via the unit's ExecStartPre (the '+' prefix) so cica (User=ubuntu)
      // gets CICA_SKILLS_GIT_TOKEN without the token living in the unit file.
      `cat > /usr/local/bin/cica-skills-token.sh << 'TOKFILE'
#!/bin/sh
TOKEN=$(aws secretsmanager get-secret-value --secret-id sprout/skills-git-token --query SecretString --output text --region ${this.region} 2>/dev/null || true)
printf 'CICA_SKILLS_GIT_TOKEN=%s\\n' "$TOKEN" > /etc/cica.env
chmod 600 /etc/cica.env
TOKFILE`,
      "chmod 755 /usr/local/bin/cica-skills-token.sh",
```

In the systemd unit heredoc (the `[Service]` section, around lines 107-113), add the `EnvironmentFile` and `ExecStartPre` lines so the block reads:

```
[Service]
Type=simple
User=ubuntu
ExecStartPre=+/usr/local/bin/cica-skills-token.sh
EnvironmentFile=-/etc/cica.env
ExecStart=/usr/local/bin/cica
Restart=always
RestartSec=10
Environment=HOME=/home/ubuntu
```

- [ ] **Step 4: Run the synth test to verify it passes**

Run: `cd /Users/dcvz/Github/sprout && pnpm test -- router-stack`
Expected: PASS (2 tests).

- [ ] **Step 5: Verify the full app still synthesizes**

Run: `cd /Users/dcvz/Github/sprout && pnpm test && pnpm cdk synth SproutRouterStack -c efsFileSystemId=fs-deadbeef >/dev/null && echo SYNTH_OK`
Expected: all tests pass, prints `SYNTH_OK`.

- [ ] **Step 6: Commit**

```bash
cd /Users/dcvz/Github/sprout
git add lib/router-stack.ts test/router-stack.test.ts
git commit -m "feat(router): provide CICA_SKILLS_GIT_TOKEN + git for skills sync"
```

---

### Task 6: Document the deployment steps

**Files:**
- Modify: `/Users/dcvz/Github/sprout/RUNBOOK.md`

- [ ] **Step 1: Add the populate-secret step**

In `RUNBOOK.md`, under `## 1. Deploy the fleet`, after the AI-keys `put-secret-value` block, add:

```markdown
- Populate the skills git token (once) — a fine-grained GitHub token with
  `contents:read` on `root-global/ai-skills`:
  `aws secretsmanager put-secret-value --secret-id sprout/skills-git-token \
     --secret-string 'github_pat_xxx'`
  (The router fetches this into `/etc/cica.env` at service start; workers never see it.)
```

- [ ] **Step 2: Add the `[skills]` config block**

In `RUNBOOK.md`, under `## 4. Reconfigure + start the new router`, inside the `config.toml` edit block, add after the `[deployment.fargate]` section:

```markdown
  [skills]
  repo = "https://github.com/root-global/ai-skills"
  ref  = "main"
  refresh_secs = 600
```

And add a sentence after the code block:

```markdown
- The router will clone the skills repo on start and every `refresh_secs`,
  mirror it to S3 under `skills/`, and workers hydrate it per turn. If the token
  or repo is unreachable the router keeps the last-good skills and logs a warning.
```

- [ ] **Step 3: Commit**

```bash
cd /Users/dcvz/Github/sprout
git add RUNBOOK.md
git commit -m "docs(runbook): skills git token + [skills] router config"
```

---

## Self-Review

**Spec coverage:**
- `[skills]` config (repo/ref/refresh_secs), router-only, token via env → Task 1 (+ env read in Task 2's `GIT_ASKPASS`). ✓
- Router periodic git-sync: shallow clone, `GIT_ASKPASS`, atomic swap, keep-last-good, push `"skills"` → Task 2. ✓
- Cloud-agnostic via `StateStore` (no new trait methods; uses `push`/`pull`) → Tasks 2 & 4. ✓
- Worker reads-only hydration of `"skills"`, graceful-empty → Task 4. ✓
- Router spawns the loop only when `[skills]` set; store optional (single-box still refreshes) → Task 3. ✓
- Deployment requirements: token on router from secret, `git` on host, `[skills]` in router config → Tasks 5 & 6. ✓
- Deferred (draft-survival, publish-as-PR): not in any task. ✓ (correctly out of scope)

**Type consistency:** `SkillsConfig { repo, git_ref (serde "ref"), refresh_secs }` is defined in Task 1 and constructed identically in Tasks 2/3 tests and consumed in `run.rs` (Task 3). `sync_once(cfg, Option<&dyn StateStore>, &Path)` and `run_sync_loop(SkillsConfig, Option<Arc<dyn StateStore>>, PathBuf)` signatures match their call sites. `store.pull("skills", &self.cwd.join("skills"))` (Task 4) matches the key written by `store.push(tmp, "skills")` (Task 2). ✓

**Placeholder scan:** none — every code/test step shows complete code and exact commands.

**Note for the implementer:** the `skills_sync` tests shell out to real `git`; ensure `git` is installed in the dev/CI environment (it is on this machine and standard CI images).
