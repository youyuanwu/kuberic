use std::collections::HashMap;

use async_trait::async_trait;
use tracing::{info, warn};

use crate::error::{KubelicateError, Result};
use crate::types::{
    DataLossAction, Epoch, Lsn, OpenMode, ReplicaId, ReplicaInfo, ReplicaSetConfig,
    ReplicaSetQuorumMode, ReplicaStatus, Role,
};

// ---------------------------------------------------------------------------
// ReplicaHandle trait — abstraction over how we talk to a replica
// ---------------------------------------------------------------------------

/// Handle for communicating with a single replica's replicator.
/// Tests implement this via in-process channels; the operator implements it
/// via gRPC to a remote pod.
#[async_trait]
pub trait ReplicaHandle: Send + Sync {
    fn id(&self) -> ReplicaId;

    // Lifecycle
    async fn open(&self, mode: OpenMode) -> Result<()>;
    async fn close(&self) -> Result<()>;
    fn abort(&self);

    // Role management
    async fn change_role(&self, epoch: Epoch, role: Role) -> Result<()>;
    async fn update_epoch(&self, epoch: Epoch) -> Result<()>;

    // Progress (for primary selection)
    fn current_progress(&self) -> Lsn;
    fn catch_up_capability(&self) -> Lsn;

    // Primary-only reconfiguration
    async fn on_data_loss(&self) -> Result<DataLossAction>;
    async fn update_catch_up_configuration(
        &self,
        current: ReplicaSetConfig,
        previous: ReplicaSetConfig,
    ) -> Result<()>;
    async fn update_current_configuration(&self, current: ReplicaSetConfig) -> Result<()>;
    async fn wait_for_catch_up_quorum(&self, mode: ReplicaSetQuorumMode) -> Result<()>;
    async fn build_replica(&self, replica: ReplicaInfo) -> Result<()>;
    async fn remove_replica(&self, replica_id: ReplicaId) -> Result<()>;

    /// Revoke write status before switchover demotion.
    /// Sets write_status = ReconfigurationPending so new writes are
    /// immediately rejected. In-flight writes continue to completion.
    async fn revoke_write_status(&self) -> Result<()>;

    /// The gRPC address where this replica's replication server listens.
    fn replicator_address(&self) -> String;
}

// ---------------------------------------------------------------------------
// PartitionDriver — pure workflow orchestrator
// ---------------------------------------------------------------------------

/// Workflow driver that encodes the correct SF-style lifecycle sequences
/// for a partition. Operates on `ReplicaHandle` trait objects — agnostic
/// to whether replicas are in-process or remote.
///
/// Mirrors `StatefulServicePartitionDriver` from service-fabric-rs.
pub struct PartitionDriver {
    replicas: HashMap<ReplicaId, ReplicaState>,
    primary_id: Option<ReplicaId>,
    epoch: Epoch,
    current_config: ReplicaSetConfig,
}

struct ReplicaState {
    handle: Box<dyn ReplicaHandle>,
    role: Role,
}

impl Default for PartitionDriver {
    fn default() -> Self {
        Self::new()
    }
}

impl PartitionDriver {
    pub fn new() -> Self {
        Self {
            replicas: HashMap::new(),
            primary_id: None,
            epoch: Epoch::new(0, 0),
            current_config: ReplicaSetConfig {
                members: vec![],
                write_quorum: 0,
            },
        }
    }

    fn next_epoch(&mut self) -> Epoch {
        self.epoch.configuration_number += 1;
        self.epoch
    }

    pub fn primary_id(&self) -> Option<ReplicaId> {
        self.primary_id
    }

    pub fn epoch(&self) -> Epoch {
        self.epoch
    }

    pub fn replica_ids(&self) -> Vec<ReplicaId> {
        self.replicas.keys().cloned().collect()
    }

    pub fn handle(&self, id: ReplicaId) -> Option<&dyn ReplicaHandle> {
        self.replicas.get(&id).map(|s| s.handle.as_ref())
    }

    /// Remove a replica from the driver's tracking without notifying
    /// the primary's replicator. Used when the reconciler detects a pod
    /// is permanently dead before failover. Returns the handle for cleanup.
    pub fn remove_replica_from_driver(&mut self, id: ReplicaId) -> Option<Box<dyn ReplicaHandle>> {
        self.replicas.remove(&id).map(|s| s.handle)
    }

