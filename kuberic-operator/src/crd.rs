use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// KubericSet is the primary CRD for managing a stateful replica set.
#[derive(CustomResource, Serialize, Deserialize, Debug, PartialEq, Clone, JsonSchema)]
#[kube(
    group = "kuberic.io",
    version = "v1",
    kind = "KubericSet",
    plural = "kubericsets",
    shortname = "kls",
    derive = "PartialEq",
    namespaced,
    status = "KubericSetStatus"
)]
#[serde(rename_all = "camelCase")]
pub struct KubericSetSpec {
    /// Total number of replicas (1 primary + N-1 secondaries).
    #[serde(default = "default_replicas")]
    pub replicas: i32,

    /// Minimum replica set size. Operator won't reduce below this.
    #[serde(default = "default_min_replicas")]
    pub min_replicas: i32,

    /// Container image for the application pods.
    pub image: String,

    /// Seconds to wait before triggering failover after primary failure.
    #[serde(default)]
    pub failover_delay: i32,

    /// Max seconds for graceful primary demotion during switchover.
    #[serde(default = "default_switchover_delay")]
    pub switchover_delay: i32,

    /// Port for the application container.
    #[serde(default = "default_port")]
    pub port: i32,

    /// Port for the gRPC control server (operator → pod).
    #[serde(default = "default_control_port")]
    pub control_port: i32,

    /// Port for the gRPC data server (pod ↔ pod replication).
    #[serde(default = "default_data_port")]
    pub data_port: i32,

    /// Storage size per pod PVC (e.g. "256Mi").
    #[serde(default = "default_storage")]
    pub storage: String,

    /// PVC retention policy on CR deletion: Delete (default) or Retain.
    #[serde(default)]
    pub pvc_retention_policy: PvcRetentionPolicy,
}

/// Status of the KubericSet.
#[derive(Serialize, Deserialize, Debug, PartialEq, Clone, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct KubericSetStatus {
    /// Current epoch.
    #[serde(default)]
    pub epoch: EpochStatus,

    /// Pod name of the current primary.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_primary: Option<String>,

    /// Pod name that should be primary. "pending" during failover.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_primary: Option<String>,

    /// Lifecycle phase.
    #[serde(default)]
    pub phase: Phase,

    /// Current reconfiguration phase (if any).
    #[serde(default)]
    pub reconfiguration_phase: ReconfigurationPhase,

    /// Active replica count.
    #[serde(default)]
    pub ready_replicas: i32,

    /// Total replica count (including non-ready).
    #[serde(default)]
    pub replicas: i32,

    /// Per-replica status.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub members: Vec<MemberStatus>,

    /// When the primary started failing (for failover delay).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub primary_failing_since: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct EpochStatus {
    pub data_loss_number: i64,
    pub configuration_number: i64,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct MemberStatus {
    /// Pod name.
    pub name: String,
    /// Replica ID.
    pub id: i64,
    /// Role: primary, active-secondary, idle-secondary, none.
    pub role: String,
    /// Current progress LSN.
    pub current_progress: i64,
    /// Whether the pod is healthy.
    pub healthy: bool,
    /// gRPC control address.
    pub control_address: String,
    /// gRPC data address.
    pub data_address: String,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone, JsonSchema, Default)]
pub enum Phase {
    #[default]
    Pending,
    Creating,
    Healthy,
    FailingOver,
    Switchover,
    Deleting,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone, JsonSchema, Default)]
pub enum ReconfigurationPhase {
    #[default]
    None,
    Phase0Demote,
    Phase1GetLSN,
    Phase2Catchup,
    Phase3Deactivate,
    Phase4Activate,
}

fn default_replicas() -> i32 {
    3
}

fn default_min_replicas() -> i32 {
    2
}

fn default_switchover_delay() -> i32 {
    3600
}

fn default_port() -> i32 {
    8080
}

fn default_control_port() -> i32 {
    9090
}

fn default_data_port() -> i32 {
    9091
}

fn default_storage() -> String {
    "256Mi".to_string()
}

/// PVC retention policy when the KubericSet CR is deleted.
#[derive(Serialize, Deserialize, Debug, PartialEq, Clone, JsonSchema, Default)]
pub enum PvcRetentionPolicy {
    /// Delete PVCs when the KubericSet is deleted.
    #[default]
    Delete,
    /// Retain PVCs for manual recovery.
    Retain,
}
