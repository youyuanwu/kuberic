use std::collections::HashSet;

use kuberic_core::types::{
    DurableActionState, DurableReplicaAction, Epoch, ReplicaConfigurationMemberStatus,
    ReplicaConfigurationMode, ReplicaConfigurationStatus, ReplicaInfo, ReplicaInstanceId,
    ReplicaSetConfig, ReplicaSetQuorumMode, ReplicaStatus, Role,
};

use crate::crd::{
    DURABLE_OPERATION_VERSION, DurableActionKind, DurableOperationKind, DurableOperationPhase,
    DurableOperationStatus, DurablePostconditionKind, DurablePostconditionStatus,
    DurableRemoveMode, EpochStatus, PendingActionStatus, StablePartitionSnapshotStatus,
    StableReplicaRoleStatus,
};

use super::{
    ACTION_DEADLINE_SECONDS, Decision, OperationObservations, OperationPodIdentities,
    bounded_error, poison, record_activity_error,
};

#[derive(Debug, Clone, PartialEq, Eq)]
enum ActionObservation {
    Precondition,
    InProgress,
    Postcondition,
    Failed(String),
    Unavailable,
    Impossible,
}

pub struct RemoveReplicaTarget {
    pub replica_id: i64,
    pub pod_name: String,
    pub pod_uid: String,
}

pub fn start_remove_replica(
    set_uid: &str,
    previous: StablePartitionSnapshotStatus,
    target_request: RemoveReplicaTarget,
    mode: DurableRemoveMode,
    min_replicas: usize,
    now: i64,
) -> Result<DurableOperationStatus, String> {
    validate_snapshot(&previous)?;
    if target_request.pod_name.is_empty() || target_request.pod_uid.is_empty() {
        return Err("remove target pod name and UID must be non-empty".to_string());
    }
    if target_request.replica_id == previous.primary_id {
        return Err("cannot remove the primary".to_string());
    }
    let target_member = previous
        .members
        .iter()
        .find(|member| member.id == target_request.replica_id)
        .ok_or_else(|| {
            format!(
                "remove target replica {} is not committed",
                target_request.replica_id
            )
        })?;
    if target_member.role != StableReplicaRoleStatus::ActiveSecondary {
        return Err("only a stable secondary can be removed".to_string());
    }
    if target_member.instance_id != target_request.pod_uid {
        return Err("remove target runtime incarnation and pod UID differ".to_string());
    }

    let mut target_snapshot = previous.clone();
    target_snapshot
        .members
        .retain(|member| member.id != target_request.replica_id);
    if target_snapshot.members.len() < min_replicas {
        return Err(format!(
            "removal would reduce membership below minReplicas ({min_replicas})"
        ));
    }
    if target_snapshot.members.len() < previous.write_quorum as usize {
        return Err("retained members cannot satisfy the previous write quorum".to_string());
    }
    target_snapshot.write_quorum = target_snapshot.members.len() as u32 / 2 + 1;
    validate_snapshot(&target_snapshot)?;

    let operation_id = format!(
        "{set_uid}:remove-replica:v{DURABLE_OPERATION_VERSION}:{}-{}:{}@{}",
        previous.epoch.data_loss_number,
        previous.epoch.configuration_number,
        target_request.replica_id,
        target_member.instance_id
    );
    Ok(DurableOperationStatus {
        execution_id: format!("{operation_id}:execution-1"),
        operation_id,
        version: DURABLE_OPERATION_VERSION,
        kind: DurableOperationKind::RemoveReplica,
        phase: DurableOperationPhase::RemoveCatchUpConfiguration,
        old_primary_id: previous.primary_id,
        target_primary_id: previous.primary_id,
        add_mode: None,
        remove_mode: Some(mode),
        target_replica_id: Some(target_request.replica_id),
        target_instance_id: Some(target_member.instance_id.clone()),
        target_pod_name: Some(target_request.pod_name),
        target_pod_uid: Some(target_request.pod_uid),
        retired_instance_id: None,
        previous_snapshot: previous.into(),
        target_snapshot,
        committed_snapshot: None,
        minimum_committed_replicas: None,
        frozen_lsn: None,
        next_secondary_index: 0,
        phase_deadline_unix_seconds: now + ACTION_DEADLINE_SECONDS,
        pending_action: None,
        last_error: None,
        failover: None,
    })
}

