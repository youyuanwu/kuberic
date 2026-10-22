//! Durable Public V1 replica-agent state.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::host::public_build_store::PublicV1BuildRecord;
use crate::protocol::public_operations::{
    DataLossAuthorization, PublicDataLossRecoveryStatus, PublicOperationIntent, PublicV1Identity,
    PublicV1LifecycleBinding, PublicV1SessionRenewalStatus,
};
use crate::protocol::types::{
    EffectivePolicy, FaultType, HistoryContext, HistoryInitializationId, InitializationId,
    LoadMetric, OperationId, PodUid, PvcUid, ReplicaIdentity, ReplicaStorageIncarnationContext,
    ResourceUid,
};

pub(crate) const SCHEMA_VERSION: u32 = 6;
pub(crate) const PUBLIC_V1_PHASE6_SCHEMA_VERSION: u32 = 11;
pub(crate) const PUBLIC_V1_REPLACEMENT_SCHEMA_VERSION: u32 = 14;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StorageIdentity {
    pub(crate) schema_version: u32,
    pub(crate) resource_uid: ResourceUid,
    pub(crate) pod_uid: PodUid,
    pub(crate) pvc_uid: PvcUid,
    pub(crate) initialization_id: InitializationId,
    pub(crate) local_identity: ReplicaIdentity,
    pub(crate) effective_policy: EffectivePolicy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum PublicOperationStage {
    WaitingForContainment,
    Ready,
    Running,
    CallbackApplied,
    RetryPending,
    ContainmentPending,
    Completed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "kind", content = "detail")]
