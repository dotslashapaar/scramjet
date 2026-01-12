//! Scramjet Shield: Hot-swappable blocklist manager for malicious validator filtering.
//!
//! This module provides a memory-resident blocklist that filters out known malicious
//! validators from the leader schedule.
//!
//! **Design: Local-First**
//! - Primary: Local file (`blocklist.txt`) - user maintains their own list
//! - Optional: Remote URL sync if configured via `SCRAMJET_BLOCKLIST_URL`
//! - Fail-safe: never overwrites good data with empty responses

use log::{debug, info, warn};
use solana_sdk::pubkey::Pubkey;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;

/// Default refresh interval for file watching (5 minutes)
const DEFAULT_REFRESH_INTERVAL: Duration = Duration::from_secs(300);

/// Handle type for sharing blocklist across components
pub type BlocklistHandle = Arc<RwLock<HashSet<Pubkey>>>;

/// BlocklistManager handles loading, persisting, and refreshing the blocklist.
///
/// Architecture:
/// - Hot path (reads): O(1) HashSet lookup via shared RwLock (non-blocking for readers)
/// - Cold path (writes): Background task acquires write lock periodically
///
/// **Usage:**
/// 1. Create a `blocklist.txt` file with one validator pubkey per line
/// 2. The Shield loads it on startup and periodically checks for changes
/// 3. Optionally set `SCRAMJET_BLOCKLIST_URL` for remote sync
pub struct BlocklistManager {
    /// The shared blocklist data structure
    blocklist: BlocklistHandle,
    /// Local file path for the blocklist
    local_path: PathBuf,
    /// Optional remote URL for updates (None = local-only mode)
    remote_url: Option<String>,
    /// Refresh interval (for file watching or remote sync)
    refresh_interval: Duration,
}

impl BlocklistManager {
    /// Create a new BlocklistManager with default settings (local-only).
    ///
    /// Defaults:
    /// - Local path: `./blocklist.txt`
    /// - Remote URL: None (local-only)
    /// - Refresh interval: 5 minutes
    pub fn new() -> Self {
        Self::with_config(PathBuf::from("./blocklist.txt"), None, DEFAULT_REFRESH_INTERVAL)
    }

    /// Create a BlocklistManager with custom configuration.
    pub fn with_config(
        local_path: PathBuf,
        remote_url: Option<String>,
        refresh_interval: Duration,
    ) -> Self {
        Self {
            blocklist: Arc::new(RwLock::new(HashSet::new())),
            local_path,
            remote_url,
            refresh_interval,
        }
    }

    /// Create from environment variables with fallback to defaults.
    ///
    /// Environment variables:
    /// - `SCRAMJET_BLOCKLIST_FILE`: Local file path (default: `./blocklist.txt`)
    /// - `SCRAMJET_BLOCKLIST_URL`: Optional remote URL (default: none, local-only)
    /// - `SCRAMJET_BLOCKLIST_REFRESH_SECS`: Refresh interval in seconds (default: 300)
    pub fn from_env() -> Self {
        let local_path = std::env::var("SCRAMJET_BLOCKLIST_FILE")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("./blocklist.txt"));

        // Remote URL is OPTIONAL - only set if explicitly configured
        let remote_url = std::env::var("SCRAMJET_BLOCKLIST_URL").ok();

        let refresh_interval = std::env::var("SCRAMJET_BLOCKLIST_REFRESH_SECS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .map(Duration::from_secs)
            .unwrap_or(DEFAULT_REFRESH_INTERVAL);

        Self::with_config(local_path, remote_url, refresh_interval)
    }

    /// Get a handle to the blocklist for injection into Cartographer.
    ///
    /// This handle can be cloned and shared across threads safely.
    pub fn get_handle(&self) -> BlocklistHandle {
        self.blocklist.clone()
    }

    /// Load blocklist from local file (for fast boot).
    ///
    /// Returns the number of valid pubkeys loaded.
    pub async fn load_local(&self) -> usize {
        match self.load_from_file(&self.local_path).await {
            Ok(keys) => {
                let count = keys.len();
                if count > 0 {
                    let mut guard = self.blocklist.write().await;
                    *guard = keys;
                    info!(
                        "Shield: Loaded {} blocked validators from {:?}",
                        count, self.local_path
                    );
                } else {
                    info!(
                        "Shield: Blocklist {:?} is empty. No validators blocked.",
                        self.local_path
                    );
                }
                count
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                info!(
                    "Shield: No blocklist file at {:?}. Create one to block malicious validators.",
                    self.local_path
                );
                0
            }
            Err(e) => {
                warn!(
                    "Shield: Failed to load blocklist {:?}: {}",
                    self.local_path, e
                );
                0
            }
        }
    }

    /// Fetch blocklist from remote URL and update if valid.
    ///
    /// Safety checks:
    /// - Rejects empty responses (protects against accidental deletion)
    /// - Rejects HTTP errors (404, 500, etc.)
    /// - On success, persists to local file for next boot
    ///
    /// Returns Err if no remote URL is configured.
    pub async fn fetch_remote(&self) -> Result<usize, String> {
        let url = self
            .remote_url
            .as_ref()
            .ok_or_else(|| "No remote URL configured (local-only mode)".to_string())?;

        debug!("Shield: Fetching blocklist from {}", url);

        let response = reqwest::get(url)
            .await
            .map_err(|e| format!("HTTP request failed: {}", e))?;

        if !response.status().is_success() {
            return Err(format!("HTTP error: {}", response.status()));
        }

        let body = response
            .text()
            .await
            .map_err(|e| format!("Failed to read response body: {}", e))?;

        let keys = self.parse_blocklist(&body);

        // SAFETY CHECK: Reject empty responses to prevent accidental unblock-all
        if keys.is_empty() {
            return Err("Remote blocklist is empty. Ignoring update to preserve protection.".into());
        }

        let count = keys.len();

        // Persist to local file first (ensures next boot uses fresh data)
        if let Err(e) = self.persist_to_file(&keys).await {
            warn!("🛡️ Shield: Failed to persist blocklist: {}", e);
            // Continue anyway - memory update is more important
        }

        // Hot-swap the blocklist (write lock, brief)
        {
            let mut guard = self.blocklist.write().await;
            *guard = keys;
        }

        info!(
            "🛡️ Shield: Updated blocklist with {} validators from remote",
            count
        );
        Ok(count)
    }