pub fn decide_remove_replica(
    operation: &DurableOperationStatus,
    observations: &OperationObservations,
    pod_identities: &OperationPodIdentities,
    now: i64,
) -> Result<Decision, String> {
    validate_operation(operation)?;
    if matches!(
        operation.phase,
        DurableOperationPhase::Completed
            | DurableOperationPhase::Failed
            | DurableOperationPhase::Poisoned
    ) {
        return Ok(Decision::Wait);
    }
    if let Some(pending) = &operation.pending_action {
        validate_pending_action(operation, pending)?;
    }
    if let Err(error) = validate_retained_observations(operation, observations) {
        return Ok(Decision::Persist(poison(operation, &error)));
    }
    if let Some(pending) = &operation.pending_action {
        return decide_pending(operation, pending, observations, pod_identities, now);
    }

    Ok(match operation.phase {
        DurableOperationPhase::RemoveCatchUpConfiguration => persist_pending(
            operation,
            pending_action(
                operation,
                1,
                DurableActionKind::RemoveCatchUpConfiguration,
                0,
                now,
            )?,
        ),
        DurableOperationPhase::RemoveWaitForCatchUpQuorum => persist_pending(
            operation,
            pending_action(
                operation,
                2,
                DurableActionKind::RemoveWaitForCatchUpQuorum,
                0,
                now,
            )?,
        ),
        DurableOperationPhase::RemoveCurrentConfiguration => persist_pending(
            operation,
            pending_action(
                operation,
                3,
                DurableActionKind::RemoveCurrentConfiguration,
                0,
                now,
            )?,
        ),
        DurableOperationPhase::RemovePrimaryConnection => persist_pending(
            operation,
            pending_action(
                operation,
                4,
                DurableActionKind::RemovePrimaryConnection,
                0,
                now,
            )?,
        ),
        DurableOperationPhase::RemoveDemoteTarget => {
            if target_observation(operation, observations).is_none() {
                Decision::Persist(with_phase(
                    operation,
                    DurableOperationPhase::RemoveCloseTarget,
                    now,
                ))
            } else {
                persist_pending(
                    operation,
                    pending_action(operation, 5, DurableActionKind::RemoveDemoteTarget, 0, now)?,
                )
            }
        }
        DurableOperationPhase::RemoveCloseTarget => {
            if target_observation(operation, observations).is_none() {
                Decision::Persist(with_phase(
                    operation,
                    DurableOperationPhase::RemoveDeleteTarget,
                    now,
                ))
            } else {
                persist_pending(
                    operation,
                    pending_action(operation, 6, DurableActionKind::RemoveCloseTarget, 0, now)?,
                )
            }
        }
        DurableOperationPhase::RemoveDeleteTarget => persist_pending(
            operation,
            pending_action(operation, 7, DurableActionKind::RemoveDeleteTarget, 0, now)?,
        ),
        DurableOperationPhase::RemoveFinalize => {
            let primary = observations
                .get(&operation.previous_snapshot.primary_id)
                .ok_or_else(|| "retained primary is unavailable".to_string())?;
            if primary.status.configuration.as_ref()
                != Some(&configuration_status_for_snapshot(
                    &operation.target_snapshot,
                    ReplicaConfigurationMode::Current,
                ))
            {
                Decision::Persist(poison(
                    operation,
                    "target current configuration is not committed at removal finalization",
                ))
            } else if has_target_connection(operation, observations)? {
                Decision::Persist(poison(
                    operation,
                    "removed replica connection remains on the primary",
                ))
            } else {
                let mut completed = operation.clone();
                completed.phase = DurableOperationPhase::Completed;
                completed.pending_action = None;
                Decision::Complete {
                    operation: completed,
                    snapshot: operation.target_snapshot.clone(),
                    compensated: false,
                }
            }
        }
        DurableOperationPhase::RemoveCompensateConfiguration => persist_pending(
            operation,
            pending_action(
                operation,
                100,
                DurableActionKind::RemoveCompensateConfiguration,
                0,
                now,
            )?,
        ),
        DurableOperationPhase::RemoveCompensateFinalize => {
            let mut failed = operation.clone();
            failed.phase = DurableOperationPhase::Failed;
            failed.pending_action = None;
            Decision::Complete {
                operation: failed,
                snapshot: operation.previous_snapshot.cloned().unwrap(),
                compensated: true,
            }
        }
        _ => {
            return Err(format!(
                "phase {:?} is not valid for replica removal",
                operation.phase
            ));
        }
    })
}

fn decide_pending(
    operation: &DurableOperationStatus,
    pending: &PendingActionStatus,
    observations: &OperationObservations,
    pod_identities: &OperationPodIdentities,
    now: i64,
) -> Result<Decision, String> {
    match observe_action(operation, pending, observations, pod_identities)? {
        ActionObservation::Postcondition => {
            let mut next = operation.clone();
            next.pending_action = None;
            next.last_error = None;
            next.phase_deadline_unix_seconds = now + ACTION_DEADLINE_SECONDS;
            next.phase = next_phase(pending.kind);
            if pending.kind == DurableActionKind::RemoveCurrentConfiguration {
                return Ok(Decision::CommitSnapshot {
                    operation: next,
                    snapshot: operation.target_snapshot.clone(),
                });
            }
            Ok(Decision::Persist(next))
        }
        ActionObservation::InProgress => {
            if now < pending.deadline_unix_seconds {
                Ok(Decision::Wait)
            } else if is_precommit(pending.kind) {
                Ok(Decision::Persist(failure_transition(
                    operation,
                    "durable removal activity remained in progress past its deadline",
                    now,
                )))
            } else {
                Ok(Decision::Wait)
            }
        }
        ActionObservation::Precondition => {
            if now >= pending.deadline_unix_seconds {
                if is_precommit(pending.kind) {
                    return Ok(Decision::Persist(failure_transition(
                        operation,
                        &format!(
                            "durable removal action {:?} reached its deadline",
                            pending.kind
                        ),
                        now,
                    )));
                }
                if is_target_cleanup(pending.kind) {
                    let mut next = operation.clone();
                    next.pending_action = None;
                    next.last_error = Some(bounded_error(&format!(
                        "best-effort cleanup action {:?} reached its deadline",
                        pending.kind
                    )));
                    next.phase = next_phase(pending.kind);
                    return Ok(Decision::Persist(next));
                }
            }
            if pending.kind == DurableActionKind::RemoveDeleteTarget {
                return Ok(Decision::DeletePod {
                    pod_name: target_pod_name(operation)?.to_string(),
                    expected_uid: target_pod_uid(operation)?.to_string(),
                });
            }
            Ok(Decision::Execute {
                target_id: pending.target_id,
                action_id: pending.action_id.clone(),
                action: action_for(operation, pending, observations)?,
            })
        }
        ActionObservation::Unavailable => {
            if is_target_cleanup(pending.kind) {
                let mut next = operation.clone();
                next.pending_action = None;
                next.last_error = None;
                next.phase = next_phase(pending.kind);
                Ok(Decision::Persist(next))
            } else {
                Ok(Decision::Persist(poison(
                    operation,
                    "required retained replica is unavailable",
                )))
            }
        }
        ActionObservation::Impossible => Ok(Decision::Persist(poison(
            operation,
            "live state matches neither the removal action precondition nor postcondition",
        ))),
        ActionObservation::Failed(error) => {
            if is_precommit(pending.kind) {
                Ok(Decision::Persist(failure_transition(
                    &record_activity_error(operation, &error),
                    &error,
                    now,
                )))
            } else if is_target_cleanup(pending.kind) {
                let mut next = record_activity_error(operation, &error);
                next.pending_action = None;
                next.phase = next_phase(pending.kind);
                Ok(Decision::Persist(next))
            } else {
                let attempt = pending.attempts.saturating_add(1);
                let mut next = record_activity_error(operation, &error);
                next.pending_action = Some(pending_action(
                    operation,
                    pending.sequence,
                    pending.kind,
                    attempt,
                    now,
                )?);
                Ok(Decision::Persist(next))
            }
        }
    }
}

