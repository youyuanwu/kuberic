use kube::CustomResource;
use kuberic_core::types::{
    Epoch, ReplicaInstanceId, Role, StablePartitionSnapshot, StableReplicaSnapshot,
};
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

    /// Last durably committed stable topology. This is the only authoritative
    /// recovery input after operator process state is lost.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stable_snapshot: Option<StablePartitionSnapshotStatus>,

    /// Compact durable checkpoint for the current or most recent operation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub operation: Option<DurableOperationStatus>,

    /// Kubernetes-style conditions describing durable operation state.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conditions: Vec<StatusCondition>,

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
    /// Kubernetes pod UID for this concrete replica incarnation.
    #[serde(default)]
    pub instance_id: String,
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

/// Schema-safe persisted form of the core stable partition snapshot.
#[derive(Serialize, Deserialize, Debug, PartialEq, Clone, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct StablePartitionSnapshotStatus {
    pub epoch: EpochStatus,
    pub primary_id: i64,
    pub members: Vec<StableReplicaSnapshotStatus>,
    pub write_quorum: u32,
}

/// Schema-safe persisted identity and stable role of one replica.
#[derive(Serialize, Deserialize, Debug, PartialEq, Clone, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct StableReplicaSnapshotStatus {
    pub id: i64,
    pub instance_id: String,
    pub role: StableReplicaRoleStatus,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone, Copy, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum StableReplicaRoleStatus {
    Primary,
    ActiveSecondary,
}

