//! Durable replica-agent state.

use kuberic_protocol::types::{
    AccessStatus, ConfigurationDescriptor, EffectivePolicy, Epoch, InitializationId, OperationId,
    PodUid, PvcUid, ReplicaIdentity, ReplicaRole, ResourceUid,
};
use kuberic_runtime_internal::effects::{RuntimeEffect, RuntimeEffectResult};
use serde::{Deserialize, Serialize};

pub const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StorageIdentity {
    pub schema_version: u32,
    pub resource_uid: ResourceUid,
    pub pod_uid: PodUid,
    pub pvc_uid: PvcUid,
    pub initialization_id: InitializationId,
    pub local_identity: ReplicaIdentity,
    pub effective_policy: EffectivePolicy,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeactivationState {
    pub epoch: Epoch,
    pub deactivated_lsn: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum EffectStage {
    IntentCommitted,
    EffectApplied,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingEffect {
    pub effect: RuntimeEffect,
    pub stage: EffectStage,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RetainedResult {
    pub operation_id: OperationId,
    pub effect: RuntimeEffect,
    pub result: RuntimeEffectResult,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentState {
    pub identity: StorageIdentity,
    pub highest_epoch: Epoch,
    pub previous_configuration: Option<ConfigurationDescriptor>,
    pub current_configuration: Option<ConfigurationDescriptor>,
    pub role: ReplicaRole,
    pub write_status: AccessStatus,
    pub deactivation: Option<DeactivationState>,
    pub reconfiguration_data: Option<String>,
    pub pending_effect: Option<PendingEffect>,
    pub retained_result: Option<RetainedResult>,
}

impl AgentState {
    pub fn new(identity: StorageIdentity) -> Self {
        Self {
            identity,
            highest_epoch: Epoch::default(),
            previous_configuration: None,
            current_configuration: None,
            role: ReplicaRole::None,
            write_status: AccessStatus::NotPrimary,
            deactivation: None,
            reconfiguration_data: None,
            pending_effect: None,
            retained_result: None,
        }
    }
}