fn observe_action(
    operation: &DurableOperationStatus,
    pending: &PendingActionStatus,
    observations: &OperationObservations,
    pod_identities: &OperationPodIdentities,
) -> Result<ActionObservation, String> {
    if pending.kind == DurableActionKind::RemoveDeleteTarget {
        let target_id = target_replica_id(operation)?;
        return Ok(
            if pod_identities.get(&target_id).map(String::as_str)
                != Some(target_pod_uid(operation)?)
            {
                ActionObservation::Postcondition
            } else {
                ActionObservation::Precondition
            },
        );
    }

    let Some(observation) = observations.get(&pending.target_id) else {
        return Ok(ActionObservation::Unavailable);
    };
    if observation.status.instance_id.as_str() != pending.target_instance_id {
        return Ok(ActionObservation::Impossible);
    }

    let expected_epoch = epoch(&operation.previous_snapshot.epoch);
    let status = &observation.status;
    let direct = match pending.kind {
        DurableActionKind::RemoveCatchUpConfiguration => configuration_observation(
            status,
            &configuration_status_for_snapshot(
                &operation.target_snapshot,
                ReplicaConfigurationMode::CatchUp,
            ),
            Some(&configuration_status_for_snapshot(
                &operation.previous_snapshot,
                ReplicaConfigurationMode::Current,
            )),
            expected_epoch,
        ),
        DurableActionKind::RemoveCurrentConfiguration => configuration_observation(
            status,
            &configuration_status_for_snapshot(
                &operation.target_snapshot,
                ReplicaConfigurationMode::Current,
            ),
            Some(&configuration_status_for_snapshot(
                &operation.target_snapshot,
                ReplicaConfigurationMode::CatchUp,
            )),
            expected_epoch,
        ),
        DurableActionKind::RemoveCompensateConfiguration => configuration_observation(
            status,
            &configuration_status_for_snapshot(
                &operation.previous_snapshot,
                ReplicaConfigurationMode::Current,
            ),
            Some(&configuration_status_for_snapshot(
                &operation.target_snapshot,
                ReplicaConfigurationMode::CatchUp,
            )),
            expected_epoch,
        ),
        DurableActionKind::RemovePrimaryConnection => {
            if has_target_connection(operation, observations)? {
                ActionObservation::Precondition
            } else {
                ActionObservation::Postcondition
            }
        }
        DurableActionKind::RemoveDemoteTarget => {
            if status.role == Role::None || !status.healthy {
                ActionObservation::Postcondition
            } else if status.role == Role::ActiveSecondary && status.epoch == expected_epoch {
                ActionObservation::Precondition
            } else {
                ActionObservation::Impossible
            }
        }
        DurableActionKind::RemoveCloseTarget => {
            if !status.healthy {
                ActionObservation::Postcondition
            } else if status.role == Role::None && status.epoch == expected_epoch {
                ActionObservation::Precondition
            } else {
                ActionObservation::Impossible
            }
        }
        DurableActionKind::RemoveWaitForCatchUpQuorum => ActionObservation::Precondition,
        DurableActionKind::RemoveDeleteTarget => unreachable!(),
        _ => return Err("non-remove action reached remove observation".to_string()),
    };
    if direct == ActionObservation::Postcondition {
        return Ok(direct);
    }

    let expected_action = action_for(operation, pending, observations)?;
    let expected_signature = super::pending_action_signature(pending, &expected_action)?;
    if let Some(active) =
        super::correlated_action_observation(&observation.status, &pending.action_id)
    {
        if active.signature != expected_signature {
            return Ok(ActionObservation::Impossible);
        }
        return Ok(match active.state {
            DurableActionState::Scheduled | DurableActionState::InProgress => {
                ActionObservation::InProgress
            }
            DurableActionState::Completed => ActionObservation::Postcondition,
            DurableActionState::Failed => ActionObservation::Failed(
                active
                    .error
                    .clone()
                    .unwrap_or_else(|| "runtime reported durable activity failure".to_string()),
            ),
        });
    }
    Ok(direct)
}

