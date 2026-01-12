use log::{debug, info};
use scramjet_common::ScramjetError;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::pubkey::Pubkey;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::blocklist::BlocklistHandle;

/// Cartographer maintains cluster topology and leader schedule
pub struct Cartographer {
    rpc: Arc<RpcClient>,
    node_map: Arc<RwLock<HashMap<Pubkey, SocketAddr>>>, // Validator pubkey -> QUIC socket
    schedule: Arc<RwLock<HashMap<u64, Pubkey>>>,        // Slot -> Leader pubkey
    current_slot: Arc<AtomicU64>,                       // Atomic slot tracker (lock-free)
    current_epoch: Arc<AtomicU64>,
    blocklist: BlocklistHandle,                          // Shield: blocked validators
}

impl Cartographer {
    pub fn new(rpc_url: String, blocklist: BlocklistHandle) -> Self {
        let rpc = Arc::new(RpcClient::new(rpc_url));
        Self {
            rpc,
            node_map: Arc::new(RwLock::new(HashMap::new())),
            schedule: Arc::new(RwLock::new(HashMap::new())),
            current_slot: Arc::new(AtomicU64::new(0)),
            current_epoch: Arc::new(AtomicU64::new(0)),
            blocklist,
        }
    }

    /// Get current slot (lock-free atomic read)
    pub fn get_known_slot(&self) -> u64 {
        self.current_slot.load(Ordering::Relaxed)
    }

    /// Update slot tracker (atomic write)
    pub fn update_slot(&self, slot: u64) {
        let old = self.current_slot.swap(slot, Ordering::Relaxed);
        if slot > old {
            debug!("Slot advanced: {} -> {}", old, slot);
        }
    }

    /// Resolve leader IP for given slot (pubkey lookup + socket resolution)
    /// Returns None if leader is blocked by Shield
    pub async fn get_target(&self, slot: u64) -> Option<SocketAddr> {
        // Step 1: Lookup leader pubkey for this slot
        let leader_pubkey = {
            let schedule = self.schedule.read().await;
            schedule.get(&slot).cloned()?
        };
        
        // Step 2: Shield check - skip blocked validators
        {
            let blocklist = self.blocklist.read().await;
            if blocklist.contains(&leader_pubkey) {
                debug!("Shield: Blocked {} for slot {}", leader_pubkey, slot);
                return None;
            }
        }
        
        // Step 3: Resolve pubkey to QUIC socket address
        let node_map = self.node_map.read().await;
        node_map.get(&leader_pubkey).cloned()
    }

    /// Returns deduplicated upcoming leader sockets (for Scout pre-warming)
    /// Filters out blocked validators to save resources
    pub async fn get_upcoming_leaders(&self, current_slot: u64, lookahead: u64) -> Vec<SocketAddr> {
        let mut unique_targets = Vec::new();
        let schedule = self.schedule.read().await;
        let node_map = self.node_map.read().await;
        let blocklist = self.blocklist.read().await;

        // Collect unique addresses for upcoming slots (excluding blocked validators)
        for i in 1..=lookahead {
            let target_slot = current_slot + i;
            if let Some(pubkey) = schedule.get(&target_slot) {
                // Shield: Skip blocked validators
                if blocklist.contains(pubkey) {
                    debug!("Shield: Skipping blocked leader {} for scout", pubkey);
                    continue;
                }
                if let Some(addr) = node_map.get(pubkey) {
                    if !unique_targets.contains(addr) {
                        unique_targets.push(*addr);
                    }
                }
            }
        }
        unique_targets
    }

    /// Fetch cluster topology (validator pubkey -> QUIC socket mapping)
    pub async fn refresh_topology(&self) -> Result<(), ScramjetError> {
        info!("Refreshing cluster topology via RPC...");
        let nodes = self
            .rpc
            .get_cluster_nodes()
            .await
            .map_err(|e| ScramjetError::RpcError(format!("Failed to fetch nodes: {}", e)))?;
        let mut new_map = HashMap::new();

        for node in nodes {
            if let Some(tpu_quic) = node.tpu_quic {
                if let Ok(pubkey) = Pubkey::from_str(&node.pubkey) {
                    new_map.insert(pubkey, tpu_quic);
                }
            }
        }
        let mut map_guard = self.node_map.write().await;
        *map_guard = new_map;
        info!(
            "Topology updated. Known QUIC Validators: {}",
            map_guard.len()
        );
        Ok(())
    }

    /// Update leader schedule for current epoch (refresh on epoch change)
    pub async fn update_schedule(&self) -> Result<(), ScramjetError> {
        let epoch_info = self
            .rpc
            .get_epoch_info()
            .await
            .map_err(|e| ScramjetError::RpcError(format!("Failed to get epoch info: {}", e)))?;
        let current_epoch = epoch_info.epoch;
        let stored_epoch = self.current_epoch.load(Ordering::Relaxed);

        // Only refresh if epoch changed or first run
        if current_epoch > stored_epoch || stored_epoch == 0 {
            info!(
                "New Epoch detected ({}). Fetching Leader Schedule...",
                current_epoch
            );
            let schedule_data = self
                .rpc
                .get_leader_schedule(None)
                .await
                .map_err(|e| ScramjetError::RpcError(format!("Failed to get leader schedule: {}", e)))?
                .ok_or(ScramjetError::ScheduleUnavailable)?;

            let mut new_schedule = HashMap::new();
            let start_slot = epoch_info.absolute_slot - epoch_info.slot_index;

            // Convert relative slot offsets to absolute slot numbers
            for (pubkey_str, relative_slots) in schedule_data {
                if let Ok(pubkey) = Pubkey::from_str(&pubkey_str) {
                    for rel_slot in relative_slots {
                        let abs_slot = start_slot + rel_slot as u64;
                        new_schedule.insert(abs_slot, pubkey);
                    }
                }
            }

            let mut schedule_guard = self.schedule.write().await;
            *schedule_guard = new_schedule;
            self.current_epoch.store(current_epoch, Ordering::Relaxed);
            self.update_slot(epoch_info.absolute_slot);
        }
        Ok(())
    }