    // -----------------------------------------------------------------------
    // Workflow: Create Partition
    // -----------------------------------------------------------------------

    /// Create a partition from pre-created replica handles.
    /// The first handle becomes primary; the rest become secondaries.
    ///
    /// Follows the exact SF workflow:
    /// 1. Open all replicators
    /// 2. Assign primary role (replicator first)
    /// 3. Assign idle role to secondaries
    /// 4. build_replica for each secondary
    /// 5. Promote each secondary to active
    /// 6. Update configuration incrementally
    /// 7. Set access status
    pub async fn create_partition(&mut self, handles: Vec<Box<dyn ReplicaHandle>>) -> Result<()> {
        assert!(!handles.is_empty());
        assert!(self.replicas.is_empty());

        let epoch = self.next_epoch();

        let ids: Vec<ReplicaId> = handles.iter().map(|h| h.id()).collect();
        let primary_id = ids[0];
        let secondary_ids: Vec<ReplicaId> = ids[1..].to_vec();

        // Store handles
        for handle in handles {
            let id = handle.id();
            self.replicas.insert(
                id,
                ReplicaState {
                    handle,
                    role: Role::None,
                },
            );
        }

        // 1. Open all replicators
        for &id in &ids {
            self.replicas[&id].handle.open(OpenMode::New).await?;
        }

        // 2. Assign roles to replicators (replicator BEFORE status set)
        self.replicas[&primary_id]
            .handle
            .change_role(epoch, Role::Primary)
            .await?;
        self.replicas.get_mut(&primary_id).unwrap().role = Role::Primary;
        self.primary_id = Some(primary_id);

        // 3. Secondaries → Idle
        for &id in &secondary_ids {
            let entry = &self.replicas[&id];
            entry.handle.update_epoch(epoch).await?;
            entry.handle.change_role(epoch, Role::IdleSecondary).await?;
            self.replicas.get_mut(&id).unwrap().role = Role::IdleSecondary;
        }

        // 4. Build each secondary via primary, then promote
        for &id in &secondary_ids {
            let addr = self.replicas[&id].handle.replicator_address();
            let replica_info = ReplicaInfo {
                id,
                role: Role::IdleSecondary,
                status: ReplicaStatus::Up,
                replicator_address: addr,
                current_progress: -1,
                catch_up_capability: -1,
                must_catch_up: false,
            };
            // Primary handles the full copy protocol internally
            // (connects to secondary's data plane, runs GetCopyContext + CopyStream)
            self.replicas[&primary_id]
                .handle
                .build_replica(replica_info)
                .await?;

            // 5. Promote idle → active
            self.replicas[&id]
                .handle
                .change_role(epoch, Role::ActiveSecondary)
                .await?;
            self.replicas.get_mut(&id).unwrap().role = Role::ActiveSecondary;
        }

        // 6. Update configuration incrementally
        let mut config = ReplicaSetConfig {
            members: vec![],
            write_quorum: 1,
        };
        let mut ready_count: u32 = 1; // Primary

        for &id in &secondary_ids {
            let prev_config = config.clone();
            let addr = self.replicas[&id].handle.replicator_address();

            config.members.push(ReplicaInfo {
                id,
                role: Role::ActiveSecondary,
                status: ReplicaStatus::Up,
                replicator_address: addr,
                current_progress: 0,
                catch_up_capability: 0,
                must_catch_up: false,
            });
            ready_count += 1;
            config.write_quorum = ready_count / 2 + 1;

            self.replicas[&primary_id]
                .handle
                .update_catch_up_configuration(config.clone(), prev_config)
                .await?;

            // Give gRPC connections time to establish (in-process only)
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;

            self.replicas[&primary_id]
                .handle
                .wait_for_catch_up_quorum(ReplicaSetQuorumMode::Write)
                .await?;

            self.replicas[&primary_id]
                .handle
                .update_current_configuration(config.clone())
                .await?;
        }

        self.current_config = config;

        // Access status is set by each pod's PodRuntime during change_role()

        info!(
            primary = primary_id,
            secondaries = ?secondary_ids,
            epoch = ?self.epoch,
            write_quorum = self.current_config.write_quorum,
            "partition created"
        );

        Ok(())
    }