fn action_for(
    operation: &DurableOperationStatus,
    pending: &PendingActionStatus,
    observations: &OperationObservations,
) -> Result<DurableReplicaAction, String> {
    let target_id = target_replica_id(operation)?;
    let target_instance = ReplicaInstanceId::new(target_instance_id(operation)?.to_string());
    Ok(match pending.kind {
        DurableActionKind::RemoveCatchUpConfiguration => {
            DurableReplicaAction::UpdateCatchUpConfiguration {
                current: config_for_snapshot(&operation.target_snapshot, observations, None)?,
                previous: config_for_snapshot(
                    &operation.previous_snapshot,
                    observations,
                    Some(target_id),
                )?,
            }
        }
        DurableActionKind::RemoveWaitForCatchUpQuorum => {
            DurableReplicaAction::WaitForCatchUpQuorum {
                mode: ReplicaSetQuorumMode::Write,
            }
        }
        DurableActionKind::RemoveCurrentConfiguration => {
            DurableReplicaAction::UpdateCurrentConfiguration {
                current: config_for_snapshot(&operation.target_snapshot, observations, None)?,
            }
        }
        DurableActionKind::RemovePrimaryConnection => DurableReplicaAction::RemoveReplica {
            replica_id: target_id,
            instance_id: target_instance,
        },
        DurableActionKind::RemoveDemoteTarget => DurableReplicaAction::ChangeRole {
            epoch: epoch(&operation.previous_snapshot.epoch),
            role: Role::None,
        },
        DurableActionKind::RemoveCloseTarget => DurableReplicaAction::Close,
        DurableActionKind::RemoveCompensateConfiguration => {
            DurableReplicaAction::UpdateCurrentConfiguration {
                current: config_for_snapshot(
                    &operation.previous_snapshot,
                    observations,
                    Some(target_id),
                )?,
            }
        }
        DurableActionKind::RemoveDeleteTarget => {
            return Err("pod deletion is not a runtime action".to_string());
        }
        _ => return Err("non-remove action reached remove action construction".to_string()),
    })
}

fn pending_action(
    operation: &DurableOperationStatus,
    sequence: u32,
    kind: DurableActionKind,
    attempts: u32,
    now: i64,
) -> Result<PendingActionStatus, String> {
    let primary_id = operation.previous_snapshot.primary_id;
    let target_id = target_replica_id(operation)?;
    let runtime_target_id = match kind {
        DurableActionKind::RemoveDemoteTarget
        | DurableActionKind::RemoveCloseTarget
        | DurableActionKind::RemoveDeleteTarget => target_id,
        _ => primary_id,
    };
    let target_instance_id = if runtime_target_id == target_id {
        target_instance_id(operation)?.to_string()
    } else {
        operation
            .previous_snapshot
            .members
            .iter()
            .find(|member| member.id == primary_id)
            .map(|member| member.instance_id.clone())
            .ok_or_else(|| "stable primary is absent".to_string())?
    };
    let postcondition = match kind {
        DurableActionKind::RemoveCatchUpConfiguration => {
            DurablePostconditionKind::CatchUpConfiguration
        }
        DurableActionKind::RemoveWaitForCatchUpQuorum => DurablePostconditionKind::CatchUpQuorum,
        DurableActionKind::RemoveCurrentConfiguration
        | DurableActionKind::RemoveCompensateConfiguration => {
            DurablePostconditionKind::CurrentConfiguration
        }
        DurableActionKind::RemovePrimaryConnection => DurablePostconditionKind::ReplicaRemoved,
        DurableActionKind::RemoveDemoteTarget => DurablePostconditionKind::Role,
        DurableActionKind::RemoveCloseTarget => DurablePostconditionKind::Closed,
        DurableActionKind::RemoveDeleteTarget => DurablePostconditionKind::PodDeleted,
        _ => return Err("non-remove action cannot be scheduled by removal".to_string()),
    };
    Ok(PendingActionStatus {
        action_id: format!(
            "{}:{sequence}:{kind:?}:attempt-{attempts}",
            operation.execution_id
        ),
        sequence,
        kind,
        target_id: runtime_target_id,
        target_instance_id,
        expected_epoch: operation.previous_snapshot.epoch.clone(),
        desired_postcondition: DurablePostconditionStatus {
            kind: postcondition,
            role: None,
        },
        attempts,
        deadline_unix_seconds: now + ACTION_DEADLINE_SECONDS,
        last_error: None,
        dispatch_authorized: false,
        dispatch_agent_generation: None,
        dispatch_agent_control_version: None,
        dispatch_observed_runtime_epoch: None,
        dispatch_action_payload: String::new(),
    })
}

fn validate_pending_action(
    operation: &DurableOperationStatus,
    pending: &PendingActionStatus,
) -> Result<(), String> {
    if !action_allowed_in_phase(operation.phase, pending.kind) {
        return Err(format!(
            "pending action {:?} is illegal in phase {:?}",
            pending.kind, operation.phase
        ));
    }
    let expected = pending_action(
        operation,
        pending.sequence,
        pending.kind,
        pending.attempts,
        0,
    )?;
    let prior_attempt_id = pending
        .attempts
        .checked_sub(1)
        .map(|attempts| {
            pending_action(operation, pending.sequence, pending.kind, attempts, 0)
                .map(|action| action.action_id)
        })
        .transpose()?;
    if pending.action_id != expected.action_id
        && prior_attempt_id.as_deref() != Some(pending.action_id.as_str())
    {
        return Err("pending remove action ID is not deterministic".to_string());
    }
    if pending.target_id != expected.target_id
        || pending.target_instance_id != expected.target_instance_id
        || pending.expected_epoch != expected.expected_epoch
        || pending.desired_postcondition != expected.desired_postcondition
    {
        return Err("pending remove action is not deterministic".to_string());
    }
    Ok(())
}

