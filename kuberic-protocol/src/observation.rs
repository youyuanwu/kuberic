use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::types::{
    AcceptedStatus, AccessStatus, ConfigurationDescriptor, Epoch, PodUid, ProcessSessionId, PvcUid,
    ReplicaId, ReplicaIdentity, ReplicaRole, ResourceUid,
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
    pub epoch: Epoch,
    pub previous_configuration: Option<ConfigurationDescriptor>,
    pub current_configuration: Option<ConfigurationDescriptor>,
    pub current_progress: i64,
    pub committed_lsn: i64,
    pub catch_up_capability: Option<i64>,
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
pub struct ObservationSnapshot {
    pub resource_uid: ResourceUid,
    pub resource_version: String,
    pub desired: DesiredState,
    pub status: AcceptedStatus,
    pub replicas: BTreeMap<ReplicaId, ReplicaObservation>,
    pub previous_report_watermarks: BTreeMap<ReplicaId, ReportWatermark>,
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
            self.replicas
                .get(&replica_id)
                .and_then(|observation| observation.kubernetes.as_ref())
                .is_some_and(KubernetesReplicaObservation::has_exact_scaffolding)
        })
    }
}
