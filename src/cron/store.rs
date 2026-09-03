//! Persistent storage for cron jobs.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::config::Paths;

use super::schedule::CronSchedule;

pub type JobId = String;

/// Where to deliver cron job results.
///
/// Generic across platforms. For single-channel platforms (Telegram, Signal),
/// the target is always the owner's DM (represented by the default).
/// For multi-channel platforms (Slack, Discord), the target can be a specific
/// channel ID and optionally a thread ID.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct DeliveryTarget {
    /// Target channel/conversation ID (e.g., Slack channel ID "C0123456789").
    /// When None, delivers to the owner's DM (the user_id field on the job).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel_id: Option<String>,

    /// Optional thread ID for threaded delivery (e.g., Slack thread_ts).
    /// Only meaningful when channel_id is set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
}

impl DeliveryTarget {
    /// Create a target that delivers to a specific channel.
    pub fn channel(channel_id: String) -> Self {
        Self {
            channel_id: Some(channel_id),
            thread_id: None,
        }
    }

    /// Resolve the effective channel_id for delivery.
    /// Falls back to user_id (owner DM) when no explicit channel is set.
    pub fn resolve_channel_id<'a>(&'a self, user_id: &'a str) -> &'a str {
        self.channel_id.as_deref().unwrap_or(user_id)
    }
}

/// Status of last job execution.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(tag = "status", content = "error")]
pub enum JobStatus {
    #[default]
    Pending,
    Running,
    Success,
    Failed(String),
}

impl JobStatus {
    pub fn as_str(&self) -> &str {
        match self {
            JobStatus::Pending => "pending",
            JobStatus::Running => "running",
            JobStatus::Success => "success",
            JobStatus::Failed(_) => "failed",
        }
    }
}

/// Runtime state for a job (mutable between runs).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CronJobState {
    pub next_run_at: Option<u64>,
    pub last_run_at: Option<u64>,
    #[serde(default)]
    pub last_status: JobStatus,
    pub last_duration_ms: Option<u64>,
    #[serde(default)]
    pub failure_count: u32,
}

/// A scheduled cron job.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CronJob {
    pub id: JobId,
    pub name: String,
    pub prompt: String,
    pub schedule: CronSchedule,
    /// Owner: channel name (e.g., "telegram", "signal").
    pub channel: String,
    /// Owner: user ID within the channel.
    pub user_id: String,
    /// Where to deliver results. Defaults to owner's DM when absent.
    #[serde(default)]
    pub target: DeliveryTarget,
    /// Whether to send results back to the user's chat.
    #[serde(default = "default_true")]
    pub notify: bool,
    /// Job is enabled (can be paused).
    #[serde(default = "default_true")]
    pub enabled: bool,
    pub created_at: u64,
    #[serde(default)]
    pub state: CronJobState,
}

fn default_true() -> bool {
    true
}

impl CronJob {
    /// Create a new job with generated ID.
    pub fn new(
        name: String,
        prompt: String,
        schedule: CronSchedule,
        channel: String,
        user_id: String,
        target: Option<DeliveryTarget>,
    ) -> Self {
        let now = now_millis();
        let mut job = Self {
            id: generate_job_id(),
            name,
            prompt,
            schedule,
            channel,
            user_id,
            target: target.unwrap_or_default(),
            notify: true,
            enabled: true,
            created_at: now,
            state: CronJobState::default(),
        };
        job.update_next_run(now);
        job
    }

    /// Calculate and update next_run_at based on given time.
    pub fn update_next_run(&mut self, now_ms: u64) {
        self.state.next_run_at = self.schedule.next_run_after(now_ms);
    }

    /// Check if this job is due to run.
    pub fn is_due(&self, now_ms: u64) -> bool {
        self.enabled && self.state.next_run_at.is_some_and(|t| t <= now_ms)
    }

    /// Short ID for display (first 8 chars).
    pub fn short_id(&self) -> &str {
        if self.id.len() > 8 {
            &self.id[..8]
        } else {
            &self.id
        }
    }
}

/// Persistent storage for cron jobs.
/// Follows PairingStore pattern with JSON file persistence.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CronStore {
    #[serde(skip)]
    path: PathBuf,
    /// All jobs indexed by ID.
    pub jobs: HashMap<JobId, CronJob>,
}

impl CronStore {
    /// Load cron store from disk.
    pub fn load(paths: &Paths) -> Result<Self> {
        let path = paths.base.join("cron.json");

        if !path.exists() {
            return Ok(Self {
                path,
                ..Self::default()
            });
        }

        let content = std::fs::read_to_string(&path)
            .with_context(|| format!("Failed to read cron file: {:?}", path))?;

        let mut store: Self = serde_json::from_str(&content)
            .with_context(|| format!("Failed to parse cron file: {:?}", path))?;
        store.path = path;

        Ok(store)
    }

    /// Save cron store to disk.
    pub fn save(&self) -> Result<()> {
        let content = serde_json::to_string_pretty(self)?;
        std::fs::write(&self.path, content)?;

        Ok(())
    }

    /// Add a new job.
    pub fn add(&mut self, job: CronJob) -> Result<JobId> {
        let id = job.id.clone();
        self.jobs.insert(id.clone(), job);
        self.save()?;

        Ok(id)
    }