pub(crate) enum PublicOperationDisposition {
    Succeeded,
    Failed(String),
    Ambiguous(String),
    Cancelled,
    Attached(OperationId),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub(crate) enum PublicOperationContainment {
    #[default]
    NotRequired,
    Pending,
    Complete,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PublicOperationRecord {
    pub(crate) intent: PublicOperationIntent,
    pub(crate) attempt: u64,
    pub(crate) attempt_results: BTreeMap<u64, PublicOperationAttemptResult>,
    pub(crate) retry_failure: Option<PublicCallbackFailure>,
    #[serde(default)]
    pub(crate) accepted_history: Option<HistoryContext>,
    #[serde(default)]
    pub(crate) accepted_storage: Option<ReplicaStorageIncarnationContext>,
    pub(crate) stage: PublicOperationStage,
    pub(crate) disposition: Option<PublicOperationDisposition>,
    #[serde(default)]
    pub(crate) containment: PublicOperationContainment,
    #[serde(default)]
    pub(crate) superseded_by: Option<OperationId>,
    #[serde(default)]
    pub(crate) blockers: BTreeSet<OperationId>,
    #[serde(default)]
    pub(crate) lifecycle: PublicLifecycleRecord,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PublicOperationAttemptResult {
    pub(crate) stage: PublicOperationStage,
    pub(crate) disposition: PublicOperationDisposition,
    pub(crate) lifecycle: PublicLifecycleRecord,
    pub(crate) retry_failure: Option<PublicCallbackFailure>,
    pub(crate) containment: PublicOperationContainment,
    pub(crate) superseded_by: Option<OperationId>,
    pub(crate) blockers: BTreeSet<OperationId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub(crate) enum PublicCallbackFamily {
    ReplicatorRole,
    ApplicationRole,
    Epoch,
    Configuration,
    CatchUp,
}

impl PublicCallbackFamily {
    pub(crate) fn allows(self, instruction: PublicInstruction) -> bool {
        use PublicInstruction::*;
        match self {
            Self::ReplicatorRole => matches!(instruction, ReplicatorRole | ReplicatorPrimary),
            Self::ApplicationRole => matches!(instruction, ApplicationRole | ApplicationPrimary),
            Self::Epoch => matches!(instruction, Epoch | ProgramEpoch),
            Self::Configuration => matches!(
                instruction,
                Configuration
                    | StartingConfiguration
                    | RefreshedConfiguration
                    | CurrentConfiguration
                    | CatchUpConfiguration
                    | FirstCatchUp
                    | SecondCatchUp
                    | CatchUp
            ),
            Self::CatchUp => matches!(instruction, FirstCatchUp | SecondCatchUp | CatchUp),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PublicCallbackFailure {
    pub(crate) instruction: PublicInstruction,
    pub(crate) family: PublicCallbackFamily,
    pub(crate) error: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum PublicInstruction {
    ApplicationOpen,
    ReplicatorOpen,
    ReplicatorRole,
    ApplicationRole,
    ProgramEpoch,
    Configuration,
    StartingConfiguration,
    RefreshedConfiguration,
    FirstCatchUp,
    SecondCatchUp,
    Revoke,
    Progress,
    Build,
    Remove,
    ReplicatorClose,
    ApplicationClose,
    Abort,
    Cleanup,
    ReplicatorPrimary,
    Epoch,
    ApplicationPrimary,
    DataLoss,
    CurrentConfiguration,
    CatchUpConfiguration,
    CatchUp,
    HistoryAdmission,
    Access,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum PublicInstructionOutcome {
    Done,
    ApplicationRole(Option<String>),
    DataLoss(DataLossOutcome),
    Endpoint(String),
    Progress(i64),
    CloseFailure {
        child: PublicCloseChild,
        error: String,
    },
    RebuildRequired(Box<crate::protocol::public_operations::CanonicalReplicaRebuildRequired>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum PublicCloseChild {
    Replicator,
    Application,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum DataLossOutcome {
    NotInvoked,
    False,
    True,
    Error(String),
    Ambiguous,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub(crate) struct PublicLifecycleRecord {
    pub(crate) in_flight: Option<PublicInstruction>,
    pub(crate) outcomes: Vec<PublicInstructionOutcome>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct HistoryAdmissionBarrier {
    pub(crate) intent: PublicOperationIntent,
    pub(crate) outcome: DataLossOutcome,
    pub(crate) retired_by: Option<OperationId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PublicV1OperationState {
    pub(crate) identity: PublicV1Identity,
    #[serde(default)]
    pub(crate) binding: Option<PublicV1LifecycleBinding>,
    pub(crate) operations: BTreeMap<OperationId, PublicOperationRecord>,
    #[serde(default)]
    pub(crate) history_barriers: BTreeMap<OperationId, HistoryAdmissionBarrier>,
    #[serde(default)]
    pub(crate) history_initialization_id: Option<HistoryInitializationId>,
    #[serde(default)]
    pub(crate) history: Option<HistoryContext>,
    #[serde(default)]
    pub(crate) storage_incarnation: Option<ReplicaStorageIncarnationContext>,
    #[serde(default)]
    pub(crate) data_loss_authorization: Option<DataLossAuthorization>,
    #[serde(default)]
    pub(crate) data_loss_recovery: Option<PublicDataLossRecoveryStatus>,
    pub(crate) session_renewal: Option<PublicV1SessionRenewalStatus>,
    #[serde(default)]
    pub(crate) current_operation: Option<OperationId>,
    #[serde(default)]
    pub(crate) current_topology_operation: Option<OperationId>,
    #[serde(default)]
    pub(crate) active_builds: BTreeMap<
        crate::protocol::types::ReplicaId,
        crate::protocol::public_operations::PublicBuildInput,
    >,
    #[serde(default)]
    pub(crate) build_records: BTreeMap<OperationId, PublicV1BuildRecord>,
    #[serde(default)]
    pub(crate) absent_builds: BTreeSet<OperationId>,
    #[serde(default)]
    pub(crate) current_inbound_build_authority:
        Option<crate::protocol::public_operations::PublicInboundBuildAuthority>,
    #[serde(default)]
    pub(crate) inbound_build_generation: u64,
    #[serde(default)]
    pub(crate) terminal_inbound_build_receipts:
        BTreeMap<OperationId, crate::protocol::public_operations::PublicInboundBuildReceipt>,
    #[serde(default)]
    pub(crate) retired_storage_incarnations: Vec<ReplicaStorageIncarnationContext>,
    #[serde(default)]
    pub(crate) writes_revoked: bool,
    #[serde(default)]
    pub(crate) terminal: bool,
}

pub(crate) use crate::protocol::public_operations::PublicV1RemoteBuildState as PublicRemoteBuildState;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PublicRemoteBuildObservation {
    pub(crate) build: PublicV1BuildRecord,
    pub(crate) state: PublicRemoteBuildState,
}

impl PublicV1OperationState {
    pub(crate) fn build_was_promoted(
        &self,
        build: &crate::protocol::public_operations::PublicBuildInput,
    ) -> bool {
        use crate::host::public_build_store::PublicV1BuildRetirement;
        use crate::protocol::public_operations::PublicV1OperationResult;
        let Some(record) = self.build_records.get(&build.attempt) else {
            return false;
        };
        if record.result != Some(PublicV1OperationResult::Succeeded)
            || record.retirement != PublicV1BuildRetirement::Retain
        {
            return false;
        }
        self.operations.values().any(|operation| {
            if operation.intent.revision <= record.admission_revision
                || operation.intent.process_session_id != record.source_session
                || operation.accepted_history.as_ref() != Some(&record.history)
                || operation.accepted_storage.as_ref() != Some(&record.source_storage)
            {
                return false;
            }
            let Some(configuration) =
                crate::host::public_lifecycle::installed_configuration_from_record(operation)
            else {
                return false;
            };
            configuration.current.epoch >= build.epoch
                && configuration.current.members.iter().any(|member| {
                    member.identity == build.replica
                        && matches!(
                            member.role,
                            crate::protocol::types::ReplicaRole::Primary
                                | crate::protocol::types::ReplicaRole::ActiveSecondary
                        )
                })
                && configuration.replicas.iter().any(|replica| {
                    replica.identity == build.replica
                        && replica.storage_incarnation.as_ref() == Some(&build.target_storage)
                        && replica.process_session_id == build.process_session_id
                        && replica.replication_address == build.replication_address
                })
        })
    }

    pub(crate) fn remote_build_states(
        &self,
        session: &crate::protocol::types::ProcessSessionId,
    ) -> Vec<PublicRemoteBuildObservation> {
        use crate::host::public_build_store::PublicV1BuildRetirement;
        use crate::protocol::public_operations::{PublicOperationProgram, PublicV1OperationResult};
        self.build_records
            .values()
            .filter(|build| {
                &build.source_session == session
                    && self.history.as_ref() == Some(&build.history)
                    && self.storage_incarnation.as_ref() == Some(&build.source_storage)
            })
            .map(|build| {
                let operation = self.operations.get(&build.operation_id);
                let exact = operation.is_some_and(|operation| {
                    operation.intent.revision == build.admission_revision
                        && operation.intent.process_session_id == build.source_session
                        && operation.accepted_history.as_ref() == Some(&build.history)
                        && operation.accepted_storage.as_ref() == Some(&build.source_storage)
                        && matches!(operation.intent.program.as_ref(),
                            Some(PublicOperationProgram::Build(input))
                                if input.attempt == build.build_id
                                    && input.epoch == build.epoch
                                    && input.replica == build.target
                                    && input.target_storage == build.target_storage
                                    && input.process_session_id == build.target_session
                                    && input.replication_address == build.target_address)
                });
                let retiring = self.terminal
                    || self.absent_builds.contains(&build.build_id)
                    || operation.is_some_and(|operation| operation.superseded_by.is_some())
                    || self.operations.values().any(|operation| {
                        operation.intent.process_session_id == build.source_session
                            && operation.accepted_history.as_ref() == Some(&build.history)
                            && operation.accepted_storage.as_ref() == Some(&build.source_storage)
                            && match operation.intent.program.as_ref() {
                                Some(PublicOperationProgram::Remove(input)) => {
                                    input.attempt == build.build_id
                                        && input.epoch == build.epoch
                                        && input.history.as_ref() == Some(&build.history)
                                        && input.replica == build.target
                                        && input.target_storage == build.target_storage
                                        && input.process_session_id == build.target_session
                                        && input.replication_address == build.target_address
                                }
                                _ => false,
                            }
                    });
                let promoted = exact
                    && operation
                        .and_then(|operation| match &operation.intent.program {
                            Some(PublicOperationProgram::Build(input)) => Some(input),
                            _ => None,
                        })
                        .is_some_and(|input| self.build_was_promoted(input));
                let state = match build.retirement {
                    PublicV1BuildRetirement::Retired => PublicRemoteBuildState::Dropped,
                    PublicV1BuildRetirement::Required => PublicRemoteBuildState::InDrop,
                    // Configuration now owns this peer. Lazy Build-slot GC is
                    // not continued idle ownership or proof of native presence.
                    PublicV1BuildRetirement::Retain if promoted => PublicRemoteBuildState::Dropped,
                    PublicV1BuildRetirement::Retain if retiring => PublicRemoteBuildState::InDrop,
                    PublicV1BuildRetirement::Retain => {
                        if exact
                            && operation.is_some_and(|operation| {
                                operation.stage == PublicOperationStage::Completed
                                    && operation.disposition
                                        == Some(PublicOperationDisposition::Succeeded)
                                    && build.result == Some(PublicV1OperationResult::Succeeded)
                            })
                        {
                            PublicRemoteBuildState::Ready
                        } else if exact
                            && operation.is_some_and(|operation| {
                                operation.stage == PublicOperationStage::Running
                            })
                        {
                            PublicRemoteBuildState::InBuild
                        } else {
                            PublicRemoteBuildState::Accepted
                        }
                    }
                };
                PublicRemoteBuildObservation {
                    build: build.clone(),
                    state,
                }
            })
            .collect()
    }

    pub(crate) fn new_bound(binding: PublicV1LifecycleBinding) -> Self {
        Self::from_parts(binding.identity.clone(), Some(binding))
    }

    fn from_parts(identity: PublicV1Identity, binding: Option<PublicV1LifecycleBinding>) -> Self {
        Self {
            identity,
            binding,
            operations: BTreeMap::new(),
            history_barriers: BTreeMap::new(),
            history_initialization_id: None,
            history: None,
            storage_incarnation: None,
            data_loss_authorization: None,
            data_loss_recovery: None,
            session_renewal: None,
            current_operation: None,
            current_topology_operation: None,
            active_builds: BTreeMap::new(),
            build_records: BTreeMap::new(),
            absent_builds: BTreeSet::new(),
            current_inbound_build_authority: None,
            inbound_build_generation: 0,
            terminal_inbound_build_receipts: BTreeMap::new(),
            retired_storage_incarnations: Vec::new(),
            writes_revoked: false,
            terminal: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ApplicationStorageBinding {
    pub(crate) paths: BTreeMap<String, PathBuf>,
    pub(crate) initializing: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AgentState {
    pub(crate) identity: StorageIdentity,
    #[serde(default)]
    pub(crate) public_v1_operation_state: Option<PublicV1OperationState>,
    pub(crate) application_storage: Option<ApplicationStorageBinding>,
    #[serde(default)]
    pub(crate) load_metrics: Vec<LoadMetric>,
    #[serde(default)]
    pub(crate) reported_fault: Option<FaultType>,
}

impl AgentState {
    pub(crate) fn application_open_mode(&self) -> crate::application::OpenMode {
        if self
            .application_storage
            .as_ref()
            .is_some_and(|binding| binding.initializing)
        {
            crate::application::OpenMode::New
        } else {
            crate::application::OpenMode::Existing
        }
    }

    pub(crate) fn new(identity: StorageIdentity) -> Self {
        Self {
            identity,
            public_v1_operation_state: None,
            application_storage: None,
            load_metrics: Vec::new(),
            reported_fault: None,
        }
    }
}
