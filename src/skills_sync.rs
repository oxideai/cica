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

use anyhow::{Result, bail};
use tracing::{info, warn};
use uuid::Uuid;

use crate::atomic::Staging;
use crate::config::SkillsConfig;
use crate::sandbox::state::{StateStore, clear_dir};

/// Clone `cfg.repo`@`cfg.git_ref`, mirror to `store` (if any) under "skills",
/// and atomically replace `skills_dir`. Leaves `skills_dir` untouched on error.
pub async fn sync_once(
    cfg: &SkillsConfig,
    store: Option<&dyn StateStore>,
    skills_dir: &Path,
) -> Result<()> {
    let staging = Staging::beside(skills_dir)?;
    clone_repo(cfg, staging.path())?;
    let _ = std::fs::remove_dir_all(staging.path().join(".git"));

    if let Some(store) = store {
        store.push(staging.path(), "skills").await?;
    }
    staging.commit()?;
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
        "clone",
        "--depth",
        "1",
        "--branch",
        &cfg.git_ref,
        &cfg.repo,
        &dest_s,
    ])? {
        return Ok(());
    }
    // Fallback (e.g. `ref` is a sha that --branch can't take): full clone + checkout.
    clear_dir(dest)?;
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
        git(&[
            "-c",
            "user.email=t@t",
            "-c",
            "user.name=t",
            "commit",
            "-m",
            "init",
        ]);
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
}
