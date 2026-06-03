//! Maps a Claude session id to its on-disk files and captures/restores them.
//!
//! Capture finds files by session id (slug-independent). Restore writes the
//! transcript under the slug of the *current* cwd, so a worker with a different
//! cwd in a later phase still resumes correctly.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::sandbox::state::{clear_dir, copy_dir_all, copy_path};

/// Backend-specific capture/restore of a session's on-disk state.
///
/// `home` is the backend's HOME dir (claude_home or cursor_home). `capture`
/// copies the files making up `session_id` into `staging` (returns false if the
/// session isn't found); `restore` reinstates them under `home` so a resume run
/// with `cwd` finds them.
pub trait SessionArtifacts {
    fn capture(&self, home: &Path, session_id: &str, staging: &Path) -> Result<bool>;
    fn restore(&self, home: &Path, cwd: &Path, session_id: &str, staging: &Path) -> Result<()>;
}

/// Slugify a working directory the way Claude Code names its project dir:
/// every non-alphanumeric character becomes `-`.
pub fn claude_project_slug(cwd: &Path) -> String {
    cwd.to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// Capture/restore of Claude session files.
pub struct ClaudeSessionArtifacts;

impl ClaudeSessionArtifacts {
    /// Copy the files making up `session_id` from `claude_home` into `staging`,
    /// laid out as `transcript.jsonl`, `session-env`, and `todos/`.
    /// Returns `false` (capturing nothing) if no transcript is found.
    pub fn capture(claude_home: &Path, session_id: &str, staging: &Path) -> Result<bool> {
        let dot = claude_home.join(".claude");
        clear_dir(staging)?;

        // Transcript: find <session_id>.jsonl under any projects/<slug>/ dir.
        let projects = dot.join("projects");
        let mut transcript: Option<PathBuf> = None;
        if projects.is_dir() {
            for entry in fs::read_dir(&projects)? {
                let candidate = entry?.path().join(format!("{session_id}.jsonl"));
                if candidate.is_file() {
                    transcript = Some(candidate);
                    break;
                }
            }
        }
        let Some(transcript) = transcript else {
            return Ok(false);
        };
        fs::copy(&transcript, staging.join("transcript.jsonl"))?;

        // session-env/<id> (file or dir), if present.
        let env_src = dot.join("session-env").join(session_id);
        if env_src.exists() {
            copy_path(&env_src, &staging.join("session-env"))?;
        }

        // todos/<id>-*.json, if present.
        let todos_src = dot.join("todos");
        if todos_src.is_dir() {
            let prefix = format!("{session_id}-");
            let staged_todos = staging.join("todos");
            for entry in fs::read_dir(&todos_src)? {
                let entry = entry?;
                let name = entry.file_name();
                if name.to_string_lossy().starts_with(&prefix) {
                    fs::create_dir_all(&staged_todos)?;
                    fs::copy(entry.path(), staged_todos.join(&name))?;
                }
            }
        }
        Ok(true)
    }

    /// Restore staged artifacts into `claude_home` so `claude --resume
    /// <session_id>` (run with `cwd`) finds them.
    pub fn restore(claude_home: &Path, cwd: &Path, session_id: &str, staging: &Path) -> Result<()> {
        let dot = claude_home.join(".claude");

        let transcript = staging.join("transcript.jsonl");
        if transcript.is_file() {
            let proj = dot.join("projects").join(claude_project_slug(cwd));
            fs::create_dir_all(&proj)?;
            fs::copy(&transcript, proj.join(format!("{session_id}.jsonl")))?;
        }

        let env_staged = staging.join("session-env");
        if env_staged.exists() {
            let env_dst = dot.join("session-env").join(session_id);
            if let Some(parent) = env_dst.parent() {
                fs::create_dir_all(parent)?;
            }
            // Remove any stale destination so a file<->dir type change can't
            // make the copy fail.
            if env_dst.exists() {
                if env_dst.is_dir() {
                    fs::remove_dir_all(&env_dst)?;
                } else {
                    fs::remove_file(&env_dst)?;
                }
            }
            copy_path(&env_staged, &env_dst)?;
        }

        // `.claude/todos/` is shared across ALL sessions (files are named
        // `<session_id>-agent-*.json`), so we merge our session's todos in
        // rather than clearing the directory, which would delete other
        // sessions' todos.
        let todos_staged = staging.join("todos");
        if todos_staged.is_dir() {
            copy_dir_all(&todos_staged, &dot.join("todos"))?;
        }
        Ok(())
    }
}

impl SessionArtifacts for ClaudeSessionArtifacts {
    fn capture(&self, home: &Path, session_id: &str, staging: &Path) -> Result<bool> {
        ClaudeSessionArtifacts::capture(home, session_id, staging)
    }
    fn restore(&self, home: &Path, cwd: &Path, session_id: &str, staging: &Path) -> Result<()> {
        ClaudeSessionArtifacts::restore(home, cwd, session_id, staging)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_matches_known_example() {
        let cwd = Path::new("/Users/dcvz/Library/Application Support/cica");
        assert_eq!(
            claude_project_slug(cwd),
            "-Users-dcvz-Library-Application-Support-cica"
        );
    }

    fn write(path: &Path, contents: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    #[test]
    fn capture_then_restore_reproduces_files() {
        let id = "abc-123";
        let cwd = Path::new("/work/cica");
        let slug = claude_project_slug(cwd);

        let home_a = tempfile::tempdir().unwrap();
        let dot_a = home_a.path().join(".claude");
        write(
            &dot_a
                .join("projects")
                .join(&slug)
                .join(format!("{id}.jsonl")),
            "line1\n",
        );
        write(&dot_a.join("session-env").join(id), "ENV=1");
        write(
            &dot_a.join("todos").join(format!("{id}-agent-{id}.json")),
            "[]",
        );

        let staging = tempfile::tempdir().unwrap();
        assert!(ClaudeSessionArtifacts::capture(home_a.path(), id, staging.path()).unwrap());

        let home_b = tempfile::tempdir().unwrap();
        ClaudeSessionArtifacts::restore(home_b.path(), cwd, id, staging.path()).unwrap();

        let dot_b = home_b.path().join(".claude");
        assert_eq!(
            fs::read_to_string(
                dot_b
                    .join("projects")
                    .join(&slug)
                    .join(format!("{id}.jsonl"))
            )
            .unwrap(),
            "line1\n"
        );
        assert_eq!(
            fs::read_to_string(dot_b.join("session-env").join(id)).unwrap(),
            "ENV=1"
        );
        assert_eq!(
            fs::read_to_string(dot_b.join("todos").join(format!("{id}-agent-{id}.json"))).unwrap(),
            "[]"
        );
    }

    #[test]
    fn capture_returns_false_without_transcript() {
        let home = tempfile::tempdir().unwrap();
        let staging = tempfile::tempdir().unwrap();
        assert!(!ClaudeSessionArtifacts::capture(home.path(), "no-such", staging.path()).unwrap());
    }

    #[test]
    fn claude_via_trait_round_trips() {
        let artifacts: &dyn SessionArtifacts = &ClaudeSessionArtifacts;
        let id = "abc-123";
        let cwd = Path::new("/work/cica");
        let slug = claude_project_slug(cwd);

        let home_a = tempfile::tempdir().unwrap();
        write(
            &home_a.path().join(".claude").join("projects").join(&slug).join(format!("{id}.jsonl")),
            "line1\n",
        );
        let staging = tempfile::tempdir().unwrap();
        assert!(artifacts.capture(home_a.path(), id, staging.path()).unwrap());

        let home_b = tempfile::tempdir().unwrap();
        artifacts.restore(home_b.path(), cwd, id, staging.path()).unwrap();
        assert_eq!(
            std::fs::read_to_string(
                home_b.path().join(".claude").join("projects").join(&slug).join(format!("{id}.jsonl"))
            ).unwrap(),
            "line1\n"
        );
    }
}