    /// Remove a job by ID (only if user owns it).
    pub fn remove(&mut self, id: &str, channel: &str, user_id: &str) -> Result<Option<CronJob>> {
        // Check ownership first
        if let Some(job) = self.jobs.get(id)
            && (job.channel != channel || job.user_id != user_id)
        {
            anyhow::bail!("You don't own this job");
        }

        let removed = self.jobs.remove(id);
        if removed.is_some() {
            self.save()?;
        }

        Ok(removed)
    }

    /// List jobs for a specific user.
    pub fn list_for_user(&self, channel: &str, user_id: &str) -> Vec<&CronJob> {
        self.jobs
            .values()
            .filter(|j| j.channel == channel && j.user_id == user_id)
            .collect()
    }

    /// Get a job by ID (with ownership check).
    pub fn get(&self, id: &str, channel: &str, user_id: &str) -> Option<&CronJob> {
        self.jobs
            .get(id)
            .filter(|j| j.channel == channel && j.user_id == user_id)
    }

    /// Get mutable reference (internal use, no ownership check).
    pub fn get_mut(&mut self, id: &str) -> Option<&mut CronJob> {
        self.jobs.get_mut(id)
    }

    /// Get all jobs that are due to run.
    pub fn get_due_jobs(&self, now_ms: u64) -> Vec<&CronJob> {
        self.jobs.values().filter(|j| j.is_due(now_ms)).collect()
    }

    /// Reset any jobs stuck in Running state (e.g., after a crash).
    /// Recalculates next_run_at so they get scheduled again.
    pub fn recover_stuck_jobs(&mut self, now_ms: u64) -> usize {
        let stuck_ids: Vec<JobId> = self
            .jobs
            .values()
            .filter(|j| j.state.last_status == JobStatus::Running)
            .map(|j| j.id.clone())
            .collect();

        let count = stuck_ids.len();
        for id in &stuck_ids {
            if let Some(job) = self.jobs.get_mut(id) {
                job.state.last_status = JobStatus::Success;
                job.update_next_run(now_ms);
            }
        }

        count
    }

    /// Merge disk state into the current store, preserving in-flight job states.
    /// Jobs currently marked as Running in memory keep their in-memory state
    /// to avoid losing completion updates from concurrent tasks.
    pub fn merge_from_disk(&mut self, disk: CronStore) {
        let running_ids: std::collections::HashSet<String> = self
            .jobs
            .values()
            .filter(|j| j.state.last_status == JobStatus::Running)
            .map(|j| j.id.clone())
            .collect();

        let disk_ids: std::collections::HashSet<String> = disk.jobs.keys().cloned().collect();

        for (id, disk_job) in disk.jobs {
            if running_ids.contains(&id) {
                continue;
            }
            self.jobs.insert(id, disk_job);
        }

        self.jobs
            .retain(|id, _| disk_ids.contains(id) || running_ids.contains(id));
    }
}

/// Generate a unique job ID.
fn generate_job_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// Get current time in milliseconds.
pub fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_job_creation() {
        let job = CronJob::new(
            "Test Job".to_string(),
            "Test prompt".to_string(),
            CronSchedule::Every(60_000),
            "telegram".to_string(),
            "12345".to_string(),
            None,
        );

        assert!(!job.id.is_empty());
        assert_eq!(job.name, "Test Job");
        assert_eq!(job.channel, "telegram");
        assert!(job.enabled);
        assert!(job.notify);
        assert!(job.state.next_run_at.is_some());
        assert_eq!(job.target, DeliveryTarget::default());
    }

    #[test]
    fn test_job_creation_with_target() {
        let job = CronJob::new(
            "Test Job".to_string(),
            "Test prompt".to_string(),
            CronSchedule::Every(60_000),
            "slack".to_string(),
            "U12345".to_string(),
            Some(DeliveryTarget::channel("C98765".to_string())),
        );

        assert_eq!(
            job.target,
            DeliveryTarget {
                channel_id: Some("C98765".to_string()),
                thread_id: None,
            }
        );
    }

    #[test]
    fn test_job_due_check() {
        let mut job = CronJob::new(
            "Test".to_string(),
            "Test".to_string(),
            CronSchedule::Every(60_000),
            "test".to_string(),
            "user1".to_string(),
            None,
        );

        // Set next_run to 1000
        job.state.next_run_at = Some(1000);

        assert!(!job.is_due(500)); // Before
        assert!(job.is_due(1000)); // Exact
        assert!(job.is_due(1500)); // After

        // Disabled job should never be due
        job.enabled = false;
        assert!(!job.is_due(1500));
    }

    #[test]
    fn test_delivery_target_resolve() {
        let default_target = DeliveryTarget::default();
        assert_eq!(default_target.resolve_channel_id("U12345"), "U12345");

        let channel_target = DeliveryTarget::channel("C98765".to_string());
        assert_eq!(channel_target.resolve_channel_id("U12345"), "C98765");
    }

    #[test]
    fn test_delivery_target_serde_backward_compat() {
        // Simulate a CronJob JSON without a target field (old format)
        let json = r#"{
            "id": "test-id",
            "name": "Test",
            "prompt": "Hello",
            "schedule": {"type": "Every", "value": 60000},
            "channel": "telegram",
            "user_id": "12345",
            "notify": true,
            "enabled": true,
            "created_at": 1700000000000,
            "state": {}
        }"#;

        let job: CronJob = serde_json::from_str(json).unwrap();
        assert_eq!(job.target, DeliveryTarget::default());
        assert!(job.target.channel_id.is_none());
        assert!(job.target.thread_id.is_none());
    }
}