fn action_allowed_in_phase(phase: DurableOperationPhase, kind: DurableActionKind) -> bool {
    matches!(
        (phase, kind),
        (
            DurableOperationPhase::RemoveCatchUpConfiguration,
            DurableActionKind::RemoveCatchUpConfiguration
        ) | (
            DurableOperationPhase::RemoveWaitForCatchUpQuorum,
            DurableActionKind::RemoveWaitForCatchUpQuorum
        ) | (
            DurableOperationPhase::RemoveCurrentConfiguration,
            DurableActionKind::RemoveCurrentConfiguration
        ) | (
            DurableOperationPhase::RemovePrimaryConnection,
            DurableActionKind::RemovePrimaryConnection
        ) | (
            DurableOperationPhase::RemoveDemoteTarget,
            DurableActionKind::RemoveDemoteTarget
        ) | (
            DurableOperationPhase::RemoveCloseTarget,
            DurableActionKind::RemoveCloseTarget
        ) | (
            DurableOperationPhase::RemoveDeleteTarget,
            DurableActionKind::RemoveDeleteTarget
        ) | (
            DurableOperationPhase::RemoveCompensateConfiguration,
            DurableActionKind::RemoveCompensateConfiguration
        )
    )
}

fn next_phase(kind: DurableActionKind) -> DurableOperationPhase {
    match kind {
        DurableActionKind::RemoveCatchUpConfiguration => {
            DurableOperationPhase::RemoveWaitForCatchUpQuorum
        }
        DurableActionKind::RemoveWaitForCatchUpQuorum => {
            DurableOperationPhase::RemoveCurrentConfiguration
        }
        DurableActionKind::RemoveCurrentConfiguration => {
            DurableOperationPhase::RemovePrimaryConnection
        }
        DurableActionKind::RemovePrimaryConnection => DurableOperationPhase::RemoveDemoteTarget,
        DurableActionKind::RemoveDemoteTarget => DurableOperationPhase::RemoveCloseTarget,
        DurableActionKind::RemoveCloseTarget => DurableOperationPhase::RemoveDeleteTarget,
        DurableActionKind::RemoveDeleteTarget => DurableOperationPhase::RemoveFinalize,
        DurableActionKind::RemoveCompensateConfiguration => {
            DurableOperationPhase::RemoveCompensateFinalize
        }
        _ => unreachable!("validated remove action"),
    }
}

fn failure_transition(
    operation: &DurableOperationStatus,
    error: &str,
    now: i64,
) -> DurableOperationStatus {
    let mut next = operation.clone();
    next.pending_action = None;
    next.last_error = Some(bounded_error(error));
    next.phase_deadline_unix_seconds = now + ACTION_DEADLINE_SECONDS;
    next.phase = match operation.phase {
        DurableOperationPhase::RemoveCatchUpConfiguration
        | DurableOperationPhase::RemoveWaitForCatchUpQuorum
        | DurableOperationPhase::RemoveCurrentConfiguration => {
            DurableOperationPhase::RemoveCompensateConfiguration
        }
        DurableOperationPhase::RemoveCompensateConfiguration => DurableOperationPhase::Poisoned,
        _ => DurableOperationPhase::RemoveFinalize,
    };
    next
}

fn persist_pending(operation: &DurableOperationStatus, pending: PendingActionStatus) -> Decision {
    let mut next = operation.clone();
    next.pending_action = Some(pending);
    Decision::Persist(next)
}

fn with_phase(
    operation: &DurableOperationStatus,
    phase: DurableOperationPhase,
    now: i64,
) -> DurableOperationStatus {
    let mut next = operation.clone();
    next.phase = phase;
    next.pending_action = None;
    next.phase_deadline_unix_seconds = now + ACTION_DEADLINE_SECONDS;
    next
}

fn is_precommit(kind: DurableActionKind) -> bool {
    matches!(
        kind,
        DurableActionKind::RemoveCatchUpConfiguration
            | DurableActionKind::RemoveWaitForCatchUpQuorum
            | DurableActionKind::RemoveCurrentConfiguration
    )
}

fn is_target_cleanup(kind: DurableActionKind) -> bool {
    matches!(
        kind,
        DurableActionKind::RemoveDemoteTarget
            | DurableActionKind::RemoveCloseTarget
            | DurableActionKind::RemoveDeleteTarget
    )
}

fn target_replica_id(operation: &DurableOperationStatus) -> Result<i64, String> {
    operation
        .target_replica_id
        .ok_or_else(|| "remove operation has no target replica ID".to_string())
}

fn target_instance_id(operation: &DurableOperationStatus) -> Result<&str, String> {
    operation
        .target_instance_id
        .as_deref()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "remove operation has no target incarnation".to_string())
}

fn target_pod_name(operation: &DurableOperationStatus) -> Result<&str, String> {
    operation
        .target_pod_name
        .as_deref()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "remove operation has no target pod name".to_string())
}

fn target_pod_uid(operation: &DurableOperationStatus) -> Result<&str, String> {
    operation
        .target_pod_uid
        .as_deref()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "remove operation has no target pod UID".to_string())
}