    /// Fetch current slot from RPC and update tracker (legacy polling mode)
    pub async fn fetch_rpc_slot(&self) -> Result<u64, ScramjetError> {
        let slot = self
            .rpc
            .get_slot()
            .await
            .map_err(|e| ScramjetError::RpcError(format!("Failed to get slot: {}", e)))?;
        self.update_slot(slot);
        Ok(slot)
    }

    pub fn rpc_client(&self) -> Arc<RpcClient> {
        self.rpc.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn create_empty_blocklist() -> BlocklistHandle {
        Arc::new(RwLock::new(HashSet::new()))
    }

    fn create_empty_cartographer() -> Cartographer {
        Cartographer::new("http://mock-rpc".to_string(), create_empty_blocklist())
    }

    #[test]
    fn test_atomic_clock_basics() {
        let c = create_empty_cartographer();
        assert_eq!(c.get_known_slot(), 0);
        c.update_slot(100);
        assert_eq!(c.get_known_slot(), 100);
        c.update_slot(101);
        assert_eq!(c.get_known_slot(), 101);
    }

    #[tokio::test]
    async fn test_topology_resolution() {
        let c = create_empty_cartographer();
        let pk = Pubkey::new_unique();
        let addr: SocketAddr = "127.0.0.1:8000".parse().unwrap();

        // Simulate Schedule and Topology update
        {
            let mut sched = c.schedule.write().await;
            sched.insert(500, pk);
        }
        {
            let mut nodes = c.node_map.write().await;
            nodes.insert(pk, addr);
        }

        // Test Hit
        let result = c.get_target(500).await;
        assert_eq!(result, Some(addr));

        // Test Miss
        let miss = c.get_target(501).await;
        assert_eq!(miss, None);
    }

    #[tokio::test]
    async fn test_shield_blocks_malicious_validator() {
        let blocklist = create_empty_blocklist();
        let malicious_pk = Pubkey::new_unique();
        let good_pk = Pubkey::new_unique();
        let addr1: SocketAddr = "1.1.1.1:80".parse().unwrap();
        let addr2: SocketAddr = "2.2.2.2:80".parse().unwrap();

        // Add malicious validator to blocklist
        {
            let mut guard = blocklist.write().await;
            guard.insert(malicious_pk);
        }

        let c = Cartographer::new("http://mock-rpc".to_string(), blocklist);

        // Setup schedule and topology
        {
            let mut sched = c.schedule.write().await;
            sched.insert(100, malicious_pk);
            sched.insert(101, good_pk);
        }
        {
            let mut nodes = c.node_map.write().await;
            nodes.insert(malicious_pk, addr1);
            nodes.insert(good_pk, addr2);
        }

        // Blocked validator should return None
        assert_eq!(c.get_target(100).await, None);
        // Good validator should return address
        assert_eq!(c.get_target(101).await, Some(addr2));
    }

    #[tokio::test]
    async fn test_scout_lookahead() {
        let c = create_empty_cartographer();
        let pk1 = Pubkey::new_unique();
        let pk2 = Pubkey::new_unique();
        let addr1: SocketAddr = "1.1.1.1:80".parse().unwrap();
        let addr2: SocketAddr = "2.2.2.2:80".parse().unwrap();

        // Schedule: Slot 101->A, 102->A, 103->B
        {
            let mut sched = c.schedule.write().await;
            sched.insert(101, pk1);
            sched.insert(102, pk1);
            sched.insert(103, pk2);
        }
        {
            let mut nodes = c.node_map.write().await;
            nodes.insert(pk1, addr1);
            nodes.insert(pk2, addr2);
        }

        // Scout looking ahead 5 slots from 100
        let targets = c.get_upcoming_leaders(100, 5).await;

        // Should contain both addresses, no duplicates
        assert_eq!(targets.len(), 2);
        assert!(targets.contains(&addr1));
        assert!(targets.contains(&addr2));
    }

    #[tokio::test]
    async fn test_scout_filters_blocked_validators() {
        let blocklist = create_empty_blocklist();
        let blocked_pk = Pubkey::new_unique();
        let good_pk = Pubkey::new_unique();
        let blocked_addr: SocketAddr = "1.1.1.1:80".parse().unwrap();
        let good_addr: SocketAddr = "2.2.2.2:80".parse().unwrap();

        // Block one validator
        {
            let mut guard = blocklist.write().await;
            guard.insert(blocked_pk);
        }

        let c = Cartographer::new("http://mock-rpc".to_string(), blocklist);

        // Schedule: Slot 101->blocked, 102->good
        {
            let mut sched = c.schedule.write().await;
            sched.insert(101, blocked_pk);
            sched.insert(102, good_pk);
        }
        {
            let mut nodes = c.node_map.write().await;
            nodes.insert(blocked_pk, blocked_addr);
            nodes.insert(good_pk, good_addr);
        }

        // Scout should only return the good validator
        let targets = c.get_upcoming_leaders(100, 5).await;
        assert_eq!(targets.len(), 1);
        assert!(targets.contains(&good_addr));
        assert!(!targets.contains(&blocked_addr));
    }
}
