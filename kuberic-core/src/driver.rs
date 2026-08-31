use std::collections::{HashMap, HashSet};

use async_trait::async_trait;
use tracing::{info, warn};

use crate::error::{KubericError, RecoveryError, Result};
use crate::types::{
    CorrelatedControlActionAcknowledgement, CorrelatedControlActionRequest, DataLossAction,
    DurableReplicaAction, Epoch, Lsn, OpenMode, ReplicaId, ReplicaInfo, ReplicaInstanceId,
    ReplicaSetConfig, ReplicaSetQuorumMode, ReplicaStatus, ReplicaStatusInfo, Role,
    StablePartitionSnapshot, StableReplicaSnapshot,
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
    fn instance_id(&self) -> ReplicaInstanceId;

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
    async fn remove_replica(
        &self,
        replica_id: ReplicaId,
        instance_id: ReplicaInstanceId,
    ) -> Result<()>;

    /// Revoke write status before switchover demotion.
    /// Sets write_status = ReconfigurationPending so new writes are
    /// immediately rejected. In-flight writes continue to completion.
    async fn revoke_write_status(&self) -> Result<()>;

    /// The gRPC address where this replica's replication server listens.
    fn replicator_address(&self) -> String;

    /// Query the replica's current status directly from its PodRuntime.
    /// Used by the reconciler to detect restarted pods (epoch mismatch,
    /// role=None) and stale handles (transport errors).
    async fn get_status(&self) -> Result<ReplicaStatusInfo>;

    async fn execute_durable_action(
        &self,
        _action_id: &str,
        action: DurableReplicaAction,
    ) -> Result<()> {
        match action {
            DurableReplicaAction::Open { mode } => self.open(mode).await,
            DurableReplicaAction::Close => self.close().await,
            DurableReplicaAction::RevokeWriteStatus => self.revoke_write_status().await,
            DurableReplicaAction::ChangeRole { epoch, role } => self.change_role(epoch, role).await,
            DurableReplicaAction::UpdateEpoch { epoch } => self.update_epoch(epoch).await,
            DurableReplicaAction::UpdateCatchUpConfiguration { current, previous } => {
                self.update_catch_up_configuration(current, previous).await
            }
            DurableReplicaAction::WaitForCatchUpQuorum { mode } => {
                self.wait_for_catch_up_quorum(mode).await
            }
            DurableReplicaAction::UpdateCurrentConfiguration { current } => {
                self.update_current_configuration(current).await
            }
            DurableReplicaAction::BuildReplica { replica } => self.build_replica(replica).await,
            DurableReplicaAction::RemoveReplica {
                replica_id,
                instance_id,
            } => self.remove_replica(replica_id, instance_id).await,
            DurableReplicaAction::OnDataLoss { .. } => self.on_data_loss().await.map(|_| ()),
            DurableReplicaAction::RecordElectionConfiguration { .. } => Err(
                KubericError::Internal("election configuration observation is unsupported".into()),
            ),
        }
    }

    async fn execute_correlated_control_action(
        &self,
        _request: CorrelatedControlActionRequest,
    ) -> Result<CorrelatedControlActionAcknowledgement> {
        Err(KubericError::RemoteControlProtocolUnsupported(
            "replica handle does not support correlated control actions".to_string(),
        ))
    }
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

    /// Reconstruct a driver from the last durably committed stable snapshot.
    ///
    /// Recovery is deliberately read-only: every supplied handle is queried
    /// with `get_status`, and no other handle operation is invoked.
    pub async fn recover(
        snapshot: StablePartitionSnapshot,
        handles: Vec<Box<dyn ReplicaHandle>>,
    ) -> Result<Self> {
        if snapshot.members.is_empty() {
            return Err(RecoveryError::EmptySnapshot.into());
        }

        let mut snapshot_by_id = HashMap::new();
        let mut snapshot_instances = HashSet::new();
        let mut primary_ids = Vec::new();
        for member in &snapshot.members {
            if snapshot_by_id.insert(member.id, member).is_some() {
                return Err(RecoveryError::DuplicateReplicaId(member.id).into());
            }
            if !snapshot_instances.insert(member.instance_id.clone()) {
                return Err(RecoveryError::DuplicateInstanceId(member.instance_id.clone()).into());
            }
            match member.role {
                Role::Primary => primary_ids.push(member.id),
                Role::ActiveSecondary => {}
                role => {
                    return Err(RecoveryError::UnsupportedStableRole {
                        id: member.id,
                        role,
                    }
                    .into());
                }
            }
        }

        if !snapshot_by_id.contains_key(&snapshot.primary_id) {
            return Err(RecoveryError::PrimaryMissing(snapshot.primary_id).into());
        }
        if primary_ids.len() != 1 {
            return Err(RecoveryError::InvalidPrimaryCount(primary_ids.len()).into());
        }
        if primary_ids[0] != snapshot.primary_id {
            return Err(RecoveryError::ConflictingPrimary {
                expected: snapshot.primary_id,
                actual: primary_ids[0],
            }
            .into());
        }

        let expected_quorum = snapshot.members.len() as u32 / 2 + 1;
        if snapshot.write_quorum != expected_quorum {
            return Err(RecoveryError::InvalidWriteQuorum {
                actual: snapshot.write_quorum,
                expected: expected_quorum,
                members: snapshot.members.len(),
            }
            .into());
        }

        let mut handles_by_id = HashMap::new();
        let mut handle_instances = HashSet::new();
        for handle in handles {
            let id = handle.id();
            let instance_id = handle.instance_id();
            if handles_by_id.insert(id, handle).is_some() {
                return Err(RecoveryError::DuplicateHandleId(id).into());
            }
            if !handle_instances.insert(instance_id.clone()) {
                return Err(RecoveryError::DuplicateHandleInstanceId(instance_id).into());
            }
        }

        for member in &snapshot.members {
            let handle = handles_by_id
                .get(&member.id)
                .ok_or(RecoveryError::MissingHandle(member.id))?;
            let actual = handle.instance_id();
            if actual != member.instance_id {
                return Err(RecoveryError::HandleInstanceMismatch {
                    id: member.id,
                    expected: member.instance_id.clone(),
                    actual,
                }
                .into());
            }
        }
        if let Some(extra) = handles_by_id
            .keys()
            .find(|id| !snapshot_by_id.contains_key(id))
        {
            return Err(RecoveryError::ExtraHandle(*extra).into());
        }

        let mut statuses = HashMap::new();
        for member in &snapshot.members {
            let status = handles_by_id[&member.id].get_status().await?;
            if status.instance_id != member.instance_id {
                return Err(RecoveryError::RuntimeInstanceMismatch {
                    id: member.id,
                    expected: member.instance_id.clone(),
                    actual: status.instance_id,
                }
                .into());
            }
            if status.epoch != snapshot.epoch {
                return Err(RecoveryError::EpochMismatch {
                    id: member.id,
                    expected: snapshot.epoch,
                    actual: status.epoch,
                }
                .into());
            }
            if status.role != member.role {
                return Err(RecoveryError::RuntimeRoleMismatch {
                    id: member.id,
                    expected: member.role,
                    actual: status.role,
                }
                .into());
            }
            statuses.insert(member.id, status);
        }

        let mut replicas = HashMap::new();
        for member in &snapshot.members {
            replicas.insert(
                member.id,
                ReplicaState {
                    handle: handles_by_id.remove(&member.id).unwrap(),
                    role: member.role,
                },
            );
        }

        let members = snapshot
            .members
            .iter()
            .filter(|member| member.id != snapshot.primary_id)
            .map(|member| {
                let handle = &replicas[&member.id].handle;
                let status = &statuses[&member.id];
                ReplicaInfo {
                    id: member.id,
                    instance_id: member.instance_id.clone(),
                    role: member.role,
                    status: if status.healthy {
                        ReplicaStatus::Up
                    } else {
                        ReplicaStatus::Down
                    },
                    replicator_address: handle.replicator_address(),
                    current_progress: status.current_progress,
                    catch_up_capability: handle.catch_up_capability(),
                    must_catch_up: false,
                }
            })
            .collect();

        Ok(Self {
            replicas,
            primary_id: Some(snapshot.primary_id),
            epoch: snapshot.epoch,
            current_config: ReplicaSetConfig {
                members,
                write_quorum: snapshot.write_quorum,
            },
        })
    }

    /// Return the complete durable snapshot for the driver's current committed
    /// stable topology.
    pub fn stable_snapshot(&self) -> Result<StablePartitionSnapshot> {
        let primary_id = self.primary_id.ok_or_else(|| {
            RecoveryError::InvalidConfiguration("driver has no primary".to_string())
        })?;
        let expected_quorum = self.replicas.len() as u32 / 2 + 1;
        if self.current_config.write_quorum != expected_quorum {
            return Err(RecoveryError::InvalidConfiguration(format!(
                "write quorum {} does not match majority {}",
                self.current_config.write_quorum, expected_quorum
            ))
            .into());
        }

        let configured: HashMap<ReplicaId, &ReplicaInfo> = self
            .current_config
            .members
            .iter()
            .map(|member| (member.id, member))
            .collect();
        if configured.len() != self.current_config.members.len()
            || configured.len() + 1 != self.replicas.len()
        {
            return Err(RecoveryError::InvalidConfiguration(
                "configured secondary membership does not match driver membership".to_string(),
            )
            .into());
        }

        let mut members = Vec::with_capacity(self.replicas.len());
        for (&id, state) in &self.replicas {
            let expected_role = if id == primary_id {
                Role::Primary
            } else {
                Role::ActiveSecondary
            };
            if state.role != expected_role {
                return Err(RecoveryError::InvalidConfiguration(format!(
                    "replica {id} has role {:?}, expected {expected_role:?}",
                    state.role
                ))
                .into());
            }
            if id == primary_id {
                if configured.contains_key(&id) {
                    return Err(RecoveryError::InvalidConfiguration(
                        "primary appears in secondary configuration".to_string(),
                    )
                    .into());
                }
            } else {
                let config_member = configured.get(&id).ok_or_else(|| {
                    RecoveryError::InvalidConfiguration(format!(
                        "replica {id} is missing from secondary configuration"
                    ))
                })?;
                if config_member.instance_id != state.handle.instance_id()
                    || config_member.role != Role::ActiveSecondary
                {
                    return Err(RecoveryError::InvalidConfiguration(format!(
                        "replica {id} configuration identity or role differs from driver"
                    ))
                    .into());
                }
            }
            members.push(StableReplicaSnapshot {
                id,
                instance_id: state.handle.instance_id(),
                role: state.role,
                election_metadata: None,
            });
        }
        members.sort_by_key(|member| member.id);

        Ok(StablePartitionSnapshot {
            epoch: self.epoch,
            primary_id,
            members,
            write_quorum: self.current_config.write_quorum,
        })
    }

    /// Remove a replica from the driver's tracking without notifying
    /// the primary's replicator. Used when the reconciler detects a pod
    /// is permanently dead before failover. Returns the handle for cleanup.
    pub fn remove_replica_from_driver(&mut self, id: ReplicaId) -> Option<Box<dyn ReplicaHandle>> {
        self.replicas.remove(&id).map(|s| s.handle)
    }

    async fn abandon_failed_creation(&mut self) {
        let replicas = std::mem::take(&mut self.replicas);
        for entry in replicas.into_values() {
            let _ = entry.handle.change_role(self.epoch, Role::None).await;
            let _ = entry.handle.close().await;
        }
        self.primary_id = None;
        self.current_config = ReplicaSetConfig {
            members: Vec::new(),
            write_quorum: 0,
        };
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
                    role: Role::Unknown,
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

        // Install the initial primary-only configuration before exposing
        // writes or building secondaries. A promoted actor starts
        // unconfigured and must not treat quorum zero as success.
        let mut config = ReplicaSetConfig {
            members: vec![],
            write_quorum: 1,
        };
        self.replicas[&primary_id]
            .handle
            .update_current_configuration(config.clone())
            .await?;
        self.current_config = config.clone();

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
                instance_id: self.replicas[&id].handle.instance_id(),
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
        let mut ready_count: u32 = 1; // Primary

        for &id in &secondary_ids {
            let prev_config = config.clone();
            let addr = self.replicas[&id].handle.replicator_address();

            config.members.push(ReplicaInfo {
                id,
                instance_id: self.replicas[&id].handle.instance_id(),
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
                .update_catch_up_configuration(config.clone(), prev_config.clone())
                .await?;

            // Give gRPC connections time to establish (in-process only)
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;

            if let Err(error) = self.replicas[&primary_id]
                .handle
                .wait_for_catch_up_quorum(ReplicaSetQuorumMode::Write)
                .await
            {
                self.replicas[&primary_id]
                    .handle
                    .update_current_configuration(prev_config.clone())
                    .await?;
                self.current_config = prev_config;
                self.abandon_failed_creation().await;
                return Err(error);
            }

            self.replicas[&primary_id]
                .handle
                .update_current_configuration(config.clone())
                .await?;
            self.current_config = config.clone();
        }

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
            return Err(KubericError::Internal(
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
                    instance_id: entry.handle.instance_id(),
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

        if let Err(error) = self.replicas[&new_primary_id]
            .handle
            .wait_for_catch_up_quorum(ReplicaSetQuorumMode::Write)
            .await
        {
            // The role transition has already completed, so the surviving
            // configuration is the only valid rollback target. End the failed
            // catch-up attempt to keep future reconciles retryable.
            self.replicas[&new_primary_id]
                .handle
                .update_current_configuration(new_config.clone())
                .await?;
            self.current_config = new_config;
            warn!(
                new_primary = new_primary_id,
                error = %error,
                "failover catch-up timed out after promotion; finalized surviving configuration"
            );
            return Ok(());
        }

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
    /// 2. Wait for target to catch up (SF CatchupDuringSwap)
    /// 3. Demote old primary → ActiveSecondary
    /// 4. Promote target → Primary (SF Phase 4: Activate)
    /// 5. Distribute epoch to other secondaries (best-effort)
    /// 6. Reconfigure quorum + catchup
    pub async fn switchover(&mut self, target_id: ReplicaId) -> Result<()> {
        let old_primary_id = self.primary_id.ok_or(KubericError::NotPrimary)?;

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

        // 2. E2 fix: wait for target to catch up before demotion.
        // After revoke, the primary's LSN is frozen (no new writes). The
        // drain tasks are still delivering in-flight items to secondaries.
        // Poll until the target has received all data, then it's safe to
        // demote (which triggers close_all on the replicator).
        //
        // This matches SF's CatchupDuringSwap — the post-revoke catchup
        // that guarantees the target has all committed data before promotion.
        // Use get_status for live progress (GrpcReplicaHandle caches progress)
        let primary_lsn = match self.replicas[&old_primary_id].handle.get_status().await {
            Ok(status) => status.current_progress,
            Err(_) => self.replicas[&old_primary_id].handle.current_progress(),
        };
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            // Use get_status() to get live progress (not cached value)
            let target_progress = match self.replicas[&target_id].handle.get_status().await {
                Ok(status) => status.current_progress,
                Err(_) => -1,
            };
            if target_progress >= primary_lsn {
                info!(
                    target_id,
                    target_progress, primary_lsn, "target caught up — proceeding with demotion"
                );
                break;
            }
            if tokio::time::Instant::now() > deadline {
                warn!(
                    target_id,
                    target_progress,
                    primary_lsn,
                    "switchover catchup timeout — aborting, restoring write status"
                );
                // Abort: re-grant write status on old primary
                self.replicas[&old_primary_id]
                    .handle
                    .change_role(self.epoch, Role::Primary)
                    .await?;
                self.replicas[&old_primary_id]
                    .handle
                    .update_current_configuration(self.current_config.clone())
                    .await?;
                return Err(KubericError::Internal(
                    "switchover catchup timeout — target did not catch up".into(),
                ));
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        // 3. Demote old primary → ActiveSecondary
        self.replicas[&old_primary_id]
            .handle
            .change_role(new_epoch, Role::ActiveSecondary)
            .await?;
        self.replicas.get_mut(&old_primary_id).unwrap().role = Role::ActiveSecondary;

        // 4. Promote target → Primary (SF Phase 4: Activate)
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
            Err(_) => Some(KubericError::Internal("promotion timed out".into())),
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
            self.replicas[&old_primary_id]
                .handle
                .update_catch_up_configuration(
                    self.current_config.clone(),
                    ReplicaSetConfig {
                        members: Vec::new(),
                        write_quorum: 0,
                    },
                )
                .await?;
            self.replicas[&old_primary_id]
                .handle
                .update_current_configuration(self.current_config.clone())
                .await?;
            self.replicas.get_mut(&old_primary_id).unwrap().role = Role::Primary;
            self.primary_id = Some(old_primary_id);
            return Err(e);
        }
        self.replicas.get_mut(&target_id).unwrap().role = Role::Primary;
        self.primary_id = Some(target_id);

        // 5. Distribute epoch to other secondaries (best-effort).
        // Unreachable secondaries are skipped — they'll be rebuilt later.
        // The old primary already has the epoch from step 3 (change_role).
        // The target already has it from step 4.
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
                    instance_id: entry.handle.instance_id(),
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

        if let Err(error) = self.replicas[&target_id]
            .handle
            .wait_for_catch_up_quorum(ReplicaSetQuorumMode::Write)
            .await
        {
            // Promotion already succeeded. Finalize the role-correct
            // configuration to end the failed attempt before returning the
            // catch-up error to the caller.
            self.replicas[&target_id]
                .handle
                .update_current_configuration(new_config.clone())
                .await?;
            self.current_config = new_config;
            warn!(
                new_primary = target_id,
                error = %error,
                "switchover catch-up timed out after promotion; finalized role-correct configuration"
            );
            return Ok(());
        }

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
        let primary_id = self.primary_id.ok_or(KubericError::NotPrimary)?;
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
                    instance_id: entry.handle.instance_id(),
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

        if let Err(error) = self.replicas[&primary_id]
            .handle
            .wait_for_catch_up_quorum(ReplicaSetQuorumMode::Write)
            .await
        {
            self.replicas[&primary_id]
                .handle
                .update_current_configuration(self.current_config.clone())
                .await?;
            return Err(error);
        }

        self.replicas[&primary_id]
            .handle
            .update_current_configuration(new_config.clone())
            .await?;

        self.current_config = new_config;

        // 2. Remove the exact old primary-side connection.
        let removed_instance_id = self.replicas[&secondary_id].handle.instance_id();
        if let Err(error) = self.replicas[&primary_id]
            .handle
            .remove_replica(secondary_id, removed_instance_id)
            .await
        {
            warn!(
                secondary_id,
                error = %error,
                "current configuration committed but exact connection cleanup failed"
            );
        }

        // 3. Close the removed replica
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

    /// Retire a stale secondary incarnation before its replacement is added.
    ///
    /// The stable configuration remains unchanged until `add_replica` installs
    /// the replacement, but the primary-side connection is removed precisely
    /// by incarnation so a delayed stale request cannot remove a newer pod.
    pub async fn remove_stale_secondary(&mut self, secondary_id: ReplicaId) -> Result<()> {
        let primary_id = self.primary_id.ok_or(KubericError::NotPrimary)?;
        assert_ne!(
            secondary_id, primary_id,
            "cannot retire the primary as a stale secondary"
        );
        let instance_id = self
            .replicas
            .get(&secondary_id)
            .unwrap_or_else(|| panic!("replica {secondary_id} not found"))
            .handle
            .instance_id();

        self.replicas[&primary_id]
            .handle
            .remove_replica(secondary_id, instance_id.clone())
            .await?;

        self.replicas.remove(&secondary_id);
        info!(
            secondary_id,
            instance_id = %instance_id,
            "stale secondary incarnation retired"
        );
        Ok(())
    }

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
        let primary_id = self.primary_id.ok_or(KubericError::NotPrimary)?;
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

        // 1. Open + set epoch + assign idle role (fallible — don't insert yet)
        handle.open(OpenMode::New).await?;
        let setup_result = async {
            handle.update_epoch(epoch).await?;
            handle.change_role(epoch, Role::IdleSecondary).await?;

            // 2. build_replica on primary (copies state via data plane)
            let replica_info = ReplicaInfo {
                id: replica_id,
                instance_id: handle.instance_id(),
                role: Role::IdleSecondary,
                status: ReplicaStatus::Up,
                replicator_address: handle.replicator_address(),
                current_progress: -1,
                catch_up_capability: -1,
                must_catch_up: false,
            };
            self.replicas[&primary_id]
                .handle
                .build_replica(replica_info)
                .await?;

            // 3. Promote idle → active
            handle.change_role(epoch, Role::ActiveSecondary).await
        }
        .await;
        if let Err(error) = setup_result {
            let _ = handle.change_role(epoch, Role::None).await;
            let _ = handle.close().await;
            return Err(error);
        }

        // All fallible ops succeeded — now insert into driver state
        self.replicas.insert(
            replica_id,
            ReplicaState {
                handle,
                role: Role::ActiveSecondary,
            },
        );

        // 4. Reconfigure quorum (rebuild full config, must_catch_up on new replica)
        if let Err(error) = self.reconfigure_quorum(primary_id, Some(replica_id)).await {
            // The handle was inserted before catch-up so the new configuration
            // could be built. Roll back both driver state and the primary's
            // dual configuration; otherwise the reconciler sees the replica
            // as present and cannot retry the failed scale-up.
            if let Some(replica) = self.replicas.remove(&replica_id) {
                let instance_id = replica.handle.instance_id();
                if let Err(cleanup_error) = self.replicas[&primary_id]
                    .handle
                    .remove_replica(replica_id, instance_id.clone())
                    .await
                {
                    warn!(
                        replica_id,
                        instance_id = %instance_id,
                        error = %cleanup_error,
                        "failed to remove replacement connection after add rollback"
                    );
                }
                let _ = replica.handle.change_role(epoch, Role::None).await;
                let _ = replica.handle.close().await;
            }
            return Err(error);
        }

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
        let primary_id = self.primary_id.ok_or(KubericError::NotPrimary)?;
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

        // 1. Remove and close the old secondary (best effort — it may be
        // dead). Restore it only if the primary cannot remove its old
        // connection; after replacement starts, restoring the old handle
        // could diverge from the primary's installed incarnation.
        let old = self.replicas.remove(&secondary_id).unwrap();
        let _ = old.handle.close().await;

        // Drop the primary's sender entry for this ID before connecting the
        // replacement. Restoring the stable configuration after a failed
        // attempt intentionally retains the member ID, so configuration
        // pruning alone cannot distinguish the old endpoint from the new one.
        let old_instance_id = old.handle.instance_id();
        if let Err(error) = self.replicas[&primary_id]
            .handle
            .remove_replica(secondary_id, old_instance_id)
            .await
        {
            self.replicas.insert(secondary_id, old);
            return Err(error);
        }

        // 2. Add the replacement under the same ID.
        assert_eq!(new_handle.id(), secondary_id);
        self.add_replica(new_handle).await?;
        Ok(())
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
                    instance_id: entry.handle.instance_id(),
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

        if let Err(error) = self.replicas[&primary_id]
            .handle
            .update_catch_up_configuration(new_config.clone(), self.current_config.clone())
            .await
        {
            self.replicas[&primary_id]
                .handle
                .update_current_configuration(self.current_config.clone())
                .await?;
            return Err(error);
        }

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        if let Err(error) = self.replicas[&primary_id]
            .handle
            .wait_for_catch_up_quorum(ReplicaSetQuorumMode::Write)
            .await
        {
            // Reconfiguration has not committed to driver state. Restore the
            // last stable configuration so the caller/reconciler can retry.
            self.replicas[&primary_id]
                .handle
                .update_current_configuration(self.current_config.clone())
                .await?;
            return Err(error);
        }

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
    use std::sync::atomic::{AtomicBool, Ordering};

    use tokio::sync::{mpsc, oneshot};
    use tonic::transport::Server;

    use crate::events::{ReplicateRequest, ReplicatorControlEvent};
    use crate::handles::{PartitionState, StateReplicatorHandle};
    use crate::proto::replicator_data_server::ReplicatorDataServer;
    use crate::replicator::actor::WalReplicatorActor;
    use crate::replicator::secondary::{SecondaryReceiver, SecondaryState};
    use crate::types::{AccessStatus, CancellationToken, ReplicaStatusInfo};

    /// In-process replica handle: wraps channels to a local replicator actor
    /// and a local gRPC secondary server.
    pub struct InProcessReplicaHandle {
        id: ReplicaId,
        instance_id: ReplicaInstanceId,
        control_tx: mpsc::Sender<ReplicatorControlEvent>,
        data_tx: mpsc::Sender<ReplicateRequest>,
        state: Arc<PartitionState>,
        pub secondary_state: Arc<SecondaryState>,
        grpc_address: String,
        shutdown_token: CancellationToken,
        role: std::sync::Mutex<Role>,
        epoch: std::sync::Mutex<Epoch>,
        _actor_handle: tokio::task::JoinHandle<()>,
        _grpc_handle: tokio::task::JoinHandle<()>,
        bypass_build: AtomicBool,
        bypass_update_epoch: AtomicBool,
        fail_next_catch_up: AtomicBool,
    }

    impl InProcessReplicaHandle {
        /// Spawn a new in-process replica (actor + gRPC server).
        pub async fn spawn(id: ReplicaId) -> Result<Self> {
            static NEXT_INSTANCE: std::sync::atomic::AtomicU64 =
                std::sync::atomic::AtomicU64::new(1);
            let generation = NEXT_INSTANCE.fetch_add(1, Ordering::Relaxed);
            let (control_tx, control_rx) = mpsc::channel(16);
            let (data_tx, data_rx) = mpsc::channel::<ReplicateRequest>(256);
            let state = Arc::new(PartitionState::new());
            let secondary_state = Arc::new(SecondaryState::new());
            let shutdown_token = CancellationToken::new();

            // Start gRPC server with graceful shutdown
            let receiver = SecondaryReceiver::new(secondary_state.clone());
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .map_err(|e| KubericError::Internal(Box::new(e)))?;
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
            // In-process replicas create a dummy state_provider_tx (not used in tests)
            let (sp_tx, _sp_rx) = mpsc::unbounded_channel();
            let actor_handle = tokio::spawn(async move {
                actor.run(control_rx, data_rx, state_cp, sp_tx).await;
            });

            Ok(Self {
                id,
                instance_id: ReplicaInstanceId::new(format!("in-process-{id}-{generation}")),
                control_tx,
                data_tx,
                state,
                secondary_state,
                grpc_address,
                shutdown_token,
                role: std::sync::Mutex::new(Role::Unknown),
                epoch: std::sync::Mutex::new(Epoch::default()),
                _actor_handle: actor_handle,
                _grpc_handle: grpc_handle,
                bypass_build: AtomicBool::new(false),
                bypass_update_epoch: AtomicBool::new(false),
                fail_next_catch_up: AtomicBool::new(false),
            })
        }

        /// Arrange for the next add-replica workflow driven through this
        /// primary to reach the catch-up step and fail there.
        #[cfg(test)]
        pub fn inject_add_replica_catch_up_failure(&self) {
            self.bypass_build.store(true, Ordering::Release);
            self.fail_next_catch_up.store(true, Ordering::Release);
        }

        #[cfg(test)]
        pub fn bypass_update_epoch_for_add_replica_test(&self) {
            self.bypass_update_epoch.store(true, Ordering::Release);
        }

        async fn send_control(
            &self,
            make: impl FnOnce(oneshot::Sender<Result<()>>) -> ReplicatorControlEvent,
        ) -> Result<()> {
            let (tx, rx) = oneshot::channel();
            self.control_tx
                .send(make(tx))
                .await
                .map_err(|_| KubericError::Closed)?;
            rx.await.map_err(|_| KubericError::Closed)?
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

        fn instance_id(&self) -> ReplicaInstanceId {
            self.instance_id.clone()
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
            *self.role.lock().unwrap() = role;
            *self.epoch.lock().unwrap() = epoch;
            // Note: do NOT call secondary_state.update_epoch here — the production
            // PodRuntime::handle_change_role does not update SecondaryState epoch.
            // SecondaryState epoch is only updated via explicit update_epoch() calls.
            self.send_control(|reply| ReplicatorControlEvent::ChangeRole { epoch, role, reply })
                .await?;
            // Mirror PodRuntime: set access status based on role
            match role {
                Role::Primary => {
                    self.state.set_read_status(AccessStatus::Granted);
                    self.state
                        .set_write_status(AccessStatus::ReconfigurationPending);
                }
                _ => {
                    self.state.set_read_status(AccessStatus::NotPrimary);
                    self.state.set_write_status(AccessStatus::NotPrimary);
                }
            }
            Ok(())
        }

        async fn update_epoch(&self, epoch: Epoch) -> Result<()> {
            *self.epoch.lock().unwrap() = epoch;
            self.secondary_state.update_epoch(epoch);
            if self.bypass_update_epoch.load(Ordering::Acquire) {
                return Ok(());
            }
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
                .send(ReplicatorControlEvent::OnDataLoss {
                    expected_epoch: None,
                    reply: tx,
                })
                .await
                .map_err(|_| KubericError::Closed)?;
            rx.await.map_err(|_| KubericError::Closed)?
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
                .map_err(|_| KubericError::Closed)?;
            let result = rx.await.map_err(|_| KubericError::Closed)?;
            if result.is_ok() && *self.role.lock().unwrap() == Role::Primary {
                self.state.set_write_status(AccessStatus::Granted);
            }
            result
        }

        async fn update_current_configuration(&self, current: ReplicaSetConfig) -> Result<()> {
            let (tx, rx) = oneshot::channel();
            self.control_tx
                .send(ReplicatorControlEvent::UpdateCurrentConfiguration { current, reply: tx })
                .await
                .map_err(|_| KubericError::Closed)?;
            let result = rx.await.map_err(|_| KubericError::Closed)?;
            if result.is_ok() && *self.role.lock().unwrap() == Role::Primary {
                self.state.set_write_status(AccessStatus::Granted);
            }
            result
        }

        async fn wait_for_catch_up_quorum(&self, mode: ReplicaSetQuorumMode) -> Result<()> {
            if self.fail_next_catch_up.swap(false, Ordering::AcqRel) {
                return Err(KubericError::NoWriteQuorum);
            }
            self.send_control(|reply| ReplicatorControlEvent::WaitForCatchUpQuorum { mode, reply })
                .await
        }

        async fn build_replica(&self, replica: ReplicaInfo) -> Result<()> {
            if self.bypass_build.load(Ordering::Acquire) {
                return Ok(());
            }
            self.send_control(|reply| ReplicatorControlEvent::BuildReplica { replica, reply })
                .await
        }

        async fn remove_replica(
            &self,
            replica_id: ReplicaId,
            instance_id: ReplicaInstanceId,
        ) -> Result<()> {
            self.send_control(|reply| ReplicatorControlEvent::RemoveReplica {
                replica_id,
                instance_id,
                reply,
            })
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

        async fn get_status(&self) -> Result<ReplicaStatusInfo> {
            let role = *self.role.lock().unwrap();
            let epoch = *self.epoch.lock().unwrap();
            Ok(ReplicaStatusInfo {
                instance_id: self.instance_id.clone(),
                role,
                epoch,
                current_progress: self.state.current_progress(),
                catch_up_capability: self.state.observed_catch_up_capability(),
                committed_lsn: self.state.committed_lsn(),
                healthy: true,
                write_status: self.state.write_status(),
                configuration: None,
                election_configuration: None,
                deactivation_info: None,
                last_completed_action: None,
                durable_action: None,
                active_replica_connections: Vec::new(),
                agent: None,
            })
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::driver::testing::{InProcessReplicaHandle, spawn_replicas};
    use crate::types::AccessStatus;
    use std::sync::{Arc, Mutex};

    #[derive(Clone)]
    struct RecoveryTestHandle {
        id: ReplicaId,
        instance_id: ReplicaInstanceId,
        status: ReplicaStatusInfo,
        operations: Arc<Mutex<Vec<&'static str>>>,
    }

    impl RecoveryTestHandle {
        fn new(
            id: ReplicaId,
            instance: &str,
            role: Role,
            epoch: Epoch,
            operations: Arc<Mutex<Vec<&'static str>>>,
        ) -> Self {
            Self {
                id,
                instance_id: ReplicaInstanceId::new(instance),
                status: ReplicaStatusInfo {
                    instance_id: ReplicaInstanceId::new(instance),
                    role,
                    epoch,
                    current_progress: id,
                    catch_up_capability: Some(id),
                    committed_lsn: id,
                    healthy: true,
                    write_status: if role == Role::Primary {
                        AccessStatus::Granted
                    } else {
                        AccessStatus::NotPrimary
                    },
                    configuration: None,
                    election_configuration: None,
                    deactivation_info: None,
                    last_completed_action: None,
                    durable_action: None,
                    active_replica_connections: Vec::new(),
                    agent: None,
                },
                operations,
            }
        }

        fn record(&self, operation: &'static str) {
            self.operations.lock().unwrap().push(operation);
        }
    }

    #[async_trait]
    impl ReplicaHandle for RecoveryTestHandle {
        fn id(&self) -> ReplicaId {
            self.id
        }

        fn instance_id(&self) -> ReplicaInstanceId {
            self.instance_id.clone()
        }

        async fn open(&self, _: OpenMode) -> Result<()> {
            self.record("Open");
            Ok(())
        }

        async fn close(&self) -> Result<()> {
            self.record("Close");
            Ok(())
        }

        fn abort(&self) {
            self.record("Abort");
        }

        async fn change_role(&self, _: Epoch, _: Role) -> Result<()> {
            self.record("ChangeRole");
            Ok(())
        }

        async fn update_epoch(&self, _: Epoch) -> Result<()> {
            self.record("UpdateEpoch");
            Ok(())
        }

        fn current_progress(&self) -> Lsn {
            self.status.current_progress
        }

        fn catch_up_capability(&self) -> Lsn {
            self.status.catch_up_capability.unwrap_or(0)
        }

        async fn on_data_loss(&self) -> Result<DataLossAction> {
            self.record("OnDataLoss");
            Ok(DataLossAction::None)
        }

        async fn update_catch_up_configuration(
            &self,
            _: ReplicaSetConfig,
            _: ReplicaSetConfig,
        ) -> Result<()> {
            self.record("UpdateCatchUpConfiguration");
            Ok(())
        }

        async fn update_current_configuration(&self, _: ReplicaSetConfig) -> Result<()> {
            self.record("UpdateCurrentConfiguration");
            Ok(())
        }

        async fn wait_for_catch_up_quorum(&self, _: ReplicaSetQuorumMode) -> Result<()> {
            self.record("WaitForCatchUpQuorum");
            Ok(())
        }

        async fn build_replica(&self, _: ReplicaInfo) -> Result<()> {
            self.record("BuildReplica");
            Ok(())
        }

        async fn remove_replica(&self, _: ReplicaId, _: ReplicaInstanceId) -> Result<()> {
            self.record("RemoveReplica");
            Ok(())
        }

        async fn revoke_write_status(&self) -> Result<()> {
            self.record("RevokeWriteStatus");
            Ok(())
        }

        fn replicator_address(&self) -> String {
            format!("http://replica-{}", self.id)
        }

        async fn get_status(&self) -> Result<ReplicaStatusInfo> {
            self.record("GetStatus");
            Ok(self.status.clone())
        }
    }

    fn recovery_snapshot() -> StablePartitionSnapshot {
        StablePartitionSnapshot {
            epoch: Epoch::new(4, 9),
            primary_id: 1,
            members: vec![
                StableReplicaSnapshot {
                    id: 1,
                    instance_id: ReplicaInstanceId::new("one"),
                    role: Role::Primary,
                    election_metadata: None,
                },
                StableReplicaSnapshot {
                    id: 2,
                    instance_id: ReplicaInstanceId::new("two"),
                    role: Role::ActiveSecondary,
                    election_metadata: None,
                },
                StableReplicaSnapshot {
                    id: 3,
                    instance_id: ReplicaInstanceId::new("three"),
                    role: Role::ActiveSecondary,
                    election_metadata: None,
                },
            ],
            write_quorum: 2,
        }
    }

    fn recovery_handles(
        snapshot: &StablePartitionSnapshot,
        operations: Arc<Mutex<Vec<&'static str>>>,
    ) -> Vec<Box<dyn ReplicaHandle>> {
        snapshot
            .members
            .iter()
            .map(|member| {
                Box::new(RecoveryTestHandle::new(
                    member.id,
                    member.instance_id.as_str(),
                    member.role,
                    snapshot.epoch,
                    operations.clone(),
                )) as Box<dyn ReplicaHandle>
            })
            .collect()
    }

    fn assert_recovery_error(result: Result<PartitionDriver>, expected: RecoveryError) {
        match result {
            Err(KubericError::Recovery(actual)) => assert_eq!(actual, expected),
            Err(other) => panic!("unexpected recovery error: {other}"),
            Ok(_) => panic!("recovery unexpectedly succeeded"),
        }
    }

    #[tokio::test]
    async fn recovery_is_status_only_and_reconstructs_stable_state() {
        let snapshot = recovery_snapshot();
        let operations = Arc::new(Mutex::new(Vec::new()));
        let handles = recovery_handles(&snapshot, operations.clone());

        let mut driver = PartitionDriver::recover(snapshot.clone(), handles)
            .await
            .unwrap();

        assert_eq!(*operations.lock().unwrap(), vec!["GetStatus"; 3]);
        assert_eq!(driver.primary_id(), Some(1));
        assert_eq!(driver.epoch(), snapshot.epoch);
        assert_eq!(driver.stable_snapshot().unwrap(), snapshot);

        // Recovered state is operational input to subsequent stable workflows.
        driver.switchover(2).await.unwrap();
        assert_eq!(driver.primary_id(), Some(2));
        assert_eq!(driver.epoch(), Epoch::new(4, 10));
    }

    #[tokio::test]
    async fn recovery_accepts_unhealthy_runtime_status() {
        let snapshot = recovery_snapshot();
        let operations = Arc::new(Mutex::new(Vec::new()));
        let mut handles = recovery_handles(&snapshot, operations.clone());
        let mut unhealthy = RecoveryTestHandle::new(
            3,
            "three",
            Role::ActiveSecondary,
            snapshot.epoch,
            operations,
        );
        unhealthy.status.healthy = false;
        handles[2] = Box::new(unhealthy);

        let driver = PartitionDriver::recover(snapshot.clone(), handles)
            .await
            .unwrap();
        assert_eq!(driver.stable_snapshot().unwrap(), snapshot);
    }

    #[tokio::test]
    async fn recovery_rejects_snapshot_identity_and_configuration_errors_without_mutation() {
        let cases: Vec<(StablePartitionSnapshot, RecoveryError)> = vec![
            (
                StablePartitionSnapshot {
                    members: vec![],
                    ..recovery_snapshot()
                },
                RecoveryError::EmptySnapshot,
            ),
            (
                {
                    let mut snapshot = recovery_snapshot();
                    snapshot.members[1].id = 1;
                    snapshot
                },
                RecoveryError::DuplicateReplicaId(1),
            ),
            (
                {
                    let mut snapshot = recovery_snapshot();
                    snapshot.members[1].instance_id = ReplicaInstanceId::new("one");
                    snapshot
                },
                RecoveryError::DuplicateInstanceId(ReplicaInstanceId::new("one")),
            ),
            (
                {
                    let mut snapshot = recovery_snapshot();
                    snapshot.primary_id = 99;
                    snapshot
                },
                RecoveryError::PrimaryMissing(99),
            ),
            (
                {
                    let mut snapshot = recovery_snapshot();
                    snapshot.members[0].role = Role::ActiveSecondary;
                    snapshot
                },
                RecoveryError::InvalidPrimaryCount(0),
            ),
            (
                {
                    let mut snapshot = recovery_snapshot();
                    snapshot.members[0].role = Role::ActiveSecondary;
                    snapshot.members[1].role = Role::Primary;
                    snapshot
                },
                RecoveryError::ConflictingPrimary {
                    expected: 1,
                    actual: 2,
                },
            ),
            (
                {
                    let mut snapshot = recovery_snapshot();
                    snapshot.members[2].role = Role::IdleSecondary;
                    snapshot
                },
                RecoveryError::UnsupportedStableRole {
                    id: 3,
                    role: Role::IdleSecondary,
                },
            ),
            (
                {
                    let mut snapshot = recovery_snapshot();
                    snapshot.write_quorum = 3;
                    snapshot
                },
                RecoveryError::InvalidWriteQuorum {
                    actual: 3,
                    expected: 2,
                    members: 3,
                },
            ),
        ];

        for (snapshot, expected) in cases {
            let operations = Arc::new(Mutex::new(Vec::new()));
            let handles = recovery_handles(&recovery_snapshot(), operations.clone());
            assert_recovery_error(PartitionDriver::recover(snapshot, handles).await, expected);
            assert!(
                operations
                    .lock()
                    .unwrap()
                    .iter()
                    .all(|operation| *operation == "GetStatus"),
                "rejected recovery invoked a mutating operation"
            );
        }
    }

    #[tokio::test]
    async fn recovery_rejects_live_bijection_errors_without_mutation() {
        let snapshot = recovery_snapshot();

        let operations = Arc::new(Mutex::new(Vec::new()));
        let mut missing = recovery_handles(&snapshot, operations.clone());
        missing.pop();
        assert_recovery_error(
            PartitionDriver::recover(snapshot.clone(), missing).await,
            RecoveryError::MissingHandle(3),
        );
        assert!(operations.lock().unwrap().is_empty());

        let operations = Arc::new(Mutex::new(Vec::new()));
        let mut extra = recovery_handles(&snapshot, operations.clone());
        extra.push(Box::new(RecoveryTestHandle::new(
            4,
            "four",
            Role::ActiveSecondary,
            snapshot.epoch,
            operations.clone(),
        )));
        assert_recovery_error(
            PartitionDriver::recover(snapshot.clone(), extra).await,
            RecoveryError::ExtraHandle(4),
        );

        let operations = Arc::new(Mutex::new(Vec::new()));
        let mut duplicate_id = recovery_handles(&snapshot, operations.clone());
        duplicate_id.push(Box::new(RecoveryTestHandle::new(
            1,
            "other",
            Role::Primary,
            snapshot.epoch,
            operations.clone(),
        )));
        assert_recovery_error(
            PartitionDriver::recover(snapshot.clone(), duplicate_id).await,
            RecoveryError::DuplicateHandleId(1),
        );

        let operations = Arc::new(Mutex::new(Vec::new()));
        let mut duplicate_instance = recovery_handles(&snapshot, operations.clone());
        duplicate_instance[1] = Box::new(RecoveryTestHandle::new(
            2,
            "one",
            Role::ActiveSecondary,
            snapshot.epoch,
            operations.clone(),
        ));
        assert_recovery_error(
            PartitionDriver::recover(snapshot.clone(), duplicate_instance).await,
            RecoveryError::DuplicateHandleInstanceId(ReplicaInstanceId::new("one")),
        );

        let operations = Arc::new(Mutex::new(Vec::new()));
        let mut mismatch = recovery_handles(&snapshot, operations.clone());
        mismatch[1] = Box::new(RecoveryTestHandle::new(
            2,
            "replacement",
            Role::ActiveSecondary,
            snapshot.epoch,
            operations.clone(),
        ));
        assert_recovery_error(
            PartitionDriver::recover(snapshot, mismatch).await,
            RecoveryError::HandleInstanceMismatch {
                id: 2,
                expected: ReplicaInstanceId::new("two"),
                actual: ReplicaInstanceId::new("replacement"),
            },
        );
        assert!(
            operations
                .lock()
                .unwrap()
                .iter()
                .all(|operation| *operation == "GetStatus")
        );
    }

    #[tokio::test]
    async fn recovery_rejects_runtime_attestation_errors_without_mutation() {
        let snapshot = recovery_snapshot();

        let operations = Arc::new(Mutex::new(Vec::new()));
        let mut handles = recovery_handles(&snapshot, operations.clone());
        let mut stale = RecoveryTestHandle::new(
            2,
            "two",
            Role::ActiveSecondary,
            snapshot.epoch,
            operations.clone(),
        );
        stale.status.instance_id = ReplicaInstanceId::new("stale");
        handles[1] = Box::new(stale);
        assert_recovery_error(
            PartitionDriver::recover(snapshot.clone(), handles).await,
            RecoveryError::RuntimeInstanceMismatch {
                id: 2,
                expected: ReplicaInstanceId::new("two"),
                actual: ReplicaInstanceId::new("stale"),
            },
        );

        let operations = Arc::new(Mutex::new(Vec::new()));
        let mut handles = recovery_handles(&snapshot, operations.clone());
        let mut wrong_epoch = RecoveryTestHandle::new(
            2,
            "two",
            Role::ActiveSecondary,
            snapshot.epoch,
            operations.clone(),
        );
        wrong_epoch.status.epoch = Epoch::new(4, 8);
        handles[1] = Box::new(wrong_epoch);
        assert_recovery_error(
            PartitionDriver::recover(snapshot.clone(), handles).await,
            RecoveryError::EpochMismatch {
                id: 2,
                expected: snapshot.epoch,
                actual: Epoch::new(4, 8),
            },
        );

        let operations = Arc::new(Mutex::new(Vec::new()));
        let mut handles = recovery_handles(&snapshot, operations.clone());
        let wrong_role =
            RecoveryTestHandle::new(2, "two", Role::Primary, snapshot.epoch, operations.clone());
        handles[1] = Box::new(wrong_role);
        assert_recovery_error(
            PartitionDriver::recover(snapshot, handles).await,
            RecoveryError::RuntimeRoleMismatch {
                id: 2,
                expected: Role::ActiveSecondary,
                actual: Role::Primary,
            },
        );

        let observed = operations.lock().unwrap();
        assert!(observed.iter().all(|operation| *operation == "GetStatus"));
    }

    #[test]
    fn stable_snapshot_rejects_incomplete_driver_configuration() {
        assert_recovery_error(
            PartitionDriver::new()
                .stable_snapshot()
                .map(|_| PartitionDriver::new()),
            RecoveryError::InvalidConfiguration("driver has no primary".to_string()),
        );
    }

    /// E1: add_replica used to insert into self.replicas BEFORE fallible ops.
    /// If open() failed, the zombie replica stayed in the map.
    /// Fixed: insertion is deferred until all fallible ops succeed.
    #[tokio::test]
    async fn test_add_replica_cleans_up_on_failure() {
        // 1. Set up a primary-only partition (no secondaries — build_replica
        //    requires a real state provider which InProcessReplicaHandle lacks)
        let handles = spawn_replicas(1).await.unwrap();
        let mut driver = PartitionDriver::new();
        driver.create_partition(handles).await.unwrap();
        assert_eq!(driver.replica_ids().len(), 1);
        assert_eq!(driver.primary_id(), Some(1));

        // 2. Spawn replica 2, then abort it so its actor is dead
        let handle2 = InProcessReplicaHandle::spawn(2).await.unwrap();
        handle2.abort();
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // 3. add_replica should fail (open() sends to a dead channel)
        let result = driver.add_replica(Box::new(handle2)).await;
        assert!(result.is_err(), "add_replica should fail on aborted handle");

        // 4. FIXED: no zombie — replica 2 should NOT be in the map
        assert!(
            !driver.replica_ids().contains(&2),
            "after fix: failed add_replica must not leave zombie in driver"
        );
        assert_eq!(driver.replica_ids().len(), 1, "only the primary remains");
    }

    #[tokio::test]
    async fn test_add_replica_rolls_back_after_catch_up_failure() {
        let primary = InProcessReplicaHandle::spawn(1).await.unwrap();
        let writer = primary.state_replicator();
        primary.inject_add_replica_catch_up_failure();

        let mut driver = PartitionDriver::new();
        driver
            .create_partition(vec![Box::new(primary)])
            .await
            .unwrap();

        let replica = InProcessReplicaHandle::spawn(2).await.unwrap();
        replica.bypass_update_epoch_for_add_replica_test();
        let result = driver.add_replica(Box::new(replica)).await;
        assert!(
            matches!(result, Err(KubericError::NoWriteQuorum)),
            "unexpected add_replica result: {result:?}"
        );
        assert_eq!(driver.replica_ids(), vec![1]);

        // Restoring the prior single-replica configuration clears the failed
        // catch-up attempt and allows the primary to continue committing.
        let lsn = writer
            .replicate(
                bytes::Bytes::from_static(b"after-rollback"),
                crate::types::CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(lsn, 1);
    }
}