pub const DURABLE_OPERATION_VERSION: u32 = 1;

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DurableOperationStatus {
    pub operation_id: String,
    pub execution_id: String,
    pub version: u32,
    pub kind: DurableOperationKind,
    pub phase: DurableOperationPhase,
    pub previous_snapshot: StablePartitionSnapshotStatus,
    pub target_snapshot: StablePartitionSnapshotStatus,
    pub old_primary_id: i64,
    pub target_primary_id: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub add_mode: Option<DurableAddMode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_replica_id: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_instance_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_pod_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retired_instance_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frozen_lsn: Option<i64>,
    #[serde(default)]
    pub next_secondary_index: u32,
    pub phase_deadline_unix_seconds: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pending_action: Option<PendingActionStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone, Copy, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum DurableOperationKind {
    Switchover,
    AddReplica,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone, Copy, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum DurableAddMode {
    ScaleUp,
    Rebuild,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone, Copy, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum DurableOperationPhase {
    Revoke,
    CaptureLsn,
    PreCatchUp,
    DemoteOldPrimary,
    PromoteTarget,
    DistributeEpoch,
    UpdateCatchUpConfiguration,
    WaitForCatchUpQuorum,
    UpdateCurrentConfiguration,
    LabelTargetPrimary,
    LabelOldSecondary,
    Finalize,
    RestorePreviousConfiguration,
    CompensatePromoteOldPrimary,
    CompensateDistributeEpoch,
    CompensateCatchUpConfiguration,
    CompensateCurrentConfiguration,
    CompensateLabelOldPrimary,
    CompensateLabelTargetSecondary,
    CompensateFinalize,
    RetireOldReplica,
    OpenCandidate,
    UpdateCandidateEpoch,
    AssignCandidateIdle,
    BuildCandidate,
    AssignCandidateActive,
    AddCatchUpConfiguration,
    AddWaitForCatchUpQuorum,
    AddCurrentConfiguration,
    LabelCandidateSecondary,
    AddFinalize,
    CompensateRestoreConfiguration,
    CompensateRemoveCandidate,
    CompensateDemoteCandidate,
    CompensateCloseCandidate,
    CompensateDeleteCandidate,
    AddCompensateFinalize,
    Completed,
    Failed,
    Poisoned,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PendingActionStatus {
    pub action_id: String,
    pub sequence: u32,
    pub kind: DurableActionKind,
    pub target_id: i64,
    pub target_instance_id: String,
    pub expected_epoch: EpochStatus,
    pub desired_postcondition: DurablePostconditionStatus,
    #[serde(default)]
    pub attempts: u32,
    pub deadline_unix_seconds: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone, Copy, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum DurableActionKind {
    RevokeWrite,
    DemoteOldPrimary,
    PromoteTarget,
    UpdateSecondaryEpoch,
    UpdateCatchUpConfiguration,
    WaitForCatchUpQuorum,
    UpdateCurrentConfiguration,
    LabelTargetPrimary,
    LabelOldSecondary,
    RestorePreviousConfiguration,
    CompensatePromoteOldPrimary,
    CompensateUpdateSecondaryEpoch,
    CompensateCatchUpConfiguration,
    CompensateCurrentConfiguration,
    CompensateLabelOldPrimary,
    CompensateLabelTargetSecondary,
    RetireOldReplica,
    OpenCandidate,
    UpdateCandidateEpoch,
    AssignCandidateIdle,
    BuildCandidate,
    AssignCandidateActive,
    AddCatchUpConfiguration,
    AddWaitForCatchUpQuorum,
    AddCurrentConfiguration,
    LabelCandidateSecondary,
    CompensateRestoreConfiguration,
    CompensateRemoveCandidate,
    CompensateDemoteCandidate,
    CompensateCloseCandidate,
    CompensateDeleteCandidate,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DurablePostconditionStatus {
    pub kind: DurablePostconditionKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<StableReplicaRoleStatus>,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone, Copy, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum DurablePostconditionKind {
    WriteRevoked,
    Role,
    Epoch,
    CatchUpConfiguration,
    CatchUpQuorum,
    CurrentConfiguration,
    PodRoleLabel,
    Opened,
    BuildCompleted,
    ReplicaRemoved,
    Closed,
    PodDeleted,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct StatusCondition {
    #[serde(rename = "type")]
    pub type_: String,
    pub status: String,
    pub reason: String,
    pub message: String,
    pub last_transition_time: String,
}

impl TryFrom<&StablePartitionSnapshotStatus> for StablePartitionSnapshot {
    type Error = String;

    fn try_from(value: &StablePartitionSnapshotStatus) -> Result<Self, Self::Error> {
        let members = value
            .members
            .iter()
            .map(StableReplicaSnapshot::try_from)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            epoch: Epoch::new(
                value.epoch.data_loss_number,
                value.epoch.configuration_number,
            ),
            primary_id: value.primary_id,
            members,
            write_quorum: value.write_quorum,
        })
    }
}

impl TryFrom<&StableReplicaSnapshotStatus> for StableReplicaSnapshot {
    type Error = String;

    fn try_from(value: &StableReplicaSnapshotStatus) -> Result<Self, Self::Error> {
        if value.instance_id.is_empty() {
            return Err(format!(
                "stable snapshot replica {} has an empty incarnation",
                value.id
            ));
        }
        Ok(Self {
            id: value.id,
            instance_id: ReplicaInstanceId::new(value.instance_id.clone()),
            role: match value.role {
                StableReplicaRoleStatus::Primary => Role::Primary,
                StableReplicaRoleStatus::ActiveSecondary => Role::ActiveSecondary,
            },
        })
    }
}

impl TryFrom<&StablePartitionSnapshot> for StablePartitionSnapshotStatus {
    type Error = String;

    fn try_from(value: &StablePartitionSnapshot) -> Result<Self, Self::Error> {
        Ok(Self {
            epoch: EpochStatus {
                data_loss_number: value.epoch.data_loss_number,
                configuration_number: value.epoch.configuration_number,
            },
            primary_id: value.primary_id,
            members: value
                .members
                .iter()
                .map(StableReplicaSnapshotStatus::try_from)
                .collect::<Result<Vec<_>, _>>()?,
            write_quorum: value.write_quorum,
        })
    }
}

impl TryFrom<&StableReplicaSnapshot> for StableReplicaSnapshotStatus {
    type Error = String;

    fn try_from(value: &StableReplicaSnapshot) -> Result<Self, Self::Error> {
        Ok(Self {
            id: value.id,
            instance_id: value.instance_id.to_string(),
            role: match value.role {
                Role::Primary => StableReplicaRoleStatus::Primary,
                Role::ActiveSecondary => StableReplicaRoleStatus::ActiveSecondary,
                role => {
                    return Err(format!(
                        "replica {} has unsupported stable role {role:?}",
                        value.id
                    ));
                }
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stable_snapshot_round_trips_without_changing_incarnations() {
        let core = StablePartitionSnapshot {
            epoch: Epoch::new(2, 7),
            primary_id: 4,
            members: vec![
                StableReplicaSnapshot {
                    id: 4,
                    instance_id: ReplicaInstanceId::new("pod-uid/exact"),
                    role: Role::Primary,
                },
                StableReplicaSnapshot {
                    id: 8,
                    instance_id: ReplicaInstanceId::new("another uid"),
                    role: Role::ActiveSecondary,
                },
            ],
            write_quorum: 2,
        };

        let persisted = StablePartitionSnapshotStatus::try_from(&core).unwrap();
        let recovered = StablePartitionSnapshot::try_from(&persisted).unwrap();
        assert_eq!(recovered, core);

        let json = serde_json::to_value(&persisted).unwrap();
        assert_eq!(json["primaryId"], 4);
        assert_eq!(json["members"][0]["instanceId"], "pod-uid/exact");
        assert_eq!(json["members"][0]["role"], "primary");
        assert_eq!(json["members"][1]["role"], "activeSecondary");
    }

    #[test]
    fn status_without_stable_snapshot_remains_backward_compatible() {
        let status: KubericSetStatus =
            serde_json::from_value(serde_json::json!({"phase": "Healthy"})).unwrap();
        assert!(status.stable_snapshot.is_none());
        assert!(status.operation.is_none());
        assert!(status.conditions.is_empty());
        assert!(
            serde_json::to_value(status)
                .unwrap()
                .get("stableSnapshot")
                .is_none()
        );
    }

    #[test]
    fn persisted_snapshot_rejects_empty_incarnation() {
        let persisted = StableReplicaSnapshotStatus {
            id: 1,
            instance_id: String::new(),
            role: StableReplicaRoleStatus::Primary,
        };
        assert_eq!(
            StableReplicaSnapshot::try_from(&persisted).unwrap_err(),
            "stable snapshot replica 1 has an empty incarnation"
        );
    }

    #[test]
    fn core_snapshot_rejects_unsupported_stable_role() {
        let core = StableReplicaSnapshot {
            id: 1,
            instance_id: ReplicaInstanceId::new("one"),
            role: Role::IdleSecondary,
        };
        assert_eq!(
            StableReplicaSnapshotStatus::try_from(&core).unwrap_err(),
            "replica 1 has unsupported stable role IdleSecondary"
        );
    }
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone, JsonSchema, Default)]
pub enum Phase {
    #[default]
    Pending,
    Creating,
    Healthy,
    FailingOver,
    Switchover,
    AddingReplica,
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
