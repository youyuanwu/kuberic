use super::{ReplicaRef, ResourceRef};
use crate::crd::{KubericSet, RetainedReplicaGoal};
use kuberic_runtime::protocol::public_operations::{PublicV1CallKind, ReplicaGoal};
use kuberic_runtime::protocol::types::ReplicaRole;
use std::{
    collections::BTreeSet,
    sync::{Arc, Mutex},
};

/// The persisted point at which a controller reply is held.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BarrierStage {
    Intent,
    Completion,
}

/// A semantic operation selector. No operation-id encoding is part of this API.
#[derive(Clone, Debug)]
pub enum Barrier {
    Operation {
        stage: BarrierStage,
        kind: Option<PublicV1CallKind>,
        participant: Option<ReplicaRef>,
    },
    /// Selects a whole Prepare/Reconfigure goal, not an inner role callback.
    Role {
        stage: BarrierStage,
        role: ReplicaRole,
        participant: Option<ReplicaRef>,
    },
    SwitchoverPeerEpoch {
        request: SwitchoverRequest,
        peer: ReplicaRef,
    },
    SwitchoverPeerConfiguration {
        request: SwitchoverRequest,
        peer: ReplicaRef,
    },
    SwitchoverHandoff {
        request: SwitchoverRequest,
        source: ReplicaRef,
        stage: BarrierStage,
    },
}
impl Barrier {
    pub fn intent(kind: PublicV1CallKind) -> Self {
        Self::Operation {
            stage: BarrierStage::Intent,
            kind: Some(kind),
            participant: None,
        }
    }
    pub fn completion(kind: PublicV1CallKind) -> Self {
        Self::Operation {
            stage: BarrierStage::Completion,
            kind: Some(kind),
            participant: None,
        }
    }
    pub(super) fn stage(&self) -> BarrierStage {
        match self {
            Self::Operation { stage, .. }
            | Self::Role { stage, .. }
            | Self::SwitchoverHandoff { stage, .. } => *stage,
            Self::SwitchoverPeerEpoch { .. } => BarrierStage::Intent,
            Self::SwitchoverPeerConfiguration { .. } => BarrierStage::Completion,
        }
    }
    pub(super) fn participant(&self) -> Option<&ReplicaRef> {
        match self {
            Self::Operation { participant, .. } | Self::Role { participant, .. } => {
                participant.as_ref()
            }
            Self::SwitchoverPeerEpoch { peer, .. }
            | Self::SwitchoverPeerConfiguration { peer, .. } => Some(peer),
            Self::SwitchoverHandoff { source, .. } => Some(source),
        }
    }
    pub(super) fn request(&self) -> Option<&SwitchoverRequest> {
        match self {
            Self::Operation { .. } | Self::Role { .. } => None,
            Self::SwitchoverPeerEpoch { request, .. }
            | Self::SwitchoverPeerConfiguration { request, .. }
            | Self::SwitchoverHandoff { request, .. } => Some(request),
        }
    }
    pub(super) fn matches_goal(&self, entry: &RetainedReplicaGoal, set: &KubericSet) -> bool {
        use kuberic_runtime::protocol::public_operations::ReplicaGoalKind;

        if entry.validate().is_err()
            || set.metadata.uid.as_deref() != Some(entry.goal.scope.history.resource_uid.as_str())
            || (self.stage() == BarrierStage::Completion) != entry.outcome.is_some()
            || self.participant().is_some_and(|reference| {
                !reference.matches_command(
                    &entry.goal.scope.storage.replica,
                    &entry.goal.scope.process_session_id,
                ) || reference.storage_incarnation() != &entry.goal.scope.storage
                    || reference.replicator_creation_id() != &entry.goal.scope.creation_id
            })
        {
            return false;
        }
        if let Some(request) = self.request() {
            let pending = set
                .status
                .as_ref()
                .and_then(|status| status.public_v1_topology.as_ref())
                .and_then(|topology| topology.pending_switchover.as_ref());
            let target = &request.target;
            let scope = &entry.goal.scope;
            let target_bound = (scope.storage == *target.storage_incarnation()
                && scope.process_session_id == *target.process_session_id())
                || scope.configuration.replicas.iter().any(|replica| {
                    replica.identity == *target.identity()
                        && replica.process_session_id == *target.process_session_id()
                        && replica.storage_incarnation == *target.storage_incarnation()
                });
            if pending.is_none_or(|pending| {
                pending.request_id != request.id
                    || i64::from(pending.target_replica_id) != target.replica_id()
            }) || !target_bound
            {
                return false;
            }
        }
        let role = match &entry.goal.desired {
            ReplicaGoalKind::Prepare { role } | ReplicaGoalKind::Reconfigure { role, .. } => {
                Some(*role)
            }
            _ => None,
        };
        match self {
            Self::Operation { kind, .. } => kind.is_none_or(|kind| {
                (kind == PublicV1CallKind::Role && role.is_some())
                    || matches!(
                        (&entry.goal.desired, kind),
                        (ReplicaGoalKind::Build { .. }, PublicV1CallKind::Build)
                            | (ReplicaGoalKind::Remove { .. }, PublicV1CallKind::Remove)
                            | (ReplicaGoalKind::Close, PublicV1CallKind::Close)
                            | (ReplicaGoalKind::Abort, PublicV1CallKind::Abort)
                    )
            }),
            Self::Role { role: selected, .. } => role == Some(*selected),
            Self::SwitchoverPeerEpoch { .. } | Self::SwitchoverPeerConfiguration { .. } => {
                role == Some(ReplicaRole::ActiveSecondary)
            }
            Self::SwitchoverHandoff { request, .. } => {
                role == Some(ReplicaRole::ActiveSecondary)
                    && entry.goal.scope.configuration.current.primary_id
                        == request.target.identity().replica_id
            }
        }
    }
}