fn target_observation<'a>(
    operation: &DurableOperationStatus,
    observations: &'a OperationObservations,
) -> Option<&'a super::ReplicaObservation> {
    let target_id = operation.target_replica_id?;
    let target_instance = operation.target_instance_id.as_deref()?;
    observations
        .get(&target_id)
        .filter(|observation| observation.status.instance_id.as_str() == target_instance)
}

fn has_target_connection(
    operation: &DurableOperationStatus,
    observations: &OperationObservations,
) -> Result<bool, String> {
    let primary = observations
        .get(&operation.previous_snapshot.primary_id)
        .ok_or_else(|| "retained primary is unavailable".to_string())?;
    let target_id = target_replica_id(operation)?;
    let target_instance = target_instance_id(operation)?;
    Ok(primary
        .status
        .active_replica_connections
        .iter()
        .any(|connection| {
            connection.id == target_id && connection.instance_id.as_str() == target_instance
        }))
}

fn config_for_snapshot(
    snapshot: &StablePartitionSnapshotStatus,
    observations: &OperationObservations,
    allow_missing_id: Option<i64>,
) -> Result<ReplicaSetConfig, String> {
    let mut members = Vec::new();
    for member in &snapshot.members {
        if member.id == snapshot.primary_id {
            continue;
        }
        let observation = observations.get(&member.id);
        if observation.is_none() && Some(member.id) != allow_missing_id {
            return Err(format!(
                "replica {} is unavailable for configuration",
                member.id
            ));
        }
        members.push(ReplicaInfo {
            id: member.id,
            instance_id: ReplicaInstanceId::new(member.instance_id.clone()),
            role: Role::ActiveSecondary,
            status: if observation.is_some() {
                ReplicaStatus::Up
            } else {
                ReplicaStatus::Down
            },
            replicator_address: observation
                .map(|value| value.replicator_address.clone())
                .unwrap_or_default(),
            current_progress: observation.map_or(0, |value| value.status.current_progress),
            catch_up_capability: observation.map_or(0, |value| value.status.current_progress),
            must_catch_up: false,
        });
    }
    Ok(ReplicaSetConfig {
        members,
        write_quorum: snapshot.write_quorum,
    })
}

fn configuration_status_for_snapshot(
    snapshot: &StablePartitionSnapshotStatus,
    mode: ReplicaConfigurationMode,
) -> ReplicaConfigurationStatus {
    let mut members = snapshot
        .members
        .iter()
        .filter(|member| member.id != snapshot.primary_id)
        .map(|member| ReplicaConfigurationMemberStatus {
            id: member.id,
            instance_id: ReplicaInstanceId::new(member.instance_id.clone()),
            role: Role::ActiveSecondary,
        })
        .collect::<Vec<_>>();
    members.sort_by_key(|member| member.id);
    ReplicaConfigurationStatus {
        mode,
        members,
        write_quorum: snapshot.write_quorum,
    }
}

fn configuration_observation(
    status: &kuberic_core::types::ReplicaStatusInfo,
    expected: &ReplicaConfigurationStatus,
    precondition: Option<&ReplicaConfigurationStatus>,
    expected_epoch: Epoch,
) -> ActionObservation {
    if status.role != Role::Primary || status.epoch != expected_epoch {
        ActionObservation::Impossible
    } else if status.configuration.as_ref() == Some(expected) {
        ActionObservation::Postcondition
    } else if status.configuration.as_ref() == precondition {
        ActionObservation::Precondition
    } else {
        ActionObservation::Impossible
    }
}

fn validate_operation(operation: &DurableOperationStatus) -> Result<(), String> {
    if operation.version != DURABLE_OPERATION_VERSION {
        return Err(format!(
            "unsupported durable operation version {}",
            operation.version
        ));
    }
    if operation.kind != DurableOperationKind::RemoveReplica {
        return Err("operation kind is not remove-replica".to_string());
    }
    if operation.previous_snapshot.is_none() {
        return Err("remove operation has no previous stable snapshot".to_string());
    }
    validate_snapshot(&operation.previous_snapshot)?;
    validate_snapshot(&operation.target_snapshot)?;
    operation
        .remove_mode
        .ok_or_else(|| "remove operation has no mode".to_string())?;
    if operation.add_mode.is_some() {
        return Err("remove operation contains add mode".to_string());
    }
    let target_id = target_replica_id(operation)?;
    let target_instance = target_instance_id(operation)?;
    target_pod_name(operation)?;
    let pod_uid = target_pod_uid(operation)?;
    if target_instance != pod_uid {
        return Err("remove target runtime incarnation and pod UID differ".to_string());
    }
    if target_id == operation.previous_snapshot.primary_id {
        return Err("cannot remove the primary".to_string());
    }
    let previous_target = operation
        .previous_snapshot
        .members
        .iter()
        .find(|member| member.id == target_id)
        .ok_or_else(|| "remove previous snapshot has no target".to_string())?;
    if previous_target.instance_id != target_instance
        || previous_target.role != StableReplicaRoleStatus::ActiveSecondary
    {
        return Err("remove target identity or role is invalid".to_string());
    }
    if operation
        .target_snapshot
        .members
        .iter()
        .any(|member| member.id == target_id)
    {
        return Err("remove target remains in target snapshot".to_string());
    }
    if operation.target_snapshot.members.len() + 1 != operation.previous_snapshot.members.len()
        || operation.previous_snapshot.primary_id != operation.target_snapshot.primary_id
        || operation.previous_snapshot.epoch != operation.target_snapshot.epoch
        || operation.old_primary_id != operation.previous_snapshot.primary_id
        || operation.target_primary_id != operation.previous_snapshot.primary_id
    {
        return Err("remove snapshot transition is invalid".to_string());
    }
    for retained in &operation.target_snapshot.members {
        if !operation
            .previous_snapshot
            .members
            .iter()
            .any(|member| member == retained)
        {
            return Err(format!(
                "target snapshot changed retained replica {}",
                retained.id
            ));
        }
    }
    if operation.target_snapshot.members.len() < operation.previous_snapshot.write_quorum as usize {
        return Err("retained members cannot satisfy the previous write quorum".to_string());
    }
    Ok(())
}

