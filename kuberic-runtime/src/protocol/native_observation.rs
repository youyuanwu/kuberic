use super::*;

/// Proxy lifecycle state, distinct from native role, copy completion and access.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum NativeReplicaState {
    StandBy,
    InBuild,
    Ready,
}

/// Lifecycle-produced history. A provider checkpoint alone cannot produce this pair.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NativeDeactivationInfo {
    pub primary_fence: Epoch,
    pub catch_up_lsn: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NativeLifecycleFacts {
    pub operation_id: OperationId,
    pub revision: u64,
    pub scope: ReplicaScope,
    pub state: NativeReplicaState,
    pub native_role: ReplicaRole,
    pub application_role: Option<ReplicaRole>,
    pub applied_primary_fence: Option<Epoch>,
    pub deactivation: Option<NativeDeactivationInfo>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum NativeInspectionEpoch {
    StandBy,
    AlreadyApplied,
    Applied,
    Failed(String),
}

/// Immutable result of one journal-owned GetLSN program, not a moving sample.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NativeStatusObservation {
    pub input: NativeLifecycleFacts,
    pub epoch: NativeInspectionEpoch,
    pub progress: ProgressQueryResult,
    pub capability: ProgressQueryResult,
}

impl ReplicaAgentSnapshot {
    /// Full same-live normal demotion, including its final native GetStatus.
    pub fn normal_handoff_result(&self) -> Option<&ReplicaGoalObservation> {
        let facts = self.native_lifecycle.as_ref()?;
        let result = self.goals.iter().find(|result| {
            result.goal.operation_id == facts.operation_id && result.goal.revision == facts.revision
        })?;
        let goal = &result.goal;
        let previous = goal.scope.configuration.previous.as_ref()?;
        let current = &goal.scope.configuration.current;
        let ReplicaGoalKind::Reconfigure {
            role: ReplicaRole::ActiveSecondary,
            catch_up: Some(mode),
        } = goal.desired
        else {
            return None;
        };
        (self.format == ORDINARY_ACTION_FORMAT
            && goal.validate().is_ok()
            && !self.withdrawn
            && self.read_status != AccessStatus::Granted
            && self.write_status != AccessStatus::Granted
            && goal.scope.history == self.history
            && goal.scope.storage == self.storage
            && goal.scope.process_session_id == self.process_session_id
            && goal.scope.creation_id == self.creation_id
            && goal.scope == facts.scope
            && result.status == ReplicaGoalStatus::Achieved
            && result.host_settled
            && result.selected_call.is_none()
            && result.receipts == 7
            && match mode {
                PublicCatchUpMode::All => {
                    result.completed_handoff.as_deref() == Some(&SourceHandoffReceipt::ORDERED)
                }
                PublicCatchUpMode::WriteQuorum => result.completed_handoff.is_none(),
            }
            && previous.primary_id == self.storage.replica.replica_id
            && current.primary_id != previous.primary_id
            && current.epoch == goal.scope.epoch
            && current.epoch > previous.epoch
            && current.epoch.data_loss_number == previous.epoch.data_loss_number
            && current.write_quorum == previous.write_quorum
            && current.members.len() == previous.members.len()
            && previous.members.iter().all(|member| {
                current
                    .members
                    .iter()
                    .any(|next| next.identity == member.identity)
            })
            && goal.scope.configuration.replicas.iter().any(|peer| {
                peer.identity.replica_id == current.primary_id
                    && peer.must_catchup
                    && peer.is_up
                    && peer.is_ready
            })
            && facts.state == NativeReplicaState::Ready
            && facts.native_role == ReplicaRole::ActiveSecondary
            && facts.application_role == Some(ReplicaRole::ActiveSecondary)
            && facts.applied_primary_fence == Some(goal.scope.epoch)
            && facts.deactivation.as_ref().is_some_and(|boundary| {
                boundary.primary_fence == goal.scope.epoch && boundary.catch_up_lsn >= 0
            })
            && self.publication.as_ref().is_some_and(|publication| {
                publication.stage == ReplicaPublicationStage::AchievedScope
                    && publication.role == ReplicaRole::ActiveSecondary
                    && publication.scope == goal.scope
                    && publication.generation == goal.revision
            }))
        .then_some(result)
    }
}
