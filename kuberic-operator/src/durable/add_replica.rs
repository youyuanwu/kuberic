use std::collections::HashSet;

use kuberic_core::types::{
    DurableActionState, DurableReplicaAction, Epoch, OpenMode, ReplicaConfigurationMode,
    ReplicaConfigurationStatus, ReplicaInfo, ReplicaInstanceId, ReplicaSetConfig,
    ReplicaSetQuorumMode, ReplicaStatus, Role,
};

use crate::crd::{
    DURABLE_OPERATION_VERSION, DurableActionKind, DurableAddMode, DurableOperationKind,
    DurableOperationPhase, DurableOperationStatus, DurablePostconditionKind,
    DurablePostconditionStatus, EpochStatus, PendingActionStatus, StablePartitionSnapshotStatus,
    StableReplicaRoleStatus, StableReplicaSnapshotStatus,
};

use super::{
    ACTION_DEADLINE_SECONDS, Decision, OperationObservations, bounded_error, poison,
    record_activity_error,
};

const BUILD_DEADLINE_SECONDS: i64 = 600;

#[derive(Debug, Clone, PartialEq, Eq)]
enum ActionObservation {
    Precondition,
    InProgress,
    Postcondition,
    Failed(String),
    Unavailable,
    Impossible,
}

pub fn start_add_replica(
    set_uid: &str,
    previous: StablePartitionSnapshotStatus,
    target_replica_id: i64,
    target_instance_id: String,
    target_pod_name: String,
    mode: DurableAddMode,
    now: i64,
) -> Result<DurableOperationStatus, String> {
    validate_snapshot(&previous)?;
    if target_instance_id.is_empty() {
        return Err("add target incarnation is empty".to_string());
    }
    if target_replica_id == previous.primary_id {
        return Err("cannot add or rebuild the primary as a secondary".to_string());
    }
    let existing = previous
        .members
        .iter()
        .find(|member| member.id == target_replica_id);
    let retired_instance_id = match mode {
        DurableAddMode::ScaleUp => {
            if existing.is_some() {
                return Err(format!(
                    "scale-up target replica {target_replica_id} is already committed"
                ));
            }
            None
        }
        DurableAddMode::Rebuild => {
            let member = existing.ok_or_else(|| {
                format!("rebuild target replica {target_replica_id} is not committed")
            })?;
            if member.role != StableReplicaRoleStatus::ActiveSecondary {
                return Err("only a stable secondary can be rebuilt".to_string());
            }
            if member.instance_id == target_instance_id {
                return Err("rebuild target must have a new incarnation".to_string());
            }
            Some(member.instance_id.clone())
        }
    };
    if previous
        .members
        .iter()
        .any(|member| member.instance_id == target_instance_id && member.id != target_replica_id)
    {
        return Err("add target incarnation duplicates another stable member".to_string());
    }

    let mut target = previous.clone();
    match existing {
        Some(_) => {
            let member = target
                .members
                .iter_mut()
                .find(|member| member.id == target_replica_id)
                .unwrap();
            member.instance_id = target_instance_id.clone();
        }
        None => target.members.push(StableReplicaSnapshotStatus {
            id: target_replica_id,
            instance_id: target_instance_id.clone(),
            role: StableReplicaRoleStatus::ActiveSecondary,
            election_metadata: None,
        }),
    }
    target.members.sort_by_key(|member| member.id);
    target.write_quorum = target.members.len() as u32 / 2 + 1;
    validate_snapshot(&target)?;

    let operation_id = format!(
        "{set_uid}:add-replica:v{DURABLE_OPERATION_VERSION}:{}-{}:{}@{}",
        previous.epoch.data_loss_number,
        previous.epoch.configuration_number,
        target_replica_id,
        target_instance_id
    );
    Ok(DurableOperationStatus {
        execution_id: format!("{operation_id}:execution-1"),
        operation_id,
        version: DURABLE_OPERATION_VERSION,
        kind: DurableOperationKind::AddReplica,
        phase: if mode == DurableAddMode::Rebuild {
            DurableOperationPhase::RetireOldReplica
        } else {
            DurableOperationPhase::OpenCandidate
        },
        old_primary_id: previous.primary_id,
        target_primary_id: previous.primary_id,
        add_mode: Some(mode),
        remove_mode: None,
        target_replica_id: Some(target_replica_id),
        target_instance_id: Some(target_instance_id),
        target_pod_name: Some(target_pod_name),
        target_pod_uid: None,
        retired_instance_id,
        previous_snapshot: previous.into(),
        target_snapshot: target,
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

pub fn decide_add_replica(
    operation: &DurableOperationStatus,
    observations: &OperationObservations,
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
        return decide_pending(operation, pending, observations, now);
    }

    let decision = match operation.phase {
        DurableOperationPhase::RetireOldReplica => persist_pending(
            operation,
            pending_action(operation, 1, DurableActionKind::RetireOldReplica, now)?,
        ),
        DurableOperationPhase::OpenCandidate => persist_pending(
            operation,
            pending_action(operation, 2, DurableActionKind::OpenCandidate, now)?,
        ),
        DurableOperationPhase::UpdateCandidateEpoch => persist_pending(
            operation,
            pending_action(operation, 3, DurableActionKind::UpdateCandidateEpoch, now)?,
        ),
        DurableOperationPhase::AssignCandidateIdle => persist_pending(
            operation,
            pending_action(operation, 4, DurableActionKind::AssignCandidateIdle, now)?,
        ),
        DurableOperationPhase::BuildCandidate => persist_pending(
            operation,
            pending_action(operation, 5, DurableActionKind::BuildCandidate, now)?,
        ),
        DurableOperationPhase::AssignCandidateActive => persist_pending(
            operation,
            pending_action(operation, 6, DurableActionKind::AssignCandidateActive, now)?,
        ),
        DurableOperationPhase::AddCatchUpConfiguration => persist_pending(
            operation,
            pending_action(
                operation,
                7,
                DurableActionKind::AddCatchUpConfiguration,
                now,
            )?,
        ),
        DurableOperationPhase::AddWaitForCatchUpQuorum => persist_pending(
            operation,
            pending_action(
                operation,
                8,
                DurableActionKind::AddWaitForCatchUpQuorum,
                now,
            )?,
        ),
        DurableOperationPhase::AddCurrentConfiguration => persist_pending(
            operation,
            pending_action(
                operation,
                9,
                DurableActionKind::AddCurrentConfiguration,
                now,
            )?,
        ),
        DurableOperationPhase::LabelCandidateSecondary => persist_pending(
            operation,
            pending_action(
                operation,
                10,
                DurableActionKind::LabelCandidateSecondary,
                now,
            )?,
        ),
        DurableOperationPhase::AddFinalize => {
            if validate_target_observation(operation, observations).is_ok() {
                let mut completed = operation.clone();
                completed.phase = DurableOperationPhase::Completed;
                completed.pending_action = None;
                Decision::Complete {
                    operation: completed,
                    snapshot: operation.target_snapshot.clone(),
                    compensated: false,
                }
            } else if now >= operation.phase_deadline_unix_seconds {
                Decision::Persist(poison(
                    operation,
                    "target topology could not be attested after current configuration commit",
                ))
            } else {
                Decision::Wait
            }
        }
        DurableOperationPhase::CompensateRestoreConfiguration => persist_pending(
            operation,
            pending_action(
                operation,
                100,
                DurableActionKind::CompensateRestoreConfiguration,
                now,
            )?,
        ),
        DurableOperationPhase::CompensateRemoveCandidate => persist_pending(
            operation,
            pending_action(
                operation,
                101,
                DurableActionKind::CompensateRemoveCandidate,
                now,
            )?,
        ),
        DurableOperationPhase::CompensateDemoteCandidate => persist_pending(
            operation,
            pending_action(
                operation,
                102,
                DurableActionKind::CompensateDemoteCandidate,
                now,
            )?,
        ),
        DurableOperationPhase::CompensateCloseCandidate => persist_pending(
            operation,
            pending_action(
                operation,
                103,
                DurableActionKind::CompensateCloseCandidate,
                now,
            )?,
        ),
        DurableOperationPhase::CompensateDeleteCandidate => persist_pending(
            operation,
            pending_action(
                operation,
                104,
                DurableActionKind::CompensateDeleteCandidate,
                now,
            )?,
        ),
        DurableOperationPhase::AddCompensateFinalize => {
            let mut failed = operation.clone();
            failed.phase = DurableOperationPhase::Failed;
            Decision::Complete {
                operation: failed,
                snapshot: operation.previous_snapshot.cloned().unwrap(),
                compensated: true,
            }
        }
        _ => {
            return Err(format!(
                "phase {:?} is not valid for add-replica",
                operation.phase
            ));
        }
    };
    Ok(decision)
}

fn decide_pending(
    operation: &DurableOperationStatus,
    pending: &PendingActionStatus,
    observations: &OperationObservations,
    now: i64,
) -> Result<Decision, String> {
    match observe_action(operation, pending, observations)? {
        ActionObservation::Postcondition => {
            let mut next = operation.clone();
            next.pending_action = None;
            next.last_error = None;
            next.phase_deadline_unix_seconds = now + ACTION_DEADLINE_SECONDS;
            next.phase = next_phase(pending.kind);
            Ok(Decision::Persist(next))
        }
        ActionObservation::InProgress => {
            if now >= pending.deadline_unix_seconds {
                Ok(Decision::Persist(failure_transition(
                    operation,
                    "durable activity remained in progress past its deadline",
                    now,
                )))
            } else {
                Ok(Decision::Wait)
            }
        }
        ActionObservation::Precondition => {
            if now >= pending.deadline_unix_seconds {
                return Ok(Decision::Persist(failure_transition(
                    operation,
                    &format!("durable action {:?} reached its deadline", pending.kind),
                    now,
                )));
            }
            if pending.kind == DurableActionKind::LabelCandidateSecondary {
                return Ok(Decision::PatchPodRole {
                    target_id: pending.target_id,
                    role: "secondary".to_string(),
                });
            }
            if pending.kind == DurableActionKind::CompensateDeleteCandidate {
                return Ok(Decision::DeletePod {
                    pod_name: target_pod_name(operation)?.to_string(),
                    expected_uid: target_instance_id(operation)?.to_string(),
                });
            }
            Ok(Decision::Execute {
                target_id: pending.target_id,
                action_id: pending.action_id.clone(),
                action: action_for(operation, pending, observations)?,
            })
        }
        ActionObservation::Failed(error) => Ok(Decision::Persist(failure_transition(
            &record_activity_error(operation, &error),
            &error,
            now,
        ))),
        ActionObservation::Unavailable => {
            if pending.kind == DurableActionKind::CompensateDeleteCandidate {
                let mut next = operation.clone();
                next.pending_action = None;
                next.phase = DurableOperationPhase::AddCompensateFinalize;
                return Ok(Decision::Persist(next));
            }
            if now >= pending.deadline_unix_seconds {
                Ok(Decision::Persist(failure_transition(
                    operation,
                    "durable action target was unavailable at its deadline",
                    now,
                )))
            } else {
                Ok(Decision::Wait)
            }
        }
        ActionObservation::Impossible => Ok(Decision::Persist(poison(
            operation,
            "live state matches neither the durable action precondition nor postcondition",
        ))),
    }
}

fn next_phase(kind: DurableActionKind) -> DurableOperationPhase {
    match kind {
        DurableActionKind::RetireOldReplica => DurableOperationPhase::OpenCandidate,
        DurableActionKind::OpenCandidate => DurableOperationPhase::UpdateCandidateEpoch,
        DurableActionKind::UpdateCandidateEpoch => DurableOperationPhase::AssignCandidateIdle,
        DurableActionKind::AssignCandidateIdle => DurableOperationPhase::BuildCandidate,
        DurableActionKind::BuildCandidate => DurableOperationPhase::AssignCandidateActive,
        DurableActionKind::AssignCandidateActive => DurableOperationPhase::AddCatchUpConfiguration,
        DurableActionKind::AddCatchUpConfiguration => {
            DurableOperationPhase::AddWaitForCatchUpQuorum
        }
        DurableActionKind::AddWaitForCatchUpQuorum => {
            DurableOperationPhase::AddCurrentConfiguration
        }
        DurableActionKind::AddCurrentConfiguration => {
            DurableOperationPhase::LabelCandidateSecondary
        }
        DurableActionKind::LabelCandidateSecondary => DurableOperationPhase::AddFinalize,
        DurableActionKind::CompensateRestoreConfiguration => {
            DurableOperationPhase::CompensateRemoveCandidate
        }
        DurableActionKind::CompensateRemoveCandidate => {
            DurableOperationPhase::CompensateDemoteCandidate
        }
        DurableActionKind::CompensateDemoteCandidate => {
            DurableOperationPhase::CompensateCloseCandidate
        }
        DurableActionKind::CompensateCloseCandidate => {
            DurableOperationPhase::CompensateDeleteCandidate
        }
        DurableActionKind::CompensateDeleteCandidate => {
            DurableOperationPhase::AddCompensateFinalize
        }
        _ => unreachable!("validated add action"),
    }
}

fn observe_action(
    operation: &DurableOperationStatus,
    pending: &PendingActionStatus,
    observations: &OperationObservations,
) -> Result<ActionObservation, String> {
    let observation = observations.get(&pending.target_id);
    if pending.kind == DurableActionKind::CompensateDeleteCandidate {
        return Ok(if observation.is_none() {
            ActionObservation::Postcondition
        } else {
            ActionObservation::Precondition
        });
    }
    let Some(observation) = observation else {
        if matches!(
            pending.kind,
            DurableActionKind::CompensateDemoteCandidate
                | DurableActionKind::CompensateCloseCandidate
        ) {
            return Ok(ActionObservation::Postcondition);
        }
        return Ok(ActionObservation::Unavailable);
    };
    if observation.status.instance_id.as_str() != pending.target_instance_id {
        return Ok(ActionObservation::Impossible);
    }

    let stable_epoch = epoch(&operation.previous_snapshot.epoch);
    let status = &observation.status;
    let direct = match pending.kind {
        DurableActionKind::OpenCandidate => {
            if status.healthy && status.role == Role::Unknown {
                ActionObservation::Postcondition
            } else if !status.healthy && status.role == Role::Unknown {
                ActionObservation::Precondition
            } else {
                ActionObservation::Impossible
            }
        }
        DurableActionKind::UpdateCandidateEpoch => {
            if status.healthy && status.role == Role::Unknown && status.epoch == stable_epoch {
                ActionObservation::Postcondition
            } else if status.healthy
                && status.role == Role::Unknown
                && status.epoch == Epoch::default()
            {
                ActionObservation::Precondition
            } else {
                ActionObservation::Impossible
            }
        }
        DurableActionKind::AssignCandidateIdle => {
            if status.role == Role::IdleSecondary && status.epoch == stable_epoch {
                ActionObservation::Postcondition
            } else if status.role == Role::Unknown && status.epoch == stable_epoch {
                ActionObservation::Precondition
            } else {
                ActionObservation::Impossible
            }
        }
        DurableActionKind::BuildCandidate
        | DurableActionKind::RetireOldReplica
        | DurableActionKind::AddWaitForCatchUpQuorum
        | DurableActionKind::CompensateRemoveCandidate => ActionObservation::Precondition,
        DurableActionKind::AssignCandidateActive => {
            if status.role == Role::ActiveSecondary && status.epoch == stable_epoch {
                ActionObservation::Postcondition
            } else if status.role == Role::IdleSecondary && status.epoch == stable_epoch {
                ActionObservation::Precondition
            } else {
                ActionObservation::Impossible
            }
        }
        DurableActionKind::AddCatchUpConfiguration => {
            let expected = configuration_status_for_snapshot(
                &operation.target_snapshot,
                ReplicaConfigurationMode::CatchUp,
            );
            let previous = configuration_status_for_snapshot(
                &operation.previous_snapshot,
                ReplicaConfigurationMode::Current,
            );
            configuration_observation(status, &expected, Some(&previous), stable_epoch)
        }
        DurableActionKind::AddCurrentConfiguration => {
            let expected = configuration_status_for_snapshot(
                &operation.target_snapshot,
                ReplicaConfigurationMode::Current,
            );
            if status.configuration.as_ref() == Some(&expected)
                && status.role == Role::Primary
                && status.epoch == stable_epoch
            {
                ActionObservation::Postcondition
            } else {
                let catch_up = configuration_status_for_snapshot(
                    &operation.target_snapshot,
                    ReplicaConfigurationMode::CatchUp,
                );
                if status.configuration.as_ref() == Some(&catch_up)
                    && status.role == Role::Primary
                    && status.epoch == stable_epoch
                {
                    ActionObservation::Precondition
                } else {
                    ActionObservation::Impossible
                }
            }
        }
        DurableActionKind::LabelCandidateSecondary => {
            if status.role != Role::ActiveSecondary || status.epoch != stable_epoch {
                ActionObservation::Impossible
            } else if observation.pod_role_label.as_deref() == Some("secondary") {
                ActionObservation::Postcondition
            } else {
                ActionObservation::Precondition
            }
        }
        DurableActionKind::CompensateRestoreConfiguration => {
            let expected = configuration_status_for_snapshot(
                &operation.previous_snapshot,
                ReplicaConfigurationMode::Current,
            );
            let target = configuration_status_for_snapshot(
                &operation.target_snapshot,
                ReplicaConfigurationMode::CatchUp,
            );
            configuration_observation(status, &expected, Some(&target), stable_epoch)
        }
        DurableActionKind::CompensateDemoteCandidate => {
            if (status.role == Role::None && status.epoch == stable_epoch)
                || (!status.healthy && status.role == Role::Unknown)
            {
                ActionObservation::Postcondition
            } else if matches!(status.role, Role::IdleSecondary | Role::ActiveSecondary)
                && status.epoch == stable_epoch
            {
                ActionObservation::Precondition
            } else {
                ActionObservation::Impossible
            }
        }
        DurableActionKind::CompensateCloseCandidate => {
            if !status.healthy {
                ActionObservation::Postcondition
            } else if status.role == Role::None {
                ActionObservation::Precondition
            } else {
                ActionObservation::Impossible
            }
        }
        DurableActionKind::CompensateDeleteCandidate => unreachable!(),
        _ => return Err("non-add action reached add observation".to_string()),
    };
    if direct == ActionObservation::Postcondition {
        return Ok(direct);
    }

    if let Ok(expected_action) = action_for(operation, pending, observations) {
        let expected_signature = expected_action.signature();
        if let Some(active) = observation.status.durable_action.as_ref()
            && active.action_id == pending.action_id
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
        if let Some(completed) = observation.status.last_completed_action.as_ref()
            && completed.action_id == pending.action_id
        {
            return Ok(if completed.signature == expected_signature {
                ActionObservation::Postcondition
            } else {
                ActionObservation::Impossible
            });
        }
    }
    Ok(direct)
}

fn action_for(
    operation: &DurableOperationStatus,
    pending: &PendingActionStatus,
    observations: &OperationObservations,
) -> Result<DurableReplicaAction, String> {
    let stable_epoch = epoch(&operation.previous_snapshot.epoch);
    let target_id = target_replica_id(operation)?;
    Ok(match pending.kind {
        DurableActionKind::RetireOldReplica => DurableReplicaAction::RemoveReplica {
            replica_id: target_id,
            instance_id: ReplicaInstanceId::new(
                operation
                    .retired_instance_id
                    .clone()
                    .ok_or_else(|| "rebuild operation has no retired incarnation".to_string())?,
            ),
        },
        DurableActionKind::OpenCandidate => DurableReplicaAction::Open {
            mode: OpenMode::New,
        },
        DurableActionKind::UpdateCandidateEpoch => DurableReplicaAction::UpdateEpoch {
            epoch: stable_epoch,
        },
        DurableActionKind::AssignCandidateIdle => DurableReplicaAction::ChangeRole {
            epoch: stable_epoch,
            role: Role::IdleSecondary,
        },
        DurableActionKind::BuildCandidate => DurableReplicaAction::BuildReplica {
            replica: candidate_info(operation, observations, Role::IdleSecondary, false)?,
        },
        DurableActionKind::AssignCandidateActive => DurableReplicaAction::ChangeRole {
            epoch: stable_epoch,
            role: Role::ActiveSecondary,
        },
        DurableActionKind::AddCatchUpConfiguration => {
            DurableReplicaAction::UpdateCatchUpConfiguration {
                current: target_config(operation, observations, true)?,
                previous: previous_config(operation, observations)?,
            }
        }
        DurableActionKind::AddWaitForCatchUpQuorum => DurableReplicaAction::WaitForCatchUpQuorum {
            mode: ReplicaSetQuorumMode::Write,
        },
        DurableActionKind::AddCurrentConfiguration => {
            DurableReplicaAction::UpdateCurrentConfiguration {
                current: target_config(operation, observations, false)?,
            }
        }
        DurableActionKind::CompensateRestoreConfiguration => {
            DurableReplicaAction::UpdateCurrentConfiguration {
                current: previous_config(operation, observations)?,
            }
        }
        DurableActionKind::CompensateRemoveCandidate => DurableReplicaAction::RemoveReplica {
            replica_id: target_id,
            instance_id: ReplicaInstanceId::new(target_instance_id(operation)?.to_string()),
        },
        DurableActionKind::CompensateDemoteCandidate => DurableReplicaAction::ChangeRole {
            epoch: stable_epoch,
            role: Role::None,
        },
        DurableActionKind::CompensateCloseCandidate => DurableReplicaAction::Close,
        DurableActionKind::LabelCandidateSecondary
        | DurableActionKind::CompensateDeleteCandidate => {
            return Err("cluster action does not map to a runtime action".to_string());
        }
        _ => return Err("non-add action reached add action construction".to_string()),
    })
}

fn pending_action(
    operation: &DurableOperationStatus,
    sequence: u32,
    kind: DurableActionKind,
    now: i64,
) -> Result<PendingActionStatus, String> {
    let primary_id = operation.previous_snapshot.primary_id;
    let target_id = target_replica_id(operation)?;
    let runtime_target_id = match kind {
        DurableActionKind::RetireOldReplica
        | DurableActionKind::BuildCandidate
        | DurableActionKind::AddCatchUpConfiguration
        | DurableActionKind::AddWaitForCatchUpQuorum
        | DurableActionKind::AddCurrentConfiguration
        | DurableActionKind::CompensateRestoreConfiguration
        | DurableActionKind::CompensateRemoveCandidate => primary_id,
        _ => target_id,
    };
    let runtime_instance_id = if runtime_target_id == primary_id {
        operation
            .previous_snapshot
            .members
            .iter()
            .find(|member| member.id == primary_id)
            .map(|member| member.instance_id.clone())
            .ok_or_else(|| "stable primary is absent".to_string())?
    } else {
        target_instance_id(operation)?.to_string()
    };
    let postcondition = match kind {
        DurableActionKind::RetireOldReplica | DurableActionKind::CompensateRemoveCandidate => {
            DurablePostconditionKind::ReplicaRemoved
        }
        DurableActionKind::OpenCandidate => DurablePostconditionKind::Opened,
        DurableActionKind::UpdateCandidateEpoch => DurablePostconditionKind::Epoch,
        DurableActionKind::AssignCandidateIdle
        | DurableActionKind::AssignCandidateActive
        | DurableActionKind::CompensateDemoteCandidate => DurablePostconditionKind::Role,
        DurableActionKind::BuildCandidate => DurablePostconditionKind::BuildCompleted,
        DurableActionKind::AddCatchUpConfiguration => {
            DurablePostconditionKind::CatchUpConfiguration
        }
        DurableActionKind::AddWaitForCatchUpQuorum => DurablePostconditionKind::CatchUpQuorum,
        DurableActionKind::AddCurrentConfiguration
        | DurableActionKind::CompensateRestoreConfiguration => {
            DurablePostconditionKind::CurrentConfiguration
        }
        DurableActionKind::LabelCandidateSecondary => DurablePostconditionKind::PodRoleLabel,
        DurableActionKind::CompensateCloseCandidate => DurablePostconditionKind::Closed,
        DurableActionKind::CompensateDeleteCandidate => DurablePostconditionKind::PodDeleted,
        _ => return Err("non-add action cannot be scheduled by add protocol".to_string()),
    };
    let role = match kind {
        DurableActionKind::AssignCandidateIdle => None,
        DurableActionKind::AssignCandidateActive | DurableActionKind::LabelCandidateSecondary => {
            Some(StableReplicaRoleStatus::ActiveSecondary)
        }
        _ => None,
    };
    Ok(PendingActionStatus {
        action_id: format!("{}:{sequence}:{kind:?}", operation.execution_id),
        sequence,
        kind,
        target_id: runtime_target_id,
        target_instance_id: runtime_instance_id,
        expected_epoch: operation.previous_snapshot.epoch.clone(),
        desired_postcondition: DurablePostconditionStatus {
            kind: postcondition,
            role,
        },
        attempts: 0,
        deadline_unix_seconds: now
            + if kind == DurableActionKind::BuildCandidate {
                BUILD_DEADLINE_SECONDS
            } else {
                ACTION_DEADLINE_SECONDS
            },
        last_error: None,
        dispatch_authorized: false,
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
    let expected = pending_action(operation, pending.sequence, pending.kind, 0)?;
    if pending.action_id != expected.action_id
        || pending.target_id != expected.target_id
        || pending.target_instance_id != expected.target_instance_id
        || pending.expected_epoch != expected.expected_epoch
        || pending.desired_postcondition != expected.desired_postcondition
    {
        return Err("pending add action is not deterministic".to_string());
    }
    Ok(())
}

fn action_allowed_in_phase(phase: DurableOperationPhase, kind: DurableActionKind) -> bool {
    matches!(
        (phase, kind),
        (
            DurableOperationPhase::RetireOldReplica,
            DurableActionKind::RetireOldReplica
        ) | (
            DurableOperationPhase::OpenCandidate,
            DurableActionKind::OpenCandidate
        ) | (
            DurableOperationPhase::UpdateCandidateEpoch,
            DurableActionKind::UpdateCandidateEpoch
        ) | (
            DurableOperationPhase::AssignCandidateIdle,
            DurableActionKind::AssignCandidateIdle
        ) | (
            DurableOperationPhase::BuildCandidate,
            DurableActionKind::BuildCandidate
        ) | (
            DurableOperationPhase::AssignCandidateActive,
            DurableActionKind::AssignCandidateActive
        ) | (
            DurableOperationPhase::AddCatchUpConfiguration,
            DurableActionKind::AddCatchUpConfiguration
        ) | (
            DurableOperationPhase::AddWaitForCatchUpQuorum,
            DurableActionKind::AddWaitForCatchUpQuorum
        ) | (
            DurableOperationPhase::AddCurrentConfiguration,
            DurableActionKind::AddCurrentConfiguration
        ) | (
            DurableOperationPhase::LabelCandidateSecondary,
            DurableActionKind::LabelCandidateSecondary
        ) | (
            DurableOperationPhase::CompensateRestoreConfiguration,
            DurableActionKind::CompensateRestoreConfiguration
        ) | (
            DurableOperationPhase::CompensateRemoveCandidate,
            DurableActionKind::CompensateRemoveCandidate
        ) | (
            DurableOperationPhase::CompensateDemoteCandidate,
            DurableActionKind::CompensateDemoteCandidate
        ) | (
            DurableOperationPhase::CompensateCloseCandidate,
            DurableActionKind::CompensateCloseCandidate
        ) | (
            DurableOperationPhase::CompensateDeleteCandidate,
            DurableActionKind::CompensateDeleteCandidate
        )
    )
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
        DurableOperationPhase::AddCatchUpConfiguration
        | DurableOperationPhase::AddWaitForCatchUpQuorum
        | DurableOperationPhase::AddCurrentConfiguration => {
            DurableOperationPhase::CompensateRestoreConfiguration
        }
        DurableOperationPhase::CompensateRestoreConfiguration
        | DurableOperationPhase::CompensateRemoveCandidate
        | DurableOperationPhase::CompensateDemoteCandidate
        | DurableOperationPhase::CompensateCloseCandidate
        | DurableOperationPhase::CompensateDeleteCandidate => DurableOperationPhase::Poisoned,
        DurableOperationPhase::LabelCandidateSecondary | DurableOperationPhase::AddFinalize => {
            DurableOperationPhase::AddFinalize
        }
        _ => DurableOperationPhase::CompensateRemoveCandidate,
    };
    next
}

fn persist_pending(operation: &DurableOperationStatus, pending: PendingActionStatus) -> Decision {
    let mut next = operation.clone();
    next.pending_action = Some(pending);
    Decision::Persist(next)
}

fn target_replica_id(operation: &DurableOperationStatus) -> Result<i64, String> {
    operation
        .target_replica_id
        .ok_or_else(|| "add operation has no target replica ID".to_string())
}

fn target_instance_id(operation: &DurableOperationStatus) -> Result<&str, String> {
    operation
        .target_instance_id
        .as_deref()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "add operation has no target incarnation".to_string())
}

fn target_pod_name(operation: &DurableOperationStatus) -> Result<&str, String> {
    operation
        .target_pod_name
        .as_deref()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "add operation has no target pod".to_string())
}

fn epoch(value: &EpochStatus) -> Epoch {
    Epoch::new(value.data_loss_number, value.configuration_number)
}

fn candidate_info(
    operation: &DurableOperationStatus,
    observations: &OperationObservations,
    role: Role,
    must_catch_up: bool,
) -> Result<ReplicaInfo, String> {
    let target_id = target_replica_id(operation)?;
    let observation = observations
        .get(&target_id)
        .ok_or_else(|| format!("candidate replica {target_id} is unavailable"))?;
    if observation.status.instance_id.as_str() != target_instance_id(operation)? {
        return Err("candidate incarnation changed".to_string());
    }
    Ok(ReplicaInfo {
        id: target_id,
        instance_id: observation.status.instance_id.clone(),
        role,
        status: ReplicaStatus::Up,
        replicator_address: observation.replicator_address.clone(),
        current_progress: observation.status.current_progress,
        catch_up_capability: observation.status.current_progress,
        must_catch_up,
    })
}

fn config_for_snapshot(
    snapshot: &StablePartitionSnapshotStatus,
    operation: &DurableOperationStatus,
    observations: &OperationObservations,
    must_catch_up_target: bool,
) -> Result<ReplicaSetConfig, String> {
    let target_id = target_replica_id(operation)?;
    let mut members = Vec::new();
    for member in &snapshot.members {
        if member.id == snapshot.primary_id {
            continue;
        }
        let observation = observations
            .get(&member.id)
            .ok_or_else(|| format!("replica {} is unavailable for configuration", member.id))?;
        members.push(ReplicaInfo {
            id: member.id,
            instance_id: ReplicaInstanceId::new(member.instance_id.clone()),
            role: Role::ActiveSecondary,
            status: ReplicaStatus::Up,
            replicator_address: observation.replicator_address.clone(),
            current_progress: observation.status.current_progress,
            catch_up_capability: observation.status.current_progress,
            must_catch_up: must_catch_up_target && member.id == target_id,
        });
    }
    Ok(ReplicaSetConfig {
        members,
        write_quorum: snapshot.write_quorum,
    })
}

fn previous_config(
    operation: &DurableOperationStatus,
    observations: &OperationObservations,
) -> Result<ReplicaSetConfig, String> {
    config_for_snapshot(&operation.previous_snapshot, operation, observations, false)
}

fn target_config(
    operation: &DurableOperationStatus,
    observations: &OperationObservations,
    must_catch_up: bool,
) -> Result<ReplicaSetConfig, String> {
    config_for_snapshot(
        &operation.target_snapshot,
        operation,
        observations,
        must_catch_up,
    )
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

fn validate_target_observation(
    operation: &DurableOperationStatus,
    observations: &OperationObservations,
) -> Result<(), String> {
    let expected_epoch = epoch(&operation.target_snapshot.epoch);
    for member in &operation.target_snapshot.members {
        let observation = observations
            .get(&member.id)
            .ok_or_else(|| format!("target member {} is unavailable", member.id))?;
        if observation.status.instance_id.as_str() != member.instance_id {
            return Err(format!("target member {} incarnation changed", member.id));
        }
        let expected_role = if member.id == operation.target_snapshot.primary_id {
            Role::Primary
        } else {
            Role::ActiveSecondary
        };
        if observation.status.role != expected_role || observation.status.epoch != expected_epoch {
            return Err(format!(
                "target member {} role or epoch is not committed",
                member.id
            ));
        }
    }
    let primary = observations
        .get(&operation.target_snapshot.primary_id)
        .ok_or_else(|| "target primary is unavailable".to_string())?;
    let expected = configuration_status_for_snapshot(
        &operation.target_snapshot,
        ReplicaConfigurationMode::Current,
    );
    if primary.status.configuration.as_ref() != Some(&expected) {
        return Err("target current configuration is not committed".to_string());
    }
    Ok(())
}

fn validate_operation(operation: &DurableOperationStatus) -> Result<(), String> {
    if operation.version != DURABLE_OPERATION_VERSION {
        return Err(format!(
            "unsupported durable operation version {}",
            operation.version
        ));
    }
    if operation.kind != DurableOperationKind::AddReplica {
        return Err("operation kind is not add-replica".to_string());
    }
    if operation.previous_snapshot.is_none() {
        return Err("add operation has no previous stable snapshot".to_string());
    }
    validate_snapshot(&operation.previous_snapshot)?;
    validate_snapshot(&operation.target_snapshot)?;
    let mode = operation
        .add_mode
        .ok_or_else(|| "add operation has no mode".to_string())?;
    let target_id = target_replica_id(operation)?;
    let target_instance = target_instance_id(operation)?;
    let target_member = operation
        .target_snapshot
        .members
        .iter()
        .find(|member| member.id == target_id)
        .ok_or_else(|| "target snapshot has no candidate member".to_string())?;
    if target_member.instance_id != target_instance
        || target_member.role != StableReplicaRoleStatus::ActiveSecondary
    {
        return Err("target snapshot candidate identity or role is invalid".to_string());
    }
    if operation.previous_snapshot.primary_id != operation.old_primary_id
        || operation.target_snapshot.primary_id != operation.target_primary_id
        || operation.old_primary_id != operation.target_primary_id
    {
        return Err("add operation primary identity is inconsistent".to_string());
    }
    if operation.previous_snapshot.epoch != operation.target_snapshot.epoch {
        return Err("add operation must not change the stable epoch".to_string());
    }
    target_pod_name(operation)?;
    match mode {
        DurableAddMode::ScaleUp => {
            if operation.retired_instance_id.is_some()
                || operation
                    .previous_snapshot
                    .members
                    .iter()
                    .any(|member| member.id == target_id)
            {
                return Err("scale-up operation contains a committed target".to_string());
            }
            if operation.target_snapshot.members.len()
                != operation.previous_snapshot.members.len() + 1
            {
                return Err("scale-up target snapshot has unexpected membership".to_string());
            }
        }
        DurableAddMode::Rebuild => {
            let previous = operation
                .previous_snapshot
                .members
                .iter()
                .find(|member| member.id == target_id)
                .ok_or_else(|| "rebuild previous snapshot has no target".to_string())?;
            if operation.retired_instance_id.as_deref() != Some(previous.instance_id.as_str())
                || previous.instance_id == target_instance
            {
                return Err("rebuild incarnation transition is invalid".to_string());
            }
            if operation.target_snapshot.members.len() != operation.previous_snapshot.members.len()
            {
                return Err("rebuild target snapshot has unexpected membership".to_string());
            }
        }
    }
    for previous in &operation.previous_snapshot.members {
        if previous.id == target_id && mode == DurableAddMode::Rebuild {
            continue;
        }
        if !operation
            .target_snapshot
            .members
            .iter()
            .any(|target| target == previous)
        {
            return Err(format!(
                "target snapshot changed retained replica {}",
                previous.id
            ));
        }
    }
    Ok(())
}

fn validate_retained_observations(
    operation: &DurableOperationStatus,
    observations: &OperationObservations,
) -> Result<(), String> {
    let stable_epoch = epoch(&operation.previous_snapshot.epoch);
    let target_id = target_replica_id(operation)?;
    for member in &operation.previous_snapshot.members {
        if operation.add_mode == Some(DurableAddMode::Rebuild) && member.id == target_id {
            continue;
        }
        let observation = observations
            .get(&member.id)
            .ok_or_else(|| format!("retained replica {} is unavailable", member.id))?;
        if observation.status.instance_id.as_str() != member.instance_id {
            return Err(format!(
                "retained replica {} incarnation changed",
                member.id
            ));
        }
        let expected_role = if member.id == operation.previous_snapshot.primary_id {
            Role::Primary
        } else {
            Role::ActiveSecondary
        };
        if observation.status.role != expected_role || observation.status.epoch != stable_epoch {
            return Err(format!(
                "retained replica {} has an unexpected role or epoch",
                member.id
            ));
        }
    }

    let primary = observations
        .get(&operation.previous_snapshot.primary_id)
        .ok_or_else(|| "stable primary is unavailable".to_string())?;
    if primary.status.write_status != kuberic_core::types::AccessStatus::Granted {
        return Err("stable primary does not have write access".to_string());
    }
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
        DurableOperationPhase::RetireOldReplica
        | DurableOperationPhase::OpenCandidate
        | DurableOperationPhase::UpdateCandidateEpoch
        | DurableOperationPhase::AssignCandidateIdle
        | DurableOperationPhase::BuildCandidate
        | DurableOperationPhase::AssignCandidateActive => observed == Some(&previous),
        DurableOperationPhase::AddCatchUpConfiguration => {
            observed == Some(&previous) || observed == Some(&target_catch_up)
        }
        DurableOperationPhase::AddWaitForCatchUpQuorum => observed == Some(&target_catch_up),
        DurableOperationPhase::AddCurrentConfiguration => {
            observed == Some(&target_catch_up) || observed == Some(&target_current)
        }
        DurableOperationPhase::LabelCandidateSecondary | DurableOperationPhase::AddFinalize => {
            observed == Some(&target_current)
        }
        DurableOperationPhase::CompensateRestoreConfiguration => {
            observed == Some(&target_catch_up) || observed == Some(&previous)
        }
        DurableOperationPhase::CompensateRemoveCandidate
        | DurableOperationPhase::CompensateDemoteCandidate
        | DurableOperationPhase::CompensateCloseCandidate
        | DurableOperationPhase::CompensateDeleteCandidate
        | DurableOperationPhase::AddCompensateFinalize => observed == Some(&previous),
        _ => true,
    };
    if !valid {
        return Err("primary configuration is unexpected for add phase".to_string());
    }
    Ok(())
}

fn configuration_status_for_snapshot(
    snapshot: &StablePartitionSnapshotStatus,
    mode: ReplicaConfigurationMode,
) -> ReplicaConfigurationStatus {
    let mut members = snapshot
        .members
        .iter()
        .filter(|member| member.id != snapshot.primary_id)
        .map(
            |member| kuberic_core::types::ReplicaConfigurationMemberStatus {
                id: member.id,
                instance_id: ReplicaInstanceId::new(member.instance_id.clone()),
                role: Role::ActiveSecondary,
            },
        )
        .collect::<Vec<_>>();
    members.sort_by_key(|member| member.id);
    ReplicaConfigurationStatus {
        mode,
        members,
        write_quorum: snapshot.write_quorum,
    }
}

fn validate_snapshot(snapshot: &StablePartitionSnapshotStatus) -> Result<(), String> {
    if snapshot.members.is_empty() {
        return Err("stable snapshot has no members".to_string());
    }
    let mut ids = HashSet::new();
    let mut incarnations = HashSet::new();
    let mut primary_count = 0;
    for member in &snapshot.members {
        if member.instance_id.is_empty()
            || !ids.insert(member.id)
            || !incarnations.insert(member.instance_id.as_str())
        {
            return Err("stable snapshot has empty or duplicate identity".to_string());
        }
        if member.role == StableReplicaRoleStatus::Primary {
            primary_count += 1;
            if member.id != snapshot.primary_id {
                return Err("stable snapshot primary role disagrees with primary ID".to_string());
            }
        }
    }
    if primary_count != 1 {
        return Err("stable snapshot must contain exactly one primary".to_string());
    }
    let expected_quorum = snapshot.members.len() as u32 / 2 + 1;
    if snapshot.write_quorum != expected_quorum {
        return Err("stable snapshot write quorum is not a majority".to_string());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::durable::ReplicaObservation;
    use kuberic_core::types::{AccessStatus, DurableActionObservation};

    fn snapshot() -> StablePartitionSnapshotStatus {
        StablePartitionSnapshotStatus {
            epoch: EpochStatus {
                data_loss_number: 0,
                configuration_number: 4,
            },
            primary_id: 1,
            members: vec![
                StableReplicaSnapshotStatus {
                    id: 1,
                    instance_id: "primary".to_string(),
                    role: StableReplicaRoleStatus::Primary,
                    election_metadata: None,
                },
                StableReplicaSnapshotStatus {
                    id: 2,
                    instance_id: "secondary".to_string(),
                    role: StableReplicaRoleStatus::ActiveSecondary,
                    election_metadata: None,
                },
            ],
            write_quorum: 2,
        }
    }

    fn observation(
        instance_id: &str,
        role: Role,
        epoch: Epoch,
        configuration: Option<ReplicaConfigurationStatus>,
    ) -> ReplicaObservation {
        ReplicaObservation {
            status: kuberic_core::types::ReplicaStatusInfo {
                instance_id: ReplicaInstanceId::new(instance_id),
                role,
                epoch,
                current_progress: 10,
                catch_up_capability: Some(10),
                committed_lsn: 10,
                healthy: role != Role::Unknown,
                write_status: if role == Role::Primary {
                    AccessStatus::Granted
                } else {
                    AccessStatus::NotPrimary
                },
                configuration,
                election_configuration: None,
                deactivation_info: None,
                last_completed_action: None,
                durable_action: None,
                active_replica_connections: Vec::new(),
            },
            replicator_address: format!("http://{instance_id}"),
            pod_name: instance_id.to_string(),
            pod_role_label: None,
        }
    }

    #[test]
    fn start_add_validates_mode_and_identity() {
        let previous = snapshot();
        let scale = start_add_replica(
            "set",
            previous.clone(),
            3,
            "candidate".to_string(),
            "pod-2".to_string(),
            DurableAddMode::ScaleUp,
            10,
        )
        .unwrap();
        assert_eq!(scale.phase, DurableOperationPhase::OpenCandidate);
        assert_eq!(scale.target_snapshot.members.len(), 3);
        assert_eq!(scale.target_snapshot.write_quorum, 2);

        let rebuild = start_add_replica(
            "set",
            previous.clone(),
            2,
            "replacement".to_string(),
            "pod-1".to_string(),
            DurableAddMode::Rebuild,
            10,
        )
        .unwrap();
        assert_eq!(rebuild.phase, DurableOperationPhase::RetireOldReplica);
        assert_eq!(rebuild.retired_instance_id.as_deref(), Some("secondary"));

        assert!(
            start_add_replica(
                "set",
                previous.clone(),
                2,
                "secondary".to_string(),
                "pod-1".to_string(),
                DurableAddMode::Rebuild,
                10,
            )
            .is_err()
        );
        assert!(
            start_add_replica(
                "set",
                previous,
                3,
                "primary".to_string(),
                "pod-2".to_string(),
                DurableAddMode::ScaleUp,
                10,
            )
            .is_err()
        );
    }

    #[test]
    fn build_in_progress_waits_without_redispatch() {
        let mut operation = start_add_replica(
            "set",
            snapshot(),
            3,
            "candidate".to_string(),
            "pod-2".to_string(),
            DurableAddMode::ScaleUp,
            10,
        )
        .unwrap();
        operation.phase = DurableOperationPhase::BuildCandidate;
        let pending = pending_action(&operation, 5, DurableActionKind::BuildCandidate, 10).unwrap();
        operation.pending_action = Some(pending.clone());

        let epoch = epoch(&operation.previous_snapshot.epoch);
        let mut observations = OperationObservations::new();
        observations.insert(1, observation("primary", Role::Primary, epoch, None));
        observations.insert(
            2,
            observation("secondary", Role::ActiveSecondary, epoch, None),
        );
        observations.insert(
            3,
            observation("candidate", Role::IdleSecondary, epoch, None),
        );
        let previous = configuration_status_for_snapshot(
            &operation.previous_snapshot,
            ReplicaConfigurationMode::Current,
        );
        observations.get_mut(&1).unwrap().status.configuration = Some(previous);
        let signature = action_for(&operation, &pending, &observations)
            .unwrap()
            .signature();
        observations.get_mut(&1).unwrap().status.durable_action = Some(DurableActionObservation {
            action_id: pending.action_id,
            signature,
            state: DurableActionState::InProgress,
            error: None,
            result: None,
        });

        assert!(matches!(
            decide_add_replica(&operation, &observations, 20).unwrap(),
            Decision::Wait
        ));
    }

    #[test]
    fn committed_current_configuration_rolls_forward_after_failed_reply() {
        let mut operation = start_add_replica(
            "set",
            snapshot(),
            3,
            "candidate".to_string(),
            "pod-2".to_string(),
            DurableAddMode::ScaleUp,
            10,
        )
        .unwrap();
        operation.phase = DurableOperationPhase::AddCurrentConfiguration;
        let pending = pending_action(
            &operation,
            9,
            DurableActionKind::AddCurrentConfiguration,
            10,
        )
        .unwrap();
        operation.pending_action = Some(pending.clone());

        let stable_epoch = epoch(&operation.previous_snapshot.epoch);
        let mut observations = OperationObservations::new();
        observations.insert(1, observation("primary", Role::Primary, stable_epoch, None));
        observations.insert(
            2,
            observation("secondary", Role::ActiveSecondary, stable_epoch, None),
        );
        observations.insert(
            3,
            observation("candidate", Role::ActiveSecondary, stable_epoch, None),
        );
        let current = ReplicaConfigurationStatus::from_config(
            ReplicaConfigurationMode::Current,
            &target_config(&operation, &observations, false).unwrap(),
        );
        let failed_signature = action_for(&operation, &pending, &observations)
            .unwrap()
            .signature();
        let primary = observations.get_mut(&1).unwrap();
        primary.status.configuration = Some(current);
        primary.status.durable_action = Some(DurableActionObservation {
            action_id: pending.action_id,
            signature: failed_signature,
            state: DurableActionState::Failed,
            error: Some("lost reply".to_string()),
            result: None,
        });

        let Decision::Persist(next) = decide_add_replica(&operation, &observations, 20).unwrap()
        else {
            panic!("committed current configuration must advance");
        };
        assert_eq!(next.phase, DurableOperationPhase::LabelCandidateSecondary);
    }

    #[test]
    fn invalid_version_and_illegal_pending_action_are_rejected() {
        let mut operation = start_add_replica(
            "set",
            snapshot(),
            3,
            "candidate".to_string(),
            "pod-2".to_string(),
            DurableAddMode::ScaleUp,
            10,
        )
        .unwrap();
        operation.version += 1;
        assert!(
            decide_add_replica(&operation, &OperationObservations::new(), 10)
                .unwrap_err()
                .contains("unsupported")
        );

        operation.version = DURABLE_OPERATION_VERSION;
        operation.pending_action =
            Some(pending_action(&operation, 5, DurableActionKind::BuildCandidate, 10).unwrap());
        assert!(
            decide_add_replica(&operation, &OperationObservations::new(), 10)
                .unwrap_err()
                .contains("illegal")
        );
    }

    #[test]
    fn unexpected_retained_configuration_poisoned_without_execution() {
        let operation = start_add_replica(
            "set",
            snapshot(),
            3,
            "candidate".to_string(),
            "pod-2".to_string(),
            DurableAddMode::ScaleUp,
            10,
        )
        .unwrap();
        let stable_epoch = epoch(&operation.previous_snapshot.epoch);
        let mut observations = OperationObservations::new();
        observations.insert(1, observation("primary", Role::Primary, stable_epoch, None));
        observations.insert(
            2,
            observation("secondary", Role::ActiveSecondary, stable_epoch, None),
        );
        observations.insert(
            3,
            observation("candidate", Role::Unknown, Epoch::default(), None),
        );

        let Decision::Persist(next) = decide_add_replica(&operation, &observations, 10).unwrap()
        else {
            panic!("unexpected retained configuration must not execute");
        };
        assert_eq!(next.phase, DurableOperationPhase::Poisoned);
    }
}