fn validate_retained_observations(
    operation: &DurableOperationStatus,
    observations: &OperationObservations,
) -> Result<(), String> {
    let expected_epoch = epoch(&operation.previous_snapshot.epoch);
    for member in &operation.target_snapshot.members {
        let observation = observations
            .get(&member.id)
            .ok_or_else(|| format!("retained replica {} is unavailable", member.id))?;
        if observation.status.instance_id.as_str() != member.instance_id {
            return Err(format!(
                "retained replica {} incarnation changed",
                member.id
            ));
        }
        let expected_role = if member.id == operation.target_snapshot.primary_id {
            Role::Primary
        } else {
            Role::ActiveSecondary
        };
        if observation.status.role != expected_role || observation.status.epoch != expected_epoch {
            return Err(format!(
                "retained replica {} has an unexpected role or epoch",
                member.id
            ));
        }
    }
    let primary = observations
        .get(&operation.previous_snapshot.primary_id)
        .ok_or_else(|| "retained primary is unavailable".to_string())?;
    let previous = configuration_status_for_snapshot(
        &operation.previous_snapshot,
        ReplicaConfigurationMode::Current,
    );
    let target_catch_up = configuration_status_for_snapshot(
        &operation.target_snapshot,
        ReplicaConfigurationMode::CatchUp,
    );
    let target_current = configuration_status_for_snapshot(
        &operation.target_snapshot,
        ReplicaConfigurationMode::Current,
    );
    let observed = primary.status.configuration.as_ref();
    let valid = match operation.phase {
        DurableOperationPhase::RemoveCatchUpConfiguration => {
            observed == Some(&previous) || observed == Some(&target_catch_up)
        }
        DurableOperationPhase::RemoveWaitForCatchUpQuorum => observed == Some(&target_catch_up),
        DurableOperationPhase::RemoveCurrentConfiguration => {
            observed == Some(&target_catch_up) || observed == Some(&target_current)
        }
        DurableOperationPhase::RemovePrimaryConnection
        | DurableOperationPhase::RemoveDemoteTarget
        | DurableOperationPhase::RemoveCloseTarget
        | DurableOperationPhase::RemoveDeleteTarget
        | DurableOperationPhase::RemoveFinalize => observed == Some(&target_current),
        DurableOperationPhase::RemoveCompensateConfiguration => {
            observed == Some(&target_catch_up) || observed == Some(&previous)
        }
        DurableOperationPhase::RemoveCompensateFinalize => observed == Some(&previous),
        _ => true,
    };
    if !valid {
        return Err("primary configuration is unexpected for removal phase".to_string());
    }
    Ok(())
}

fn validate_snapshot(snapshot: &StablePartitionSnapshotStatus) -> Result<(), String> {
    if snapshot.members.is_empty() {
        return Err("stable snapshot has no members".to_string());
    }
    let mut ids = HashSet::new();
    let mut incarnations = HashSet::new();
    let mut primary_count = 0;
    for member in &snapshot.members {
        if member.instance_id.is_empty() {
            return Err(format!(
                "stable snapshot replica {} has an empty incarnation",
                member.id
            ));
        }
        if !ids.insert(member.id) || !incarnations.insert(member.instance_id.clone()) {
            return Err("stable snapshot contains duplicate identity".to_string());
        }
        match member.role {
            StableReplicaRoleStatus::Primary => {
                primary_count += 1;
                if member.id != snapshot.primary_id {
                    return Err("stable snapshot primary identity is inconsistent".to_string());
                }
            }
            StableReplicaRoleStatus::ActiveSecondary => {}
        }
    }
    if primary_count != 1 {
        return Err("stable snapshot must contain exactly one primary".to_string());
    }
    let expected_quorum = snapshot.members.len() as u32 / 2 + 1;
    if snapshot.write_quorum != expected_quorum {
        return Err(format!(
            "stable snapshot write quorum {} is not majority {}",
            snapshot.write_quorum, expected_quorum
        ));
    }
    Ok(())
}

