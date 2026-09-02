use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use crate::audit;
use crate::config::Paths;

/// How long a pairing code remains valid
const CODE_TTL: Duration = Duration::from_secs(60 * 60); // 1 hour

/// Characters used for code generation (no ambiguous chars: 0/O, 1/I)
const CODE_ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
const CODE_LENGTH: usize = 8;

/// A pending pairing request
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingRequest {
    pub code: String,
    pub channel: String,
    pub user_id: String,
    pub username: Option<String>,
    pub display_name: Option<String>,
    pub created_at: u64, // Unix timestamp
}

/// Per-user profile data
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct UserProfile {
    pub name: Option<String>,
    pub pronouns: Option<String>,
    pub location: Option<String>,
    pub timezone: Option<String>,
    pub notes: Option<String>,
    pub onboarding_complete: bool,
}

/// Storage for all pairing data
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PairingStore {
    #[serde(skip)]
    path: PathBuf,
    pub pending: Vec<PendingRequest>,
    pub approved: HashMap<String, Vec<String>>, // channel -> [user_ids]
    #[serde(default)]
    pub sessions: HashMap<String, String>, // "channel:user_id" -> session_id (UUID)
    #[serde(default)]
    pub user_profiles: HashMap<String, UserProfile>, // "channel:user_id" -> profile
}

impl PairingStore {
    /// Load pairing store from disk
    pub fn load(paths: &Paths) -> Result<Self> {
        let path = paths.pairing_file.clone();

        if !path.exists() {
            return Ok(Self {
                path,
                ..Self::default()
            });
        }

        let content = std::fs::read_to_string(&path)
            .with_context(|| format!("Failed to read pairing file: {:?}", path))?;

        let mut store: Self = serde_json::from_str(&content)
            .with_context(|| format!("Failed to parse pairing file: {:?}", path))?;
        store.path = path;

        Ok(store)
    }

    /// Save pairing store to disk
    pub fn save(&self) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let content = serde_json::to_string_pretty(self)?;
        std::fs::write(&self.path, content)?;

        Ok(())
    }

    /// Remove expired pending requests
    pub fn prune_expired(&mut self) {
        let now = now_timestamp();
        let ttl_secs = CODE_TTL.as_secs();

        self.pending
            .retain(|req| now.saturating_sub(req.created_at) < ttl_secs);
    }

    /// Check if a user is approved for a channel
    pub fn is_approved(&self, channel: &str, user_id: &str) -> bool {
        self.approved
            .get(channel)
            .map(|ids| ids.contains(&user_id.to_string()))
            .unwrap_or(false)
    }

    /// Get or create a pending request for a user
    /// Returns (code, is_new)
    pub fn get_or_create_pending(
        &mut self,
        channel: &str,
        user_id: &str,
        username: Option<String>,
        display_name: Option<String>,
    ) -> Result<(String, bool)> {
        self.prune_expired();

        if let Some(existing) = self
            .pending
            .iter()
            .find(|r| r.channel == channel && r.user_id == user_id)
        {
            return Ok((existing.code.clone(), false));
        }

        let code = generate_unique_code(&self.pending)?;

        let request = PendingRequest {
            code: code.clone(),
            channel: channel.to_string(),
            user_id: user_id.to_string(),
            username,
            display_name,
            created_at: now_timestamp(),
        };

        self.pending.push(request);
        self.save()?;

        audit::log_event(
            "pairing_requested",
            Some(channel),
            Some(user_id),
            Some(&format!("{{\"code\":\"{}\"}}", code)),
        );

        Ok((code, true))
    }

    /// Approve a pending request by code
    /// Returns the approved request details on success
    pub fn approve(&mut self, code: &str) -> Result<PendingRequest> {
        self.prune_expired();

        let code_upper = code.to_uppercase();

        let idx = self
            .pending
            .iter()
            .position(|r| r.code == code_upper)
            .ok_or_else(|| anyhow!("No pending request found for code: {}", code))?;

        let request = self.pending.remove(idx);

        self.approved
            .entry(request.channel.clone())
            .or_default()
            .push(request.user_id.clone());

        self.save()?;

        let detail = format!(
            "{{\"code\":\"{}\",\"username\":{}}}",
            code_upper,
            request
                .username
                .as_deref()
                .map(|u| format!("\"{}\"", u))
                .unwrap_or_else(|| "null".to_string()),
        );
        audit::log_event(
            "user_approved",
            Some(&request.channel),
            Some(&request.user_id),
            Some(&detail),
        );

        Ok(request)
    }

    /// Automatically approve a user without requiring a pairing code
    pub fn auto_approve(
        &mut self,
        channel: &str,
        user_id: &str,
        username: Option<String>,
        _display_name: Option<String>,
    ) -> Result<()> {
        self.approved
            .entry(channel.to_string())
            .or_default()
            .push(user_id.to_string());
        self.save()?;

        let detail = username
            .as_deref()
            .map(|u| format!("{{\"username\":\"{}\"}}", u))
            .unwrap_or_else(|| "{}".to_string());
        audit::log_event(
            "user_auto_approved",
            Some(channel),
            Some(user_id),
            Some(&detail),
        );

        Ok(())
    }
}

/// Generate a unique pairing code
fn generate_unique_code(existing: &[PendingRequest]) -> Result<String> {
    use std::collections::HashSet;

    let existing_codes: HashSet<&str> = existing.iter().map(|r| r.code.as_str()).collect();

    for _ in 0..100 {
        let code = generate_code();
        if !existing_codes.contains(code.as_str()) {
            return Ok(code);
        }
    }

    Err(anyhow!("Failed to generate unique code after 100 attempts"))
}

/// Generate a random code
fn generate_code() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};

    // Simple randomness from system time + process id
    let seed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos() as u64
        ^ std::process::id() as u64;

    let mut rng = SimpleRng::new(seed);

    (0..CODE_LENGTH)
        .map(|_| {
            let idx = rng.next() as usize % CODE_ALPHABET.len();
            CODE_ALPHABET[idx] as char
        })
        .collect()
}

/// Simple PRNG for code generation (no external deps)
struct SimpleRng(u64);

impl SimpleRng {
    fn new(seed: u64) -> Self {
        Self(seed)
    }

    fn next(&mut self) -> u64 {
        // xorshift64
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

fn now_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}
