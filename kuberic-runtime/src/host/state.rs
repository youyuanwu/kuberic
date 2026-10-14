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
pub(crate) const PUBLIC_V1_REPLACEMENT_SCHEMA_VERSION: u32 = 12;

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

impl PublicV1OperationState {
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