fn epoch(value: &EpochStatus) -> Epoch {
    Epoch::new(value.data_loss_number, value.configuration_number)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use kuberic_core::types::{AccessStatus, ReplicaConnectionStatus, ReplicaStatusInfo};

    use super::*;
    use crate::durable::ReplicaObservation;

    fn snapshot() -> StablePartitionSnapshotStatus {
        StablePartitionSnapshotStatus {
            epoch: EpochStatus {
                data_loss_number: 1,
                configuration_number: 7,
            },
            primary_id: 1,
            members: vec![
                crate::crd::StableReplicaSnapshotStatus {
                    id: 1,
                    instance_id: "one".to_string(),
                    role: StableReplicaRoleStatus::Primary,
                    election_metadata: None,
                },
                crate::crd::StableReplicaSnapshotStatus {
                    id: 2,
                    instance_id: "two".to_string(),
                    role: StableReplicaRoleStatus::ActiveSecondary,
                    election_metadata: None,
                },
                crate::crd::StableReplicaSnapshotStatus {
                    id: 3,
                    instance_id: "three".to_string(),
                    role: StableReplicaRoleStatus::ActiveSecondary,
                    election_metadata: None,
                },
            ],
            write_quorum: 2,
        }
    }

    fn observation(
        id: i64,
        instance: &str,
        role: Role,
        configuration: Option<ReplicaConfigurationStatus>,
    ) -> ReplicaObservation {
        ReplicaObservation {
            status: ReplicaStatusInfo {
                instance_id: ReplicaInstanceId::new(instance),
                role,
                epoch: Epoch::new(1, 7),
                current_progress: 10,
                catch_up_capability: Some(10),
                committed_lsn: 10,
                healthy: true,
                write_status: if role == Role::Primary {
                    AccessStatus::Granted
                } else {
                    AccessStatus::NotPrimary
                },
                configuration,
                election_configuration: None,
                deactivation_info: None,
                active_replica_connections: if role == Role::Primary {
                    vec![
                        ReplicaConnectionStatus {
                            id: 2,
                            instance_id: ReplicaInstanceId::new("two"),
                        },
                        ReplicaConnectionStatus {
                            id: 3,
                            instance_id: ReplicaInstanceId::new("three"),
                        },
                    ]
                } else {
                    Vec::new()
                },
                agent: kuberic_core::types::ReplicaAgentStatus {
                    protocol_version:
                        kuberic_core::replica_agent::CORRELATED_CONTROL_PROTOCOL_VERSION,
                    generation: kuberic_core::types::AgentGeneration::parse(
                        "0123456789abcdef0123456789abcdef",
                    )
                    .unwrap(),
                    control_version: kuberic_core::types::AgentControlVersion::default(),
                    current_action: None,
                    retained_terminal_actions: Vec::new(),
                    local_faults: Vec::new(),
                },
            },
            replicator_address: format!("http://{instance}"),
            pod_name: format!("set-{}", id - 1),
            pod_role_label: None,
        }
    }

    fn target(id: i64, instance: &str) -> RemoveReplicaTarget {
        RemoveReplicaTarget {
            replica_id: id,
            pod_name: format!("set-{}", id - 1),
            pod_uid: instance.to_string(),
        }
    }

    #[test]
    fn start_rejects_primary_minimum_and_invalid_quorum() {
        assert_eq!(
            start_remove_replica(
                "set",
                snapshot(),
                target(1, "one"),
                DurableRemoveMode::ScaleDown,
                1,
                10,
            )
            .unwrap_err(),
            "cannot remove the primary"
        );
        assert!(
            start_remove_replica(
                "set",
                snapshot(),
                target(3, "three"),
                DurableRemoveMode::ScaleDown,
                3,
                10,
            )
            .unwrap_err()
            .contains("below minReplicas")
        );
        let mut invalid = snapshot();
        invalid.write_quorum = 3;
        assert!(
            start_remove_replica(
                "set",
                invalid,
                target(3, "three"),
                DurableRemoveMode::Force,
                1,
                10,
            )
            .unwrap_err()
            .contains("not majority")
        );
    }

    #[test]
    fn unsupported_version_and_illegal_phase_action_are_rejected() {
        let mut operation = start_remove_replica(
            "set",
            snapshot(),
            target(3, "three"),
            DurableRemoveMode::ScaleDown,
            1,
            10,
        )
        .unwrap();
        operation.version += 1;
        assert!(
            decide_remove_replica(&operation, &BTreeMap::new(), &BTreeMap::new(), 10)
                .unwrap_err()
                .contains("unsupported")
        );

        operation.version = DURABLE_OPERATION_VERSION;
        operation.phase = DurableOperationPhase::RemoveCloseTarget;
        operation.pending_action = Some(
            pending_action(
                &operation,
                1,
                DurableActionKind::RemoveCatchUpConfiguration,
                0,
                10,
            )
            .unwrap(),
        );
        assert!(
            decide_remove_replica(&operation, &BTreeMap::new(), &BTreeMap::new(), 10)
                .unwrap_err()
                .contains("illegal")
        );
    }

    #[test]
    fn retained_identity_drift_poisoned_without_activity() {
        let operation = start_remove_replica(
            "set",
            snapshot(),
            target(3, "three"),
            DurableRemoveMode::Force,
            1,
            10,
        )
        .unwrap();
        let previous = configuration_status_for_snapshot(
            &operation.previous_snapshot,
            ReplicaConfigurationMode::Current,
        );
        let mut observations = BTreeMap::new();
        observations.insert(
            1,
            observation(1, "replacement", Role::Primary, Some(previous)),
        );
        observations.insert(2, observation(2, "two", Role::ActiveSecondary, None));
        match decide_remove_replica(&operation, &observations, &BTreeMap::new(), 10).unwrap() {
            Decision::Persist(next) => {
                assert_eq!(next.phase, DurableOperationPhase::Poisoned);
                assert!(
                    next.last_error
                        .as_deref()
                        .unwrap()
                        .contains("incarnation changed")
                );
            }
            other => panic!("identity drift produced unsafe decision: {other:?}"),
        }
    }
}