/// Identity returned by a spec-only switchover submission.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SwitchoverRequest {
    pub(super) cluster: uuid::Uuid,
    pub(super) id: String,
    pub(super) target: ReplicaRef,
}
impl SwitchoverRequest {
    pub fn id(&self) -> &str {
        &self.id
    }
    pub fn target(&self) -> &ReplicaRef {
        &self.target
    }
}

/// Detached evidence of the exact persisted controller status.
#[derive(Clone, Debug)]
pub struct OperationEvidence {
    set: KubericSet,
    goal: Option<ReplicaGoal>,
}
impl OperationEvidence {
    pub(super) fn new(set: KubericSet, goal: Option<ReplicaGoal>) -> Self {
        Self { set, goal }
    }
    pub fn set(&self) -> &KubericSet {
        &self.set
    }
    /// The exact changed goal selected by the status barrier. A completion
    /// conflict retains its prior, not-yet-accepted controller outcome.
    pub fn goal(&self) -> Option<&RetainedReplicaGoal> {
        let goal = self.goal.as_ref()?;
        self.set
            .status
            .as_ref()?
            .replica_goals
            .iter()
            .find(|entry| entry.goal == *goal)
    }
}

/// Owns a held controller reply without borrowing the cluster.
pub struct Pause {
    pub(super) cluster: uuid::Uuid,
    pub(super) owner: uuid::Uuid,
    pub(super) barrier: Option<super::kube::StatusBarrier>,
    pub(super) evidence: OperationEvidence,
}
impl Pause {
    pub fn evidence(&self) -> &OperationEvidence {
        &self.evidence
    }
}

/// One metadata-only conflict on the next matching completion status write.
pub struct CompletionConflict {
    pub(super) barrier: super::kube::StatusBarrier,
    pub(super) deadline: tokio::time::Instant,
}
impl CompletionConflict {
    pub async fn entered(&mut self) -> super::Result<OperationEvidence> {
        let value = tokio::time::timeout_at(self.deadline, self.barrier.entered())
            .await
            .map_err(|source| {
                super::TestError::with_source(
                    super::ErrorKind::Deadline,
                    "completion conflict deadline",
                    source,
                )
            })??;
        Ok(OperationEvidence::new(
            serde_json::from_value(value).map_err(anyhow::Error::from)?,
            self.barrier.goal(),
        ))
    }
    pub fn release(self) {
        self.barrier.release();
    }
}

/// Owns a hold on one exact deleting resource incarnation.
pub struct DeletionHold {
    pub(super) resource: ResourceRef,
    pub(super) held: Arc<Mutex<BTreeSet<String>>>,
}
impl Drop for DeletionHold {
    fn drop(&mut self) {
        self.held.lock().unwrap().remove(self.resource.uid());
    }
}
