//! Fenced protocol commands and declarative Kubernetes changes.

use serde::{Deserialize, Serialize};

use crate::types::{
    AgentGeneration, ConfigurationDescriptor, EffectivePolicy, Epoch, InitializationId,
    OperationId, PodUid, PvcUid, ReplicaId, ReplicaInstanceId, ResourceUid,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum EffectClass {
    ConvergentEnsure,
    ReconfigurationAction,
    Unsupported,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeAgentStore {
    pub initialization_id: InitializationId,
    pub resource_uid: ResourceUid,
    pub local_replica_id: ReplicaId,
    pub expected_instance_id: ReplicaInstanceId,
    pub expected_pod_uid: PodUid,
    pub expected_pvc_uid: PvcUid,
    pub assigned_agent_generation: AgentGeneration,
    pub effective_policy: EffectivePolicy,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EnsureConfiguration {
    pub operation_id: OperationId,
    pub previous_configuration: Option<ConfigurationDescriptor>,
    pub current_configuration: ConfigurationDescriptor,
    pub previous_epoch: Option<Epoch>,
    pub current_epoch: Epoch,
    pub effective_policy: EffectivePolicy,
    pub local_replica_id: ReplicaId,
    pub expected_instance_id: ReplicaInstanceId,
    pub expected_agent_generation: AgentGeneration,
    pub transition_kind: crate::types::TransitionKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "kind")]
/// One fenced, idempotent authority command issued after a full observation.
pub enum ProtocolCommand {
    InitializeAgentStore(InitializeAgentStore),
    EnsureConfiguration(Box<EnsureConfiguration>),
}

impl ProtocolCommand {
    pub fn effect_class(&self) -> EffectClass {
        match self {
            Self::InitializeAgentStore(_) => EffectClass::ConvergentEnsure,
            Self::EnsureConfiguration(_) => EffectClass::ReconfigurationAction,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "kind")]
/// Idempotent Kubernetes or status convergence performed by the controller.
pub enum KubernetesChange {
    EnsureReplicaScaffolding {
        replica_ids: Vec<ReplicaId>,
    },
    PersistStatus {
        status: Box<crate::types::AcceptedStatus>,
    },
    RemoveWriteRouting,
    PublishWriteRouting {
        primary: crate::types::ReplicaIdentity,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SafetyChange {
    RemoveWriteRouting,
}
