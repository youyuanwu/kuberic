use super::{ReplicaRef, ResourceRef};
use crate::crd::KubericSet;
use kuberic_runtime::protocol::public_operations::{PublicOperationProgram, PublicV1CallKind};
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
            Self::Operation { stage, .. } | Self::SwitchoverHandoff { stage, .. } => *stage,
            Self::SwitchoverPeerEpoch { .. } => BarrierStage::Intent,
            Self::SwitchoverPeerConfiguration { .. } => BarrierStage::Completion,
        }
    }
    pub(super) fn participant(&self) -> Option<&ReplicaRef> {
        match self {
            Self::Operation { participant, .. } => participant.as_ref(),
            Self::SwitchoverPeerEpoch { peer, .. }
            | Self::SwitchoverPeerConfiguration { peer, .. } => Some(peer),
            Self::SwitchoverHandoff { source, .. } => Some(source),
        }
    }
    pub(super) fn request(&self) -> Option<&SwitchoverRequest> {
        match self {
            Self::Operation { .. } => None,
            Self::SwitchoverPeerEpoch { request, .. }
            | Self::SwitchoverPeerConfiguration { request, .. }
            | Self::SwitchoverHandoff { request, .. } => Some(request),
        }
    }
    pub(super) fn matches(&self, object: &serde_json::Value) -> bool {
        let Ok(set) = serde_json::from_value::<KubericSet>(object.clone()) else {
            return false;
        };
        let Some(status) = set.status else {
            return false;
        };
        let Some(operation) = status.public_v1_operation else {
            return false;
        };
        let Ok(command) = operation.command() else {
            return false;
        };
        if self.participant().is_some_and(|reference| {
            !reference.matches_command(&command.target, &command.intent.process_session_id)
        }) {
            return false;
        }
        if self.request().is_some_and(|request| {
            status
                .public_v1_topology
                .as_ref()
                .and_then(|topology| topology.pending_switchover.as_ref())
                .is_none_or(|pending| pending.request_id != request.id)
        }) {
            return false;
        }
        match self {
            Self::Operation { kind, .. } => kind.as_ref().is_none_or(|kind| {
                status
                    .public_v1_transition
                    .as_ref()
                    .is_some_and(|transition| transition.call_kind == *kind)
            }),
            Self::SwitchoverPeerEpoch { .. } => matches!(
                command.intent.program,
                Some(PublicOperationProgram::Epoch { .. })
            ),
            Self::SwitchoverPeerConfiguration { .. } => matches!(
                command.intent.program,
                Some(PublicOperationProgram::Configuration(_))
            ),
            Self::SwitchoverHandoff { .. } => matches!(
                command.intent.program,
                Some(PublicOperationProgram::Swap { .. })
            ),
        }
    }

    pub(super) fn matches_goal(&self, entry: &crate::crd::RetainedReplicaGoal) -> bool {
        use kuberic_runtime::protocol::public_operations::ReplicaGoalKind;

        let Self::Operation {
            stage,
            kind,
            participant,
        } = self
        else {
            return false;
        };
        if (*stage == BarrierStage::Completion) != entry.outcome.is_some()
            || participant.as_ref().is_some_and(|reference| {
                !reference.matches_command(
                    &entry.goal.scope.storage.replica,
                    &entry.goal.scope.process_session_id,
                )
            })
        {
            return false;
        }
        kind.is_none_or(|kind| {
            matches!(
                (&entry.goal.desired, kind),
                (ReplicaGoalKind::Build { .. }, PublicV1CallKind::Build)
                    | (ReplicaGoalKind::Remove { .. }, PublicV1CallKind::Remove)
                    | (ReplicaGoalKind::Close, PublicV1CallKind::Close)
                    | (ReplicaGoalKind::Abort, PublicV1CallKind::Abort)
            )
        })
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
}
impl OperationEvidence {
    pub(super) fn new(set: KubericSet) -> Self {
        Self { set }
    }
    pub fn set(&self) -> &KubericSet {
        &self.set
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
