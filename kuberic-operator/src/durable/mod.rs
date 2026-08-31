use std::collections::BTreeMap;

use kuberic_core::types::{DurableReplicaAction, ReplicaStatusInfo};

use crate::crd::{
    DurableOperationKind, DurableOperationPhase, DurableOperationStatus,
    StablePartitionSnapshotStatus, StatusCondition,
};

mod add_replica;
mod create_partition;
mod failover;
pub mod failover_election;
mod remove_replica;
mod switchover;

pub use add_replica::{decide_add_replica, start_add_replica};
pub use create_partition::{
    CreatePartitionTarget, decide_create_partition, start_create_partition,
};
pub use failover::{
    action_for as failover_action_for, adopt_replacement_before_confirmation, decide_failover,
    pending_label as failover_pending_label, record_observation, start_failover,
};
pub use remove_replica::{RemoveReplicaTarget, decide_remove_replica, start_remove_replica};
pub use switchover::{decide, start_switchover};

// Includes authorization, dispatch-fence persistence, activity, and
// observation-first retry reconciles.
pub const ACTION_DEADLINE_SECONDS: i64 = 10;
const MAX_ERROR_LENGTH: usize = 512;

#[derive(Debug)]
pub struct ReplicaObservation {
    pub status: ReplicaStatusInfo,
    pub replicator_address: String,
    pub pod_name: String,
    pub pod_role_label: Option<String>,
}

pub type OperationObservations = BTreeMap<i64, ReplicaObservation>;
pub type OperationPodIdentities = BTreeMap<i64, String>;

#[derive(Debug)]
pub enum Decision {
    Persist(DurableOperationStatus),
    Execute {
        target_id: i64,
        action_id: String,
        action: DurableReplicaAction,
    },
    PatchPodRole {
        target_id: i64,
        role: String,
    },
    DeletePod {
        pod_name: String,
        expected_uid: String,
    },
    CommitSnapshot {
        operation: DurableOperationStatus,
        snapshot: StablePartitionSnapshotStatus,
    },
    Wait,
    Complete {
        operation: DurableOperationStatus,
        snapshot: StablePartitionSnapshotStatus,
        compensated: bool,
    },
    RestartCreation {
        operation: DurableOperationStatus,
    },
}

pub fn record_activity_error(
    operation: &DurableOperationStatus,
    error: &str,
) -> DurableOperationStatus {
    let mut next = operation.clone();
    let bounded = bounded_error(error);
    next.last_error = Some(bounded.clone());
    if let Some(pending) = &mut next.pending_action {
        pending.attempts = pending.attempts.saturating_add(1);
        pending.last_error = Some(bounded);
    }
    next
}

pub fn fail_closed(operation: &DurableOperationStatus, error: &str) -> DurableOperationStatus {
    poison(operation, error)
}

pub(crate) fn poison(operation: &DurableOperationStatus, message: &str) -> DurableOperationStatus {
    let mut next = operation.clone();
    next.phase = DurableOperationPhase::Poisoned;
    next.pending_action = None;
    next.last_error = Some(bounded_error(message));
    next
}

pub(crate) fn bounded_error(error: &str) -> String {
    error.chars().take(MAX_ERROR_LENGTH).collect()
}

pub fn operation_condition(operation: &DurableOperationStatus, now: i64) -> StatusCondition {
    let operation_name = match operation.kind {
        DurableOperationKind::CreatePartition => "partition creation",
        DurableOperationKind::Switchover => "switchover",
        DurableOperationKind::AddReplica => "replica add/rebuild",
        DurableOperationKind::RemoveReplica => "replica removal",
        DurableOperationKind::Failover => "failover",
    };
    let (status, reason, message) = match operation.phase {
        DurableOperationPhase::Completed => (
            "False",
            "Completed",
            format!("durable {operation_name} completed and stable topology was persisted"),
        ),
        DurableOperationPhase::Failed => (
            "False",
            "CompensatedOrSafeFailure",
            operation
                .last_error
                .as_deref()
                .unwrap_or("durable operation returned to its previous stable topology")
                .to_string(),
        ),
        DurableOperationPhase::Poisoned => (
            "True",
            "Poisoned",
            operation
                .last_error
                .as_deref()
                .unwrap_or("durable operation cannot advance safely")
                .to_string(),
        ),
        DurableOperationPhase::FailoverWaitForBestCandidate => (
            "True",
            "WaitingForBestCandidate",
            operation
                .failover
                .as_ref()
                .and_then(|failover| failover.assessment.clone())
                .unwrap_or_else(|| "waiting for the best eligible replica".to_string()),
        ),
        DurableOperationPhase::FailoverWaitForReadQuorum => (
            "True",
            "QuorumLoss",
            operation
                .failover
                .as_ref()
                .and_then(|failover| failover.assessment.clone())
                .unwrap_or_else(|| "waiting for previous/current read quorum".to_string()),
        ),
        DurableOperationPhase::FailoverNotifyDataLoss => (
            "True",
            "DataLossNegotiation",
            "advanced data-loss epoch is awaiting correlated callback completion".to_string(),
        ),
        _ => (
            "True",
            "Advancing",
            format!("durable {operation_name} is advancing one checkpoint at a time"),
        ),
    };
    StatusCondition {
        type_: "DurableOperation".to_string(),
        status: status.to_string(),
        reason: reason.to_string(),
        message: bounded_error(&message),
        last_transition_time: now.to_string(),
    }
}
