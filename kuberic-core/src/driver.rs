use std::collections::{HashMap, HashSet};

use async_trait::async_trait;

use crate::error::{RecoveryError, Result};
use crate::types::{
    CorrelatedControlActionAcknowledgement, CorrelatedControlActionRequest, Lsn, ReplicaId,
    ReplicaInfo, ReplicaInstanceId, ReplicaSetConfig, ReplicaStatus, ReplicaStatusInfo, Role,
    StablePartitionSnapshot, StableReplicaSnapshot,
};

// ---------------------------------------------------------------------------
// ReplicaHandle trait — the production control boundary for one replica
// ---------------------------------------------------------------------------

/// Read/status access plus the single correlated mutation path for one
/// replica. The operator implements this over gRPC.
#[async_trait]
pub trait ReplicaHandle: Send + Sync {
    fn id(&self) -> ReplicaId;
    fn instance_id(&self) -> ReplicaInstanceId;

    fn current_progress(&self) -> Lsn;
    fn catch_up_capability(&self) -> Lsn;

    fn control_address(&self) -> String;
    fn replicator_address(&self) -> String;

    async fn get_status(&self) -> Result<ReplicaStatusInfo>;

    async fn execute_correlated_control_action(
        &self,
        request: CorrelatedControlActionRequest,
    ) -> Result<CorrelatedControlActionAcknowledgement>;
}

// ---------------------------------------------------------------------------
// PartitionDriver — read-only stable topology reconstruction
// ---------------------------------------------------------------------------