    // -----------------------------------------------------------------------
    // Workflow: Delete Partition
    // -----------------------------------------------------------------------

    /// Gracefully shut down all replicas.
    pub async fn delete_partition(&mut self) -> Result<()> {
        // 1. Demote primary
        if let Some(pid) = self.primary_id {
            self.replicas[&pid]
                .handle
                .change_role(self.epoch, Role::ActiveSecondary)
                .await?;
        }

        // 2. Change all to None
        for entry in self.replicas.values() {
            entry.handle.change_role(self.epoch, Role::None).await?;
        }

        // 3. Close all
        for entry in self.replicas.values() {
            entry.handle.close().await?;
        }

        self.replicas.clear();
        self.primary_id = None;
        self.current_config = ReplicaSetConfig {
            members: vec![],
            write_quorum: 0,
        };

        info!("partition deleted");
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Workflow: Failover (unplanned primary failure)
    // -----------------------------------------------------------------------

    /// Failover after the primary has failed. The failed primary's handle
    /// may be unreachable — the driver does not call it.
    ///
    /// Matches SF's reconfiguration phases:
    /// 1. Remove failed primary, increment epoch
    /// 2. Select new primary by highest current_progress (Phase 1: GetLSN)
    /// 3. Promote new primary with new epoch (Phase 4: Activate)
    /// 4. Reconfigure quorum — epoch distributed to secondaries as part
    ///    of the new configuration (best-effort, skip unreachable)
    pub async fn failover(&mut self, failed_primary_id: ReplicaId) -> Result<()> {
        assert_eq!(
            Some(failed_primary_id),
            self.primary_id,
            "can only failover the current primary"
        );

        let new_epoch = self.next_epoch();
        info!(failed = failed_primary_id, ?new_epoch, "starting failover");

        // Remove the failed primary from our tracking
        self.replicas.remove(&failed_primary_id);
        self.primary_id = None;

        if self.replicas.is_empty() {
            return Err(KubelicateError::Internal(
                "no surviving replicas for failover".into(),
            ));
        }

        // 1. Select new primary by highest current_progress (LSN)
        let new_primary_id = self
            .replicas
            .values()
            .max_by_key(|e| e.handle.current_progress())
            .map(|e| e.handle.id())
            .unwrap();

        info!(
            new_primary = new_primary_id,
            lsn = self.replicas[&new_primary_id].handle.current_progress(),
            "selected new primary"
        );

        // 2. Promote new primary (SF Phase 4: Activate)
        // The new epoch is delivered with the promotion — no separate
        // fencing step needed. The old primary is dead and can't send ops.
        self.replicas[&new_primary_id]
            .handle
            .change_role(new_epoch, Role::Primary)
            .await?;
        self.replicas.get_mut(&new_primary_id).unwrap().role = Role::Primary;
        self.primary_id = Some(new_primary_id);

        // 3. Distribute epoch to surviving secondaries (best-effort).
        // Unreachable secondaries are skipped — they'll be rebuilt later.
        // This prevents a zombie primary (if it recovers) from sending
        // ops to secondaries that still accept the old epoch.
        for (&id, entry) in &self.replicas {
            if id != new_primary_id && entry.handle.update_epoch(new_epoch).await.is_err() {
                warn!(
                    replica_id = id,
                    "failed to update epoch on secondary (will be rebuilt)"
                );
            }
        }

        // 4. Rebuild configuration (all surviving non-primary replicas)
        let secondary_ids: Vec<ReplicaId> = self
            .replicas
            .keys()
            .filter(|&&id| id != new_primary_id)
            .cloned()
            .collect();

        let total_count = self.replicas.len() as u32;
        let write_quorum = total_count / 2 + 1;

        let members: Vec<ReplicaInfo> = secondary_ids
            .iter()
            .map(|&id| {
                let entry = &self.replicas[&id];
                ReplicaInfo {
                    id,
                    role: Role::ActiveSecondary,
                    status: ReplicaStatus::Up,
                    replicator_address: entry.handle.replicator_address(),
                    current_progress: entry.handle.current_progress(),
                    catch_up_capability: entry.handle.catch_up_capability(),
                    must_catch_up: false,
                }
            })
            .collect();

        let new_config = ReplicaSetConfig {
            members,
            write_quorum,
        };

        // Update configuration on new primary
        self.replicas[&new_primary_id]
            .handle
            .update_catch_up_configuration(new_config.clone(), self.current_config.clone())
            .await?;

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        self.replicas[&new_primary_id]
            .handle
            .wait_for_catch_up_quorum(ReplicaSetQuorumMode::Write)
            .await?;

        self.replicas[&new_primary_id]
            .handle
            .update_current_configuration(new_config.clone())
            .await?;

        self.current_config = new_config;

        info!(
            new_primary = new_primary_id,
            epoch = ?self.epoch,
            "failover complete"
        );

        Ok(())
    }

    // -----------------------------------------------------------------------
    // Workflow: Switchover (planned primary change)
    // -----------------------------------------------------------------------

    /// Graceful primary change to a specific target secondary.
    ///
    /// Matches SF's SwapPrimary reconfiguration:
    /// 1. Revoke write status on old primary (SF Phase 0: Demote)
    /// 2. Demote old primary → ActiveSecondary
    /// 3. Promote target → Primary (SF Phase 4: Activate)
    /// 4. Distribute epoch to other secondaries (best-effort)
    /// 5. Reconfigure quorum + catchup
    pub async fn switchover(&mut self, target_id: ReplicaId) -> Result<()> {
        let old_primary_id = self.primary_id.ok_or(KubelicateError::NotPrimary)?;

        assert_ne!(
            old_primary_id, target_id,
            "target must differ from current primary"
        );
        assert!(
            self.replicas.contains_key(&target_id),
            "target must be a known replica"
        );

        let new_epoch = self.next_epoch();
        info!(
            old_primary = old_primary_id,
            new_primary = target_id,
            ?new_epoch,
            "starting switchover"
        );

        // 1. Revoke write status on old primary (SF Phase 0: Demote)
        // New writes are immediately rejected; in-flight writes continue.
        self.replicas[&old_primary_id]
            .handle
            .revoke_write_status()
            .await?;

        // 2. Demote old primary → ActiveSecondary
        self.replicas[&old_primary_id]
            .handle
            .change_role(new_epoch, Role::ActiveSecondary)
            .await?;
        self.replicas.get_mut(&old_primary_id).unwrap().role = Role::ActiveSecondary;

        // 3. Promote target → Primary (SF Phase 4: Activate)
        // If this fails or times out, rollback: re-promote old primary
        // (SF AbortPhase0Demote + RevertConfiguration pattern).
        let promote_result = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            self.replicas[&target_id]
                .handle
                .change_role(new_epoch, Role::Primary),
        )
        .await;

        let promote_err = match promote_result {
            Ok(Ok(())) => None,
            Ok(Err(e)) => Some(e),
            Err(_) => Some(KubelicateError::Internal("promotion timed out".into())),
        };

        if let Some(e) = promote_err {
            warn!(
                target_id,
                error = %e,
                "target promotion failed, rolling back — re-promoting old primary"
            );
            self.replicas[&old_primary_id]
                .handle
                .change_role(new_epoch, Role::Primary)
                .await?;
            self.replicas.get_mut(&old_primary_id).unwrap().role = Role::Primary;
            self.primary_id = Some(old_primary_id);
            return Err(e);
        }
        self.replicas.get_mut(&target_id).unwrap().role = Role::Primary;
        self.primary_id = Some(target_id);

        // 4. Distribute epoch to other secondaries (best-effort).
        // Unreachable secondaries are skipped — they'll be rebuilt later.
        // The old primary already has the epoch from step 2 (change_role).
        // The target already has it from step 3.
        for (&id, entry) in &self.replicas {
            if id != old_primary_id
                && id != target_id
                && entry.handle.update_epoch(new_epoch).await.is_err()
            {
                warn!(
                    replica_id = id,
                    "failed to update epoch on secondary (will be rebuilt)"
                );
            }
        }

        // 5. Rebuild configuration
        let secondary_ids: Vec<ReplicaId> = self
            .replicas
            .keys()
            .filter(|&&id| id != target_id)
            .cloned()
            .collect();

        let total_count = self.replicas.len() as u32;
        let write_quorum = total_count / 2 + 1;

        let members: Vec<ReplicaInfo> = secondary_ids
            .iter()
            .map(|&id| {
                let entry = &self.replicas[&id];
                ReplicaInfo {
                    id,
                    role: Role::ActiveSecondary,
                    status: ReplicaStatus::Up,
                    replicator_address: entry.handle.replicator_address(),
                    current_progress: entry.handle.current_progress(),
                    catch_up_capability: entry.handle.catch_up_capability(),
                    must_catch_up: false,
                }
            })
            .collect();

        let new_config = ReplicaSetConfig {
            members,
            write_quorum,
        };

        self.replicas[&target_id]
            .handle
            .update_catch_up_configuration(new_config.clone(), self.current_config.clone())
            .await?;

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        self.replicas[&target_id]
            .handle
            .wait_for_catch_up_quorum(ReplicaSetQuorumMode::Write)
            .await?;

        self.replicas[&target_id]
            .handle
            .update_current_configuration(new_config.clone())
            .await?;

        self.current_config = new_config;

        info!(
            new_primary = target_id,
            epoch = ?self.epoch,
            "switchover complete"
        );

        Ok(())
    }

