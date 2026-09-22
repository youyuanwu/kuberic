//! Normalized Kubernetes, storage, routing, and replica-agent evidence.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::types::{
    AcceptedStatus, AccessStatus, ConfigurationDescriptor, Epoch, PodUid, ProcessSessionId, PvcUid,
    ReplicaId, ReplicaIdentity, ReplicaInstanceId, ReplicaRole, ResourceUid,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DesiredState {
    pub generation: u64,
    pub replicas: u32,
    pub image: String,
    pub failover_delay_seconds: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KubernetesReplicaObservation {
    pub replica_id: ReplicaId,
    pub pod_name: String,
    pub pod_uid: Option<PodUid>,
    pub pvc_name: String,
    pub pvc_uid: Option<PvcUid>,
    pub pod_ready: bool,
}

impl KubernetesReplicaObservation {
    pub fn has_exact_scaffolding(&self) -> bool {
        self.pod_uid.is_some() && self.pvc_uid.is_some()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UninitializedAgentObservation {
    pub protocol_version: u32,
    pub resource_uid: ResourceUid,
    pub replica_id: ReplicaId,
    pub pod_uid: PodUid,
    pub pvc_uid: PvcUid,
    pub process_session_id: ProcessSessionId,
    pub report_sequence: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentReport {
    pub protocol_version: u32,
    pub resource_uid: ResourceUid,
    pub identity: ReplicaIdentity,
    pub process_session_id: ProcessSessionId,
    pub report_sequence: u64,
    pub role: ReplicaRole,
    pub write_status: AccessStatus,
    pub healthy: bool,
    pub epoch: Epoch,
    pub previous_configuration: Option<ConfigurationDescriptor>,
    pub current_configuration: Option<ConfigurationDescriptor>,
    pub current_progress: i64,
    pub committed_lsn: i64,
    pub catch_up_capability: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
/// Lookup key that preserves multiple incarnations of one logical replica.
pub struct ReplicaObservationKey {
    pub replica_id: ReplicaId,
    pub instance_id: ReplicaInstanceId,
}

impl ReplicaObservationKey {
    pub fn new(replica_id: ReplicaId, instance_id: ReplicaInstanceId) -> Self {
        Self {
            replica_id,
            instance_id,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "kind")]
pub enum AgentObservation {
    Absent,
    Unreachable { message: String },
    Invalid { message: String },
    Uninitialized(UninitializedAgentObservation),
    Report(Box<AgentReport>),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReplicaObservation {
    pub kubernetes: Option<KubernetesReplicaObservation>,
    pub agent: AgentObservation,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ObservationFailure {
    pub source: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReportWatermark {
    pub process_session_id: ProcessSessionId,
    pub report_sequence: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct RoutingObservation {
    pub write_target: Option<ReplicaIdentity>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
/// Immutable input consumed by one evaluator invocation.
pub struct ObservationSnapshot {
    pub resource_uid: ResourceUid,
    pub resource_version: String,
    pub desired: DesiredState,
    pub status: AcceptedStatus,
    pub replicas: BTreeMap<ReplicaObservationKey, ReplicaObservation>,
    pub previous_report_watermarks: BTreeMap<ReplicaObservationKey, ReportWatermark>,
    pub durable_storage_evidence: bool,
    pub routing: RoutingObservation,
    pub observation_failures: Vec<ObservationFailure>,
    pub now_unix_seconds: i64,
}

impl ObservationSnapshot {
    pub fn intended_replica_ids(&self) -> Vec<ReplicaId> {
        (1..=self.desired.replicas)
            .map(|value| ReplicaId::new(i64::from(value)))
            .collect()
    }

    pub fn has_complete_scaffolding(&self) -> bool {
        self.intended_replica_ids().into_iter().all(|replica_id| {
            self.scaffolding_for(replica_id)
                .is_some_and(KubernetesReplicaObservation::has_exact_scaffolding)
        })
    }

    pub fn scaffolding_for(&self, replica_id: ReplicaId) -> Option<&KubernetesReplicaObservation> {
        let mut matching = self
            .replicas
            .iter()
            .filter(|(key, _)| key.replica_id == replica_id)
            .filter_map(|(_, observation)| observation.kubernetes.as_ref());
        let first = matching.next()?;
        matching.next().is_none().then_some(first)
    }

    pub fn observation_for_identity(
        &self,
        identity: &ReplicaIdentity,
    ) -> Option<&ReplicaObservation> {
        self.replicas.get(&ReplicaObservationKey::new(
            identity.replica_id,
            identity.instance_id.clone(),
        ))
    }
}