    /// Reload blocklist from local file.
    pub async fn reload_local(&self) -> usize {
        self.load_local().await
    }

    /// Spawn background updater task.
    ///
    /// Behavior depends on configuration:
    /// - If remote URL configured: Fetches from remote periodically
    /// - If local-only: Watches local file for changes
    pub fn spawn_updater(self: Arc<Self>) -> tokio::task::JoinHandle<()> {
        let manager = self.clone();
        tokio::spawn(async move {
            // Initial remote fetch if configured
            if manager.remote_url.is_some() {
                if let Err(e) = manager.fetch_remote().await {
                    warn!("Shield: Initial remote fetch failed: {}", e);
                }
            }

            loop {
                tokio::time::sleep(manager.refresh_interval).await;

                if manager.remote_url.is_some() {
                    if let Err(e) = manager.fetch_remote().await {
                        debug!("Shield: Remote fetch failed, reloading local: {}", e);
                        manager.reload_local().await;
                    }
                } else {
                    // Local-only: periodically reload file
                    manager.reload_local().await;
                }
            }
        })
    }

    /// Check if a pubkey is blocked.
    ///
    /// This is the hot path - O(1) lookup with shared read lock.
    pub async fn is_blocked(&self, pubkey: &Pubkey) -> bool {
        let guard = self.blocklist.read().await;
        guard.contains(pubkey)
    }

    /// Get current blocklist size (for monitoring).
    pub async fn len(&self) -> usize {
        let guard = self.blocklist.read().await;
        guard.len()
    }

    /// Check if blocklist is empty.
    pub async fn is_empty(&self) -> bool {
        self.len().await == 0
    }

    /// Parse blocklist text into HashSet of Pubkeys.
    ///
    /// Format: One base58 pubkey per line. Empty lines and invalid keys are skipped.
    fn parse_blocklist(&self, content: &str) -> HashSet<Pubkey> {
        content
            .lines()
            .filter_map(|line| {
                let trimmed = line.trim();
                if trimmed.is_empty() || trimmed.starts_with('#') {
                    return None;
                }
                match Pubkey::from_str(trimmed) {
                    Ok(pk) => Some(pk),
                    Err(_) => {
                        debug!("Shield: Skipping invalid pubkey: {}", trimmed);
                        None
                    }
                }
            })
            .collect()
    }

    /// Load blocklist from a file.
    async fn load_from_file(&self, path: &Path) -> Result<HashSet<Pubkey>, std::io::Error> {
        let content = tokio::fs::read_to_string(path).await?;
        Ok(self.parse_blocklist(&content))
    }

    /// Persist blocklist to local file.
    async fn persist_to_file(&self, keys: &HashSet<Pubkey>) -> Result<(), std::io::Error> {
        let content: String = keys.iter().map(|pk| format!("{}\n", pk)).collect();
        tokio::fs::write(&self.local_path, content).await?;
        debug!(
            "🛡️ Shield: Persisted {} keys to {:?}",
            keys.len(),
            self.local_path
        );
        Ok(())
    }
}

impl Default for BlocklistManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_blocklist() {
        let manager = BlocklistManager::new();
        // Use valid base58 Solana pubkeys (44 chars each)
        let content = r#"
            # Comment line
            11111111111111111111111111111112
            TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA
            invalid_key_here
            
            So11111111111111111111111111111111111111112
        "#;

        let keys = manager.parse_blocklist(content);
        assert_eq!(keys.len(), 3);
    }

    #[test]
    fn test_parse_empty_blocklist() {
        let manager = BlocklistManager::new();
        let keys = manager.parse_blocklist("");
        assert!(keys.is_empty());
    }

    #[tokio::test]
    async fn test_is_blocked() {
        let manager = BlocklistManager::new();
        let pk = Pubkey::new_unique();

        // Initially not blocked
        assert!(!manager.is_blocked(&pk).await);

        // Add to blocklist
        {
            let mut guard = manager.blocklist.write().await;
            guard.insert(pk);
        }

        // Now blocked
        assert!(manager.is_blocked(&pk).await);
    }

    #[test]
    fn test_from_env_defaults() {
        // Clear env vars to test defaults
        std::env::remove_var("SCRAMJET_BLOCKLIST_FILE");
        std::env::remove_var("SCRAMJET_BLOCKLIST_URL");
        std::env::remove_var("SCRAMJET_BLOCKLIST_REFRESH_SECS");

        let manager = BlocklistManager::from_env();
        assert_eq!(manager.local_path, PathBuf::from("./blocklist.txt"));
        assert!(manager.remote_url.is_none()); // Local-only by default!
        assert_eq!(manager.refresh_interval, DEFAULT_REFRESH_INTERVAL);
    }

    #[test]
    fn test_local_only_mode() {
        let manager = BlocklistManager::new();
        assert!(manager.remote_url.is_none());
    }
}