/// Read-only stable topology reconstructed from CRD status and current
/// replica observations.
///
/// Durable topology mutations are owned by the operator state machines. This
/// type deliberately has no mutable workflow API.
pub struct PartitionDriver {
    replicas: HashMap<ReplicaId, ReplicaState>,
    primary_id: Option<ReplicaId>,
    epoch: crate::types::Epoch,
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
            epoch: crate::types::Epoch::new(0, 0),
            current_config: ReplicaSetConfig {
                members: vec![],
                write_quorum: 0,
            },
        }
    }

    pub fn primary_id(&self) -> Option<ReplicaId> {
        self.primary_id
    }

    pub fn epoch(&self) -> crate::types::Epoch {
        self.epoch
    }

    pub fn replica_ids(&self) -> Vec<ReplicaId> {
        self.replicas.keys().copied().collect()
    }

    pub fn handle(&self, id: ReplicaId) -> Option<&dyn ReplicaHandle> {
        self.replicas.get(&id).map(|state| state.handle.as_ref())
    }

    /// Reconstruct a driver from the last durably committed stable snapshot.
    ///
    /// Recovery is deliberately read-only: every supplied handle is queried
    /// with `get_status`, and no mutation is issued.
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

    /// Return the complete durable snapshot for this reconstructed stable
    /// topology.
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
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::error::KubericError;
    use crate::replica_agent::CORRELATED_CONTROL_PROTOCOL_VERSION;
    use crate::replica_lifecycle::REPLICA_LIFECYCLE_PEER_PROTOCOL_VERSION;
    use crate::types::{
        AccessStatus, AgentControlVersion, AgentGeneration, Epoch, ReplicaAgentStatus,
        ReplicaStatusInfo,
    };

    struct RecoveryHandle {
        id: ReplicaId,
        instance_id: ReplicaInstanceId,
        status: ReplicaStatusInfo,
        status_calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl ReplicaHandle for RecoveryHandle {
        fn id(&self) -> ReplicaId {
            self.id
        }

        fn instance_id(&self) -> ReplicaInstanceId {
            self.instance_id.clone()
        }

        fn current_progress(&self) -> Lsn {
            self.status.current_progress
        }

        fn catch_up_capability(&self) -> Lsn {
            self.status.catch_up_capability.unwrap_or_default()
        }

        fn control_address(&self) -> String {
            format!("http://replica-{}-control", self.id)
        }

        fn replicator_address(&self) -> String {
            format!("http://replica-{}", self.id)
        }

        async fn get_status(&self) -> Result<ReplicaStatusInfo> {
            self.status_calls.fetch_add(1, Ordering::SeqCst);
            Ok(self.status.clone())
        }

        async fn execute_correlated_control_action(
            &self,
            _request: CorrelatedControlActionRequest,
        ) -> Result<CorrelatedControlActionAcknowledgement> {
            panic!("stable recovery must not issue mutations")
        }
    }

    fn status(id: ReplicaId, epoch: Epoch, role: Role) -> ReplicaStatusInfo {
        ReplicaStatusInfo {
            instance_id: ReplicaInstanceId::new(format!("pod-{id}")),
            role,
            epoch,
            current_progress: id * 10,
            catch_up_capability: Some(id * 10),
            committed_lsn: id * 10,
            healthy: true,
            write_status: if role == Role::Primary {
                AccessStatus::Granted
            } else {
                AccessStatus::NotPrimary
            },
            configuration: None,
            election_configuration: None,
            deactivation_info: None,
            active_replica_connections: Vec::new(),
            build_observation: None,
            agent: ReplicaAgentStatus {
                protocol_version: CORRELATED_CONTROL_PROTOCOL_VERSION,
                lifecycle_peer_protocol_version: REPLICA_LIFECYCLE_PEER_PROTOCOL_VERSION,
                generation: AgentGeneration::from_string(format!("generation-{id}")),
                control_version: AgentControlVersion::default(),
                current_action: None,
                retained_terminal_actions: Vec::new(),
                local_faults: Vec::new(),
            },
        }
    }

    fn snapshot() -> StablePartitionSnapshot {
        StablePartitionSnapshot {
            epoch: Epoch::new(1, 4),
            primary_id: 1,
            members: vec![
                StableReplicaSnapshot {
                    id: 1,
                    instance_id: ReplicaInstanceId::new("pod-1"),
                    role: Role::Primary,
                    election_metadata: None,
                },
                StableReplicaSnapshot {
                    id: 2,
                    instance_id: ReplicaInstanceId::new("pod-2"),
                    role: Role::ActiveSecondary,
                    election_metadata: None,
                },
            ],
            write_quorum: 2,
        }
    }

    fn handle(
        id: ReplicaId,
        epoch: Epoch,
        role: Role,
        calls: Arc<AtomicUsize>,
    ) -> Box<dyn ReplicaHandle> {
        Box::new(RecoveryHandle {
            id,
            instance_id: ReplicaInstanceId::new(format!("pod-{id}")),
            status: status(id, epoch, role),
            status_calls: calls,
        })
    }

    #[tokio::test]
    async fn recovery_is_status_only_and_round_trips_stable_snapshot() {
        let calls = Arc::new(AtomicUsize::new(0));
        let epoch = Epoch::new(1, 4);
        let driver = PartitionDriver::recover(
            snapshot(),
            vec![
                handle(1, epoch, Role::Primary, calls.clone()),
                handle(2, epoch, Role::ActiveSecondary, calls.clone()),
            ],
        )
        .await
        .unwrap();

        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(driver.primary_id(), Some(1));
        assert_eq!(driver.epoch(), epoch);
        assert_eq!(driver.stable_snapshot().unwrap(), snapshot());
    }

    #[tokio::test]
    async fn recovery_rejects_runtime_epoch_drift() {
        let calls = Arc::new(AtomicUsize::new(0));
        let error = PartitionDriver::recover(
            snapshot(),
            vec![
                handle(1, Epoch::new(1, 3), Role::Primary, calls.clone()),
                handle(2, Epoch::new(1, 4), Role::ActiveSecondary, calls.clone()),
            ],
        )
        .await
        .err()
        .unwrap();
        assert!(matches!(
            error,
            KubericError::Recovery(RecoveryError::EpochMismatch { id: 1, .. })
        ));
    }
}