    // -----------------------------------------------------------------------
    // Workflow: Remove Secondary (scale-down)
    // -----------------------------------------------------------------------

    /// Remove a secondary from the partition. Config-first: the configuration
    /// is updated before the replica is closed, maintaining write quorum.
    ///
    /// 1. Verify not removing primary, and above min count
    /// 2. Reconfigure without the target replica
    /// 3. Change role to None + close the removed replica
    /// 4. Remove from driver
    pub async fn remove_secondary(
        &mut self,
        secondary_id: ReplicaId,
        min_replicas: usize,
    ) -> Result<()> {
        let primary_id = self.primary_id.ok_or(KubelicateError::NotPrimary)?;
        assert_ne!(
            secondary_id, primary_id,
            "cannot remove the primary — use switchover first"
        );
        assert!(
            self.replicas.contains_key(&secondary_id),
            "replica {} not found",
            secondary_id
        );
        assert!(
            self.replicas.len() > min_replicas,
            "cannot scale below min_replicas ({})",
            min_replicas
        );

        info!(secondary_id, "removing secondary (scale-down)");

        // 1. Reconfigure without the target replica (config-first)
        let secondary_ids: Vec<ReplicaId> = self
            .replicas
            .keys()
            .filter(|&&id| id != primary_id && id != secondary_id)
            .cloned()
            .collect();

        let total_count = (self.replicas.len() - 1) as u32; // after removal
        let write_quorum = total_count / 2 + 1;

        let members: Vec<ReplicaInfo> = secondary_ids
            .iter()
            .map(|&id| {
                let entry = &self.replicas[&id];
                ReplicaInfo {
                    id,
                    role: Role::ActiveSecondary,
                    status: ReplicaStatus::Up,
                    replicator_address: entry.handle.replicator_address(),
                    current_progress: entry.handle.current_progress(),
                    catch_up_capability: entry.handle.catch_up_capability(),
                    must_catch_up: false,
                }
            })
            .collect();

        let new_config = ReplicaSetConfig {
            members,
            write_quorum,
        };

        self.replicas[&primary_id]
            .handle
            .update_catch_up_configuration(new_config.clone(), self.current_config.clone())
            .await?;

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        self.replicas[&primary_id]
            .handle
            .wait_for_catch_up_quorum(ReplicaSetQuorumMode::Write)
            .await?;

        self.replicas[&primary_id]
            .handle
            .update_current_configuration(new_config.clone())
            .await?;

        self.current_config = new_config;

        // 2. Close the removed replica
        let removed = self.replicas.remove(&secondary_id).unwrap();
        let _ = removed.handle.change_role(self.epoch, Role::None).await;
        let _ = removed.handle.close().await;

        info!(
            secondary_id,
            remaining = self.replicas.len(),
            "secondary removed"
        );
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Workflow: Add Replica (scale-up or rebuild)
    // -----------------------------------------------------------------------

    /// Add a new replica to the partition. The primary builds it via the
    /// copy protocol, then it joins the quorum configuration.
    ///
    /// Used for:
    /// - **Scale-up:** operator creates a new pod, calls add_replica
    /// - **Restart:** restart_secondary calls this after closing the old handle
    ///
    /// Flow:
    /// 1. Open + set epoch + assign idle role
    /// 2. build_replica on primary (copies state via data plane)
    /// 3. Promote idle → active
    /// 4. Reconfigure quorum (must_catch_up on the new replica)
    pub async fn add_replica(&mut self, handle: Box<dyn ReplicaHandle>) -> Result<()> {
        let primary_id = self.primary_id.ok_or(KubelicateError::NotPrimary)?;
        let replica_id = handle.id();

        assert_ne!(
            replica_id, primary_id,
            "cannot add the primary as a secondary"
        );
        assert!(
            !self.replicas.contains_key(&replica_id),
            "replica {} already exists — use restart_secondary to replace",
            replica_id
        );

        let epoch = self.epoch;
        info!(replica_id, ?epoch, "adding replica");

        // Store handle
        self.replicas.insert(
            replica_id,
            ReplicaState {
                handle,
                role: Role::None,
            },
        );

        // 1. Open + set epoch + assign idle role
        let h = &self.replicas[&replica_id].handle;
        h.open(OpenMode::New).await?;
        h.update_epoch(epoch).await?;
        h.change_role(epoch, Role::IdleSecondary).await?;
        self.replicas.get_mut(&replica_id).unwrap().role = Role::IdleSecondary;

        // 2. build_replica on primary (copies state via data plane)
        let addr = self.replicas[&replica_id].handle.replicator_address();
        let replica_info = ReplicaInfo {
            id: replica_id,
            role: Role::IdleSecondary,
            status: ReplicaStatus::Up,
            replicator_address: addr,
            current_progress: -1,
            catch_up_capability: -1,
            must_catch_up: false,
        };
        self.replicas[&primary_id]
            .handle
            .build_replica(replica_info)
            .await?;

        // 3. Promote idle → active
        self.replicas[&replica_id]
            .handle
            .change_role(epoch, Role::ActiveSecondary)
            .await?;
        self.replicas.get_mut(&replica_id).unwrap().role = Role::ActiveSecondary;

        // 4. Reconfigure quorum (rebuild full config, must_catch_up on new replica)
        self.reconfigure_quorum(primary_id, Some(replica_id))
            .await?;

        info!(replica_id, "replica added");
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Workflow: Restart Secondary
    // -----------------------------------------------------------------------

    /// Restart a secondary replica. The old handle is replaced with a new one
    /// (simulating pod restart with fresh state). The primary rebuilds it via
    /// the copy protocol.
    pub async fn restart_secondary(
        &mut self,
        secondary_id: ReplicaId,
        new_handle: Box<dyn ReplicaHandle>,
    ) -> Result<()> {
        let primary_id = self.primary_id.ok_or(KubelicateError::NotPrimary)?;
        assert_ne!(
            secondary_id, primary_id,
            "cannot restart the primary with restart_secondary"
        );
        assert!(
            self.replicas.contains_key(&secondary_id),
            "replica {} not found — use add_replica for new replicas",
            secondary_id
        );

        info!(secondary_id, "restarting secondary");

        // 1. Close old secondary (best effort — may be dead)
        if let Some(old) = self.replicas.get(&secondary_id) {
            let _ = old.handle.close().await;
        }

        // 2. Remove old handle, then add_replica with new one
        self.replicas.remove(&secondary_id);

        // Ensure new_handle has the same ID
        assert_eq!(new_handle.id(), secondary_id);
        self.add_replica(new_handle).await
    }

    // -----------------------------------------------------------------------
    // Internal: Reconfigure quorum after adding/rebuilding a replica
    // -----------------------------------------------------------------------

    async fn reconfigure_quorum(
        &mut self,
        primary_id: ReplicaId,
        must_catch_up_id: Option<ReplicaId>,
    ) -> Result<()> {
        let secondary_ids: Vec<ReplicaId> = self
            .replicas
            .keys()
            .filter(|&&id| id != primary_id)
            .cloned()
            .collect();

        let total_count = self.replicas.len() as u32;
        let write_quorum = total_count / 2 + 1;

        let members: Vec<ReplicaInfo> = secondary_ids
            .iter()
            .map(|&id| {
                let entry = &self.replicas[&id];
                ReplicaInfo {
                    id,
                    role: Role::ActiveSecondary,
                    status: ReplicaStatus::Up,
                    replicator_address: entry.handle.replicator_address(),
                    current_progress: entry.handle.current_progress(),
                    catch_up_capability: entry.handle.catch_up_capability(),
                    must_catch_up: must_catch_up_id == Some(id),
                }
            })
            .collect();

        let new_config = ReplicaSetConfig {
            members,
            write_quorum,
        };

        self.replicas[&primary_id]
            .handle
            .update_catch_up_configuration(new_config.clone(), self.current_config.clone())
            .await?;

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        self.replicas[&primary_id]
            .handle
            .wait_for_catch_up_quorum(ReplicaSetQuorumMode::Write)
            .await?;

        self.replicas[&primary_id]
            .handle
            .update_current_configuration(new_config.clone())
            .await?;

        self.current_config = new_config;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// In-process ReplicaHandle implementation (for tests)
// ---------------------------------------------------------------------------

#[cfg(any(test, feature = "testing"))]
pub mod testing {
    use super::*;
    use std::sync::Arc;

    use tokio::sync::{mpsc, oneshot};
    use tonic::transport::Server;

    use crate::events::{ReplicateRequest, ReplicatorControlEvent};
    use crate::handles::{PartitionState, StateReplicatorHandle};
    use crate::proto::replicator_data_server::ReplicatorDataServer;
    use crate::replicator::actor::WalReplicatorActor;
    use crate::replicator::secondary::{SecondaryReceiver, SecondaryState};
    use crate::types::{AccessStatus, CancellationToken};

    /// In-process replica handle: wraps channels to a local replicator actor
    /// and a local gRPC secondary server.
    pub struct InProcessReplicaHandle {
        id: ReplicaId,
        control_tx: mpsc::Sender<ReplicatorControlEvent>,
        data_tx: mpsc::Sender<ReplicateRequest>,
        state: Arc<PartitionState>,
        pub secondary_state: Arc<SecondaryState>,
        grpc_address: String,
        shutdown_token: CancellationToken,
        _actor_handle: tokio::task::JoinHandle<()>,
        _grpc_handle: tokio::task::JoinHandle<()>,
    }

    impl InProcessReplicaHandle {
        /// Spawn a new in-process replica (actor + gRPC server).
        pub async fn spawn(id: ReplicaId) -> Result<Self> {
            let (control_tx, control_rx) = mpsc::channel(16);
            let (data_tx, data_rx) = mpsc::channel::<ReplicateRequest>(256);
            let state = Arc::new(PartitionState::new());
            let secondary_state = Arc::new(SecondaryState::new());
            let shutdown_token = CancellationToken::new();

            // Start gRPC server with graceful shutdown
            let receiver = SecondaryReceiver::new(secondary_state.clone());
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .map_err(|e| KubelicateError::Internal(Box::new(e)))?;
            let addr = listener.local_addr().unwrap();
            let grpc_address = format!("http://{}", addr);

            let grpc_shutdown = shutdown_token.child_token();
            let grpc_handle = tokio::spawn(async move {
                let _ = Server::builder()
                    .add_service(ReplicatorDataServer::new(receiver))
                    .serve_with_incoming_shutdown(
                        tokio_stream::wrappers::TcpListenerStream::new(listener),
                        grpc_shutdown.cancelled(),
                    )
                    .await;
            });

            // Start replicator actor
            let actor = WalReplicatorActor::new(id);
            let state_cp = state.clone();
            let actor_handle = tokio::spawn(async move {
                actor.run(control_rx, data_rx, state_cp).await;
            });

            Ok(Self {
                id,
                control_tx,
                data_tx,
                state,
                secondary_state,
                grpc_address,
                shutdown_token,
                _actor_handle: actor_handle,
                _grpc_handle: grpc_handle,
            })
        }

        async fn send_control(
            &self,
            make: impl FnOnce(oneshot::Sender<Result<()>>) -> ReplicatorControlEvent,
        ) -> Result<()> {
            let (tx, rx) = oneshot::channel();
            self.control_tx
                .send(make(tx))
                .await
                .map_err(|_| KubelicateError::Closed)?;
            rx.await.map_err(|_| KubelicateError::Closed)?
        }

        /// Get a user-facing StateReplicatorHandle for writing data (test helper).
        pub fn state_replicator(&self) -> StateReplicatorHandle {
            StateReplicatorHandle::new(self.data_tx.clone(), self.state.clone())
        }
    }

    #[async_trait]
    impl ReplicaHandle for InProcessReplicaHandle {
        fn id(&self) -> ReplicaId {
            self.id
        }

        async fn open(&self, mode: OpenMode) -> Result<()> {
            self.send_control(|reply| ReplicatorControlEvent::Open { mode, reply })
                .await
        }

        async fn close(&self) -> Result<()> {
            let result = self
                .send_control(|reply| ReplicatorControlEvent::Close { reply })
                .await;
            self.shutdown_token.cancel();
            result
        }

        fn abort(&self) {
            let _ = self.control_tx.try_send(ReplicatorControlEvent::Abort);
            self.shutdown_token.cancel();
        }

        async fn change_role(&self, epoch: Epoch, role: Role) -> Result<()> {
            self.secondary_state.update_epoch(epoch);
            self.send_control(|reply| ReplicatorControlEvent::ChangeRole { epoch, role, reply })
                .await?;
            // Mirror PodRuntime: set access status based on role
            match role {
                Role::Primary => {
                    self.state.set_read_status(AccessStatus::Granted);
                    self.state.set_write_status(AccessStatus::Granted);
                }
                _ => {
                    self.state.set_read_status(AccessStatus::NotPrimary);
                    self.state.set_write_status(AccessStatus::NotPrimary);
                }
            }
            Ok(())
        }

        async fn update_epoch(&self, epoch: Epoch) -> Result<()> {
            self.secondary_state.update_epoch(epoch);
            self.send_control(|reply| ReplicatorControlEvent::UpdateEpoch { epoch, reply })
                .await
        }

        fn current_progress(&self) -> Lsn {
            self.state.current_progress()
        }

        fn catch_up_capability(&self) -> Lsn {
            self.state.catch_up_capability()
        }

        async fn on_data_loss(&self) -> Result<DataLossAction> {
            let (tx, rx) = oneshot::channel();
            self.control_tx
                .send(ReplicatorControlEvent::OnDataLoss { reply: tx })
                .await
                .map_err(|_| KubelicateError::Closed)?;
            rx.await.map_err(|_| KubelicateError::Closed)?
        }

        async fn update_catch_up_configuration(
            &self,
            current: ReplicaSetConfig,
            previous: ReplicaSetConfig,
        ) -> Result<()> {
            let (tx, rx) = oneshot::channel();
            self.control_tx
                .send(ReplicatorControlEvent::UpdateCatchUpConfiguration {
                    current,
                    previous,
                    reply: tx,
                })
                .await
                .map_err(|_| KubelicateError::Closed)?;
            rx.await.map_err(|_| KubelicateError::Closed)?
        }

        async fn update_current_configuration(&self, current: ReplicaSetConfig) -> Result<()> {
            let (tx, rx) = oneshot::channel();
            self.control_tx
                .send(ReplicatorControlEvent::UpdateCurrentConfiguration { current, reply: tx })
                .await
                .map_err(|_| KubelicateError::Closed)?;
            rx.await.map_err(|_| KubelicateError::Closed)?
        }

        async fn wait_for_catch_up_quorum(&self, mode: ReplicaSetQuorumMode) -> Result<()> {
            self.send_control(|reply| ReplicatorControlEvent::WaitForCatchUpQuorum { mode, reply })
                .await
        }

        async fn build_replica(&self, replica: ReplicaInfo) -> Result<()> {
            self.send_control(|reply| ReplicatorControlEvent::BuildReplica { replica, reply })
                .await
        }

        async fn remove_replica(&self, replica_id: ReplicaId) -> Result<()> {
            self.send_control(|reply| ReplicatorControlEvent::RemoveReplica { replica_id, reply })
                .await
        }

        async fn revoke_write_status(&self) -> Result<()> {
            self.state
                .set_write_status(AccessStatus::ReconfigurationPending);
            Ok(())
        }

        fn replicator_address(&self) -> String {
            self.grpc_address.clone()
        }
    }

    /// Convenience: spawn N in-process replicas.
    pub async fn spawn_replicas(count: usize) -> Result<Vec<Box<dyn ReplicaHandle>>> {
        let mut handles: Vec<Box<dyn ReplicaHandle>> = Vec::new();
        for i in 1..=(count as ReplicaId) {
            handles.push(Box::new(InProcessReplicaHandle::spawn(i).await?));
        }
        Ok(handles)
    }
}
