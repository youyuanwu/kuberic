use std::collections::HashSet;

use kuberic_core::add_replica::{
    AddReplicaIntent, AddReplicaMode, AddReplicaTerminalResult, RuntimeBuildState,
};
use kuberic_core::replica_lifecycle::{
    ConfigurationDescriptor, ConfigurationMemberDescriptor, ConfigurationProgressSource,
    REPLICA_LIFECYCLE_PEER_PROTOCOL_VERSION,
};
use kuberic_core::types::{
    DurableActionResult, DurableActionState, DurableReplicaAction, Epoch, ReplicaConfigurationMode,
    ReplicaInstanceId, ReplicaStatus, Role,
};

use crate::crd::{
    ADD_REPLICA_OPERATION_VERSION, AddReplicaIntentStatus, ConfigurationDescriptorStatus,
    ConfigurationMemberDescriptorStatus, ConfigurationProgressSourceStatus, DurableActionKind,
    DurableAddMode, DurableOperationKind, DurableOperationPhase, DurableOperationStatus,
    DurablePostconditionKind, DurablePostconditionStatus, EpochStatus, PendingActionStatus,
    StablePartitionSnapshotStatus, StableReplicaRoleStatus, StableReplicaSnapshotStatus,
};

use super::{
    ACTION_DEADLINE_SECONDS, Decision, OperationObservations, OperationPodIdentities,
    correlated_action_observation, poison,
};

const ADD_DEADLINE_SECONDS: i64 = 600;
const COMPENSATION_GRACE_SECONDS: i64 = 30;

#[allow(clippy::too_many_arguments)]
pub fn start_add_replica(
    set_uid: &str,
    previous: StablePartitionSnapshotStatus,
    target_replica_id: i64,
    target_instance_id: String,
    target_pod_name: String,
    mode: DurableAddMode,
    minimum_committed_replicas: usize,
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
            member.election_metadata = None;
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
    if target.members.len() < minimum_committed_replicas.max(1) {
        return Err("target topology is below minReplicas".to_string());
    }

    let operation_id = format!(
        "{set_uid}:add-replica:v{ADD_REPLICA_OPERATION_VERSION}:{}-{}:{}@{}",
        previous.epoch.data_loss_number,
        previous.epoch.configuration_number,
        target_replica_id,
        target_instance_id
    );
    Ok(DurableOperationStatus {
        execution_id: format!("{operation_id}:execution-1"),
        operation_id,
        version: ADD_REPLICA_OPERATION_VERSION,
        kind: DurableOperationKind::AddReplica,
        phase: DurableOperationPhase::AddFreezeIntent,
        old_primary_id: previous.primary_id,
        target_primary_id: previous.primary_id,
        add_mode: Some(mode),
        remove_mode: None,
        target_replica_id: Some(target_replica_id),
        target_instance_id: Some(target_instance_id.clone()),
        target_pod_name: Some(target_pod_name),
        target_pod_uid: None,
        remove_target_replicator_address: None,
        remove_target_agent_generation: None,
        retired_instance_id,
        previous_snapshot: previous.into(),
        target_snapshot: target,
        committed_snapshot: None,
        minimum_committed_replicas: Some(minimum_committed_replicas.max(1) as u32),
        frozen_lsn: None,
        next_secondary_index: 0,
        phase_deadline_unix_seconds: now + ADD_DEADLINE_SECONDS,
        pending_action: None,
        last_error: None,
        failover: None,
        add_intent: None,
        remove_intent: None,
        remove_commit_evidence: None,
        remove_cleanup: None,
        removal_disposition: None,
    })
}

pub fn decide_add_replica(
    operation: &DurableOperationStatus,
    observations: &OperationObservations,
    pod_identities: &OperationPodIdentities,
    target_pod_role_label: Option<&str>,
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

    match operation.phase {
        DurableOperationPhase::AddCommittedDegradedHandoff => {
            let mut completed = operation.clone();
            completed.phase = DurableOperationPhase::Failed;
            return Ok(Decision::CompleteDegraded {
                operation: completed,
                snapshot: operation
                    .committed_snapshot
                    .clone()
                    .ok_or_else(|| "degraded handoff has no committed snapshot".to_string())?,
            });
        }
        DurableOperationPhase::AddDeleteCompensatedTarget => {
            let target_id = target_replica_id(operation)?;
            if pod_identities.get(&target_id).map(String::as_str)
                != Some(target_instance_id(operation)?)
            {
                let mut next = operation.clone();
                next.phase = DurableOperationPhase::AddCompensateFinalize;
                return Ok(Decision::Persist(next));
            }
            return Ok(Decision::DeletePod {
                pod_name: target_pod_name(operation)?.to_string(),
                expected_uid: target_instance_id(operation)?.to_string(),
            });
        }
        DurableOperationPhase::AddCompensateFinalize => {
            let mut failed = operation.clone();
            failed.phase = DurableOperationPhase::Failed;
            return Ok(Decision::Complete {
                operation: failed,
                snapshot: operation.previous_snapshot.cloned().unwrap(),
                compensated: true,
            });
        }
        _ => {}
    }

    let primary = observations.get(&operation.old_primary_id);
    let target_id = target_replica_id(operation)?;
    let target = observations.get(&target_id);

    if primary.is_none() {
        if operation.committed_snapshot.is_some() {
            return committed_degraded_decision(operation, target_pod_role_label, now);
        }
        let expected_primary_instance = operation
            .previous_snapshot
            .members
            .iter()
            .find(|member| member.id == operation.old_primary_id)
            .map(|member| member.instance_id.as_str())
            .ok_or_else(|| "stable primary member is missing".to_string())?;
        if pod_identities
            .get(&operation.old_primary_id)
            .map(String::as_str)
            == Some(expected_primary_instance)
        {
            return if now >= operation.phase_deadline_unix_seconds {
                Ok(Decision::Persist(poison(
                    operation,
                    "primary status is unavailable; commit state is ambiguous and target is preserved",
                )))
            } else {
                Ok(Decision::Wait)
            };
        }
        let safely_uncommitted = target.is_some_and(|target| {
            let identity_matches = target.status.instance_id.as_str()
                == target_instance_id(operation).unwrap_or_default();
            if !identity_matches {
                return false;
            }
            match operation.add_intent.as_ref() {
                None => {
                    !target.status.healthy
                        && target.status.role == Role::Unknown
                        && target.status.epoch == Epoch::default()
                }
                Some(intent) => {
                    target.status.agent.generation.to_string() == intent.target_agent_generation
                        && ((target.status.role == Role::Unknown
                            && target.status.epoch == Epoch::default()
                            && !target.status.healthy)
                            || (target.status.epoch == epoch(&operation.previous_snapshot.epoch)
                                && matches!(target.status.role, Role::IdleSecondary | Role::None)))
                }
            }
        });
        if safely_uncommitted {
            let mut next = operation.clone();
            next.phase = DurableOperationPhase::AddDeleteCompensatedTarget;
            next.pending_action = None;
            next.last_error = Some("primary was lost before target activation".to_string());
            return Ok(Decision::Persist(next));
        }
        return Ok(Decision::Persist(poison(
            operation,
            "primary loss after target activation is commit-ambiguous; target preserved",
        )));
    }

    if primary_current_configuration_matches(operation, primary.unwrap())?
        && operation.committed_snapshot.is_none()
    {
        let mut next = operation.clone();
        next.committed_snapshot = Some(operation.target_snapshot.clone());
        next.pending_action = None;
        next.phase = DurableOperationPhase::AddRecordCommit;
        return Ok(Decision::Persist(next));
    }

    let expected_primary = operation
        .previous_snapshot
        .members
        .iter()
        .find(|member| member.id == operation.old_primary_id)
        .ok_or_else(|| "stable primary member is missing".to_string())?;
    let expected_epoch = epoch(&operation.previous_snapshot.epoch);
    if primary.is_some_and(|primary| {
        primary.status.instance_id.as_str() != expected_primary.instance_id
            || primary.status.role != Role::Primary
            || primary.status.epoch != expected_epoch
    }) {
        if operation.committed_snapshot.is_some() {
            return committed_degraded_decision(operation, target_pod_role_label, now);
        }
        return if now >= operation.phase_deadline_unix_seconds {
            Ok(Decision::Persist(poison(
                operation,
                "primary runtime continuity was lost while add intent was active",
            )))
        } else {
            Ok(Decision::Wait)
        };
    }

    match operation.phase {
        DurableOperationPhase::AddFreezeIntent => match freeze_intent(operation, observations, now)
        {
            Ok(decision) => Ok(decision),
            Err(error) if now >= operation.phase_deadline_unix_seconds => {
                Ok(Decision::Persist(poison(operation, &error)))
            }
            Err(_) => Ok(Decision::Wait),
        },
        DurableOperationPhase::AddDispatchIntent => {
            let intent = operation
                .add_intent
                .as_ref()
                .ok_or_else(|| "add dispatch phase has no frozen intent".to_string())?;
            let mut next = operation.clone();
            next.phase = DurableOperationPhase::AddAwaitCoordination;
            next.pending_action = Some(PendingActionStatus {
                action_id: intent.action_id.clone(),
                sequence: intent.attempt,
                kind: DurableActionKind::AddReplicaIntent,
                target_id: operation.old_primary_id,
                target_instance_id: intent.primary_instance_id.clone(),
                expected_epoch: operation.previous_snapshot.epoch.clone(),
                desired_postcondition: DurablePostconditionStatus {
                    kind: DurablePostconditionKind::AddReplicaCoordinated,
                    role: None,
                },
                attempts: 0,
                deadline_unix_seconds: intent.deadline_unix_seconds,
                last_error: None,
                dispatch_authorized: false,
                dispatch_agent_generation: None,
                dispatch_agent_control_version: None,
                dispatch_observed_runtime_epoch: None,
                dispatch_action_payload: String::new(),
            });
            Ok(Decision::Persist(next))
        }
        DurableOperationPhase::AddAwaitCoordination => {
            decide_await_coordination(operation, observations, now)
        }
        DurableOperationPhase::AddRecordCommit => {
            let mut next = operation.clone();
            next.phase = DurableOperationPhase::AddPublishTarget;
            next.phase_deadline_unix_seconds = now + ACTION_DEADLINE_SECONDS;
            Ok(Decision::Persist(next))
        }
        DurableOperationPhase::AddPublishTarget => {
            if final_attestation(operation, observations).is_ok() {
                if target.and_then(|target| target.pod_role_label.as_deref()) == Some("secondary") {
                    let mut next = operation.clone();
                    next.phase = DurableOperationPhase::AddFinalize;
                    next.phase_deadline_unix_seconds = now + ACTION_DEADLINE_SECONDS;
                    Ok(Decision::Persist(next))
                } else {
                    Ok(Decision::PatchPodRole {
                        target_id,
                        role: "secondary".to_string(),
                    })
                }
            } else if now >= operation.phase_deadline_unix_seconds {
                committed_degraded_decision(operation, target_pod_role_label, now)
            } else {
                Ok(Decision::Wait)
            }
        }
        DurableOperationPhase::AddFinalize => {
            if final_attestation(operation, observations).is_ok() {
                if target_pod_role_label == Some("secondary") {
                    let mut completed = operation.clone();
                    completed.phase = DurableOperationPhase::Completed;
                    Ok(Decision::Complete {
                        operation: completed,
                        snapshot: operation.target_snapshot.clone(),
                        compensated: false,
                    })
                } else {
                    Ok(Decision::PatchPodRole {
                        target_id,
                        role: "secondary".to_string(),
                    })
                }
            } else if now >= operation.phase_deadline_unix_seconds {
                committed_degraded_decision(operation, target_pod_role_label, now)
            } else {
                Ok(Decision::Wait)
            }
        }
        _ => Err(format!(
            "phase {:?} is not valid for coarse add-replica",
            operation.phase
        )),
    }
}

fn freeze_intent(
    operation: &DurableOperationStatus,
    observations: &OperationObservations,
    now: i64,
) -> Result<Decision, String> {
    let primary = observations
        .get(&operation.old_primary_id)
        .ok_or_else(|| "stable primary is unavailable while freezing add intent".to_string())?;
    let target_id = target_replica_id(operation)?;
    let target = observations
        .get(&target_id)
        .ok_or_else(|| "add target is unavailable while freezing intent".to_string())?;
    let expected_epoch = epoch(&operation.previous_snapshot.epoch);
    let primary_member = operation
        .previous_snapshot
        .members
        .iter()
        .find(|member| member.id == operation.old_primary_id)
        .ok_or_else(|| "stable primary member is missing".to_string())?;
    if primary.status.instance_id.as_str() != primary_member.instance_id
        || primary.status.role != Role::Primary
        || primary.status.epoch != expected_epoch
    {
        return Err("stable primary observation cannot freeze add intent".to_string());
    }
    if target.status.instance_id.as_str() != target_instance_id(operation)?
        || target.status.agent.lifecycle_peer_protocol_version
            != REPLICA_LIFECYCLE_PEER_PROTOCOL_VERSION
    {
        return Err("target observation cannot freeze add intent".to_string());
    }

    let previous_configuration =
        descriptor_for_snapshot(&operation.previous_snapshot, operation, observations, false)?;
    let catch_up_configuration =
        descriptor_for_snapshot(&operation.target_snapshot, operation, observations, true)?;
    let current_configuration =
        descriptor_for_snapshot(&operation.target_snapshot, operation, observations, false)?;
    let attempt = operation
        .add_intent
        .as_ref()
        .map_or(1, |intent| intent.attempt.saturating_add(1));
    let attempt_id = format!("{}:attempt-{attempt}", operation.operation_id);
    let action_id = format!("{attempt_id}:AddReplicaIntent");
    let core = AddReplicaIntent {
        operation_id: operation.operation_id.clone(),
        attempt_id: attempt_id.clone(),
        mode: match operation.add_mode {
            Some(DurableAddMode::ScaleUp) => AddReplicaMode::ScaleUp,
            Some(DurableAddMode::Rebuild) => AddReplicaMode::Rebuild,
            None => return Err("add operation has no mode".to_string()),
        },
        epoch: expected_epoch,
        primary_replica_id: operation.old_primary_id,
        primary_instance_id: ReplicaInstanceId::new(primary_member.instance_id.clone()),
        primary_agent_generation: primary.status.agent.generation.clone(),
        primary_control_address: primary.control_address.clone(),
        target_replica_id: target_id,
        target_instance_id: ReplicaInstanceId::new(target_instance_id(operation)?.to_string()),
        target_agent_generation: target.status.agent.generation.clone(),
        target_control_address: target.control_address.clone(),
        target_replicator_address: target.replicator_address.clone(),
        retired_instance_id: operation
            .retired_instance_id
            .as_ref()
            .map(|value| ReplicaInstanceId::new(value.clone())),
        previous_configuration: previous_configuration.clone(),
        catch_up_configuration: catch_up_configuration.clone(),
        current_configuration: current_configuration.clone(),
        minimum_committed_replicas: operation.minimum_committed_replicas.unwrap_or(1),
        deadline_unix_seconds: now + ADD_DEADLINE_SECONDS,
        compensation_deadline_unix_seconds: now + ADD_DEADLINE_SECONDS + COMPENSATION_GRACE_SECONDS,
        target_lifecycle_peer_protocol_version: REPLICA_LIFECYCLE_PEER_PROTOCOL_VERSION,
    };
    let intent = AddReplicaIntentStatus {
        attempt,
        attempt_id,
        action_id,
        primary_instance_id: core.primary_instance_id.to_string(),
        primary_agent_generation: core.primary_agent_generation.to_string(),
        primary_control_address: core.primary_control_address.clone(),
        target_agent_generation: core.target_agent_generation.to_string(),
        target_control_address: core.target_control_address.clone(),
        target_replicator_address: core.target_replicator_address.clone(),
        target_lifecycle_peer_protocol_version: core.target_lifecycle_peer_protocol_version,
        previous_configuration: descriptor_status(&previous_configuration),
        catch_up_configuration: descriptor_status(&catch_up_configuration),
        current_configuration: descriptor_status(&current_configuration),
        deadline_unix_seconds: core.deadline_unix_seconds,
        compensation_deadline_unix_seconds: core.compensation_deadline_unix_seconds,
        last_observed_phase: None,
    };
    let mut next = operation.clone();
    next.add_intent = Some(Box::new(intent));
    next.phase = DurableOperationPhase::AddDispatchIntent;
    next.phase_deadline_unix_seconds = core.deadline_unix_seconds;
    next.pending_action = None;
    Ok(Decision::Persist(next))
}

fn decide_await_coordination(
    operation: &DurableOperationStatus,
    observations: &OperationObservations,
    now: i64,
) -> Result<Decision, String> {
    let intent = operation
        .add_intent
        .as_ref()
        .ok_or_else(|| "await-coordination phase has no intent".to_string())?;
    let pending = operation
        .pending_action
        .as_ref()
        .ok_or_else(|| "await-coordination phase has no pending action".to_string())?;
    let expected_signature = core_intent(operation)?.signature();
    let primary = observations
        .get(&operation.old_primary_id)
        .ok_or_else(|| "add primary is unavailable".to_string())?;
    if primary.status.agent.generation.to_string() != intent.primary_agent_generation {
        let mut next = operation.clone();
        next.pending_action = None;
        next.phase = DurableOperationPhase::AddFreezeIntent;
        return Ok(Decision::Persist(next));
    }
    let target = observations.get(&target_replica_id(operation)?);
    if target.is_some_and(|target| {
        target.status.agent.generation.to_string() != intent.target_agent_generation
    }) {
        let mut next = operation.clone();
        next.pending_action = None;
        next.phase = DurableOperationPhase::AddFreezeIntent;
        return Ok(Decision::Persist(next));
    }
    if let Some(observation) = correlated_action_observation(&primary.status, &intent.action_id) {
        if observation.signature != expected_signature {
            if target.is_none()
                || target.is_some_and(|target| {
                    target.status.agent.generation.to_string() != intent.target_agent_generation
                })
            {
                let mut next = operation.clone();
                next.pending_action = None;
                next.phase = DurableOperationPhase::AddFreezeIntent;
                return Ok(Decision::Persist(next));
            }
            return Ok(Decision::Persist(poison(
                operation,
                "coarse add action signature conflicts with frozen intent",
            )));
        }
        if let Some(progress) = &observation.add_replica_progress {
            let mut next = operation.clone();
            if let Some(add_intent) = next.add_intent.as_mut() {
                add_intent.last_observed_phase = Some(format!("{:?}", progress.phase));
            }
            if next != *operation {
                return Ok(Decision::Persist(next));
            }
        }
        match observation.state {
            DurableActionState::Scheduled | DurableActionState::InProgress => {
                return if now >= intent.compensation_deadline_unix_seconds {
                    Ok(Decision::Persist(poison(
                        operation,
                        "coarse add coordinator remained in progress beyond its bounded deadline",
                    )))
                } else {
                    Ok(Decision::Wait)
                };
            }
            DurableActionState::Failed => {
                if now < intent.deadline_unix_seconds {
                    return Ok(Decision::Wait);
                }
                return Ok(Decision::Persist(poison(
                    operation,
                    observation
                        .error
                        .as_deref()
                        .unwrap_or("coarse add coordinator failed"),
                )));
            }
            DurableActionState::Completed => match observation.result {
                Some(DurableActionResult::AddReplica(AddReplicaTerminalResult::Committed)) => {
                    return if now >= intent.deadline_unix_seconds {
                        Ok(Decision::Persist(poison(
                            operation,
                            "primary reported add commit but current configuration is not observable",
                        )))
                    } else {
                        Ok(Decision::Wait)
                    };
                }
                Some(DurableActionResult::AddReplica(AddReplicaTerminalResult::Compensated)) => {
                    let mut next = operation.clone();
                    next.pending_action = None;
                    next.phase = DurableOperationPhase::AddDeleteCompensatedTarget;
                    return Ok(Decision::Persist(next));
                }
                Some(DurableActionResult::AddReplica(
                    AddReplicaTerminalResult::CompensationIncomplete,
                )) => {
                    return Ok(Decision::Persist(poison(
                        operation,
                        "coarse add compensation could not prove its safety barrier",
                    )));
                }
                Some(DurableActionResult::DataLoss(_))
                | Some(DurableActionResult::RemoveReplica(_))
                | None => {
                    return Ok(Decision::Persist(poison(
                        operation,
                        "coarse add completion has no typed add result",
                    )));
                }
            },
        }
    }
    if now >= intent.deadline_unix_seconds {
        return Ok(Decision::Persist(poison(
            operation,
            "coarse add action continuity is unavailable at its deadline",
        )));
    }
    Ok(Decision::Execute {
        target_id: operation.old_primary_id,
        action_id: pending.action_id.clone(),
        action: DurableReplicaAction::AddReplicaIntent {
            intent: Box::new(core_intent(operation)?),
        },
    })
}

fn committed_degraded_decision(
    operation: &DurableOperationStatus,
    target_pod_role_label: Option<&str>,
    _now: i64,
) -> Result<Decision, String> {
    if target_pod_role_label == Some("secondary") {
        Ok(Decision::PatchPodRole {
            target_id: target_replica_id(operation)?,
            role: "bootstrap".to_string(),
        })
    } else {
        let mut next = operation.clone();
        next.phase = DurableOperationPhase::AddCommittedDegradedHandoff;
        Ok(Decision::Persist(next))
    }
}

pub(crate) fn final_attestation(
    operation: &DurableOperationStatus,
    observations: &OperationObservations,
) -> Result<(), String> {
    let expected_epoch = epoch(&operation.target_snapshot.epoch);
    let intent = operation
        .add_intent
        .as_ref()
        .ok_or_else(|| "add operation has no frozen intent".to_string())?;
    let core = core_intent(operation)?;
    let minimum = operation.minimum_committed_replicas.unwrap_or(1) as usize;
    if operation.target_snapshot.members.len() < minimum {
        return Err("target topology is below minReplicas".to_string());
    }
    for member in &operation.target_snapshot.members {
        let observation = observations
            .get(&member.id)
            .ok_or_else(|| format!("target member {} is unavailable", member.id))?;
        if observation.status.instance_id.as_str() != member.instance_id
            || observation.status.epoch != expected_epoch
            || !observation.status.healthy
        {
            return Err(format!(
                "target member {} identity, epoch, or health is not attested",
                member.id
            ));
        }
        if member.id == target_replica_id(operation)?
            && observation.status.agent.generation.to_string() != intent.target_agent_generation
        {
            return Err("target agent generation is not attested".to_string());
        }
        let expected_role = if member.id == operation.target_snapshot.primary_id {
            Role::Primary
        } else {
            Role::ActiveSecondary
        };
        if observation.status.role != expected_role {
            return Err(format!("target member {} role is not attested", member.id));
        }
    }
    let primary = observations
        .get(&operation.target_snapshot.primary_id)
        .ok_or_else(|| "target primary is unavailable".to_string())?;
    if !primary_current_configuration_matches(operation, primary)? {
        return Err("target current configuration is not attested".to_string());
    }
    let target_id = target_replica_id(operation)?;
    let target_instance = target_instance_id(operation)?;
    if !primary
        .status
        .active_replica_connections
        .iter()
        .any(|connection| {
            connection.id == target_id && connection.instance_id.as_str() == target_instance
        })
    {
        return Err("target primary connection is not attested".to_string());
    }
    let build = primary
        .status
        .build_observation
        .as_ref()
        .ok_or_else(|| "primary has no build attestation".to_string())?;
    if build.build_key != core.semantic_build_key()
        || build.target_replica_id != target_id
        || build.target_instance_id.as_str() != target_instance
        || build.target_agent_generation.to_string() != intent.target_agent_generation
        || build.state != RuntimeBuildState::Completed
    {
        return Err("primary build attestation does not match frozen intent".to_string());
    }
    Ok(())
}

fn primary_current_configuration_matches(
    operation: &DurableOperationStatus,
    primary: &super::ReplicaObservation,
) -> Result<bool, String> {
    let Some(intent) = operation.add_intent.as_ref() else {
        return Ok(false);
    };
    let descriptor = core_descriptor(&intent.current_configuration)?;
    Ok(primary.status.role == Role::Primary
        && primary.status.epoch == epoch(&operation.target_snapshot.epoch)
        && primary.status.configuration.as_ref()
            == Some(&descriptor.status(ReplicaConfigurationMode::Current)))
}

fn descriptor_for_snapshot(
    snapshot: &StablePartitionSnapshotStatus,
    operation: &DurableOperationStatus,
    observations: &OperationObservations,
    target_must_catch_up: bool,
) -> Result<ConfigurationDescriptor, String> {
    let target_id = target_replica_id(operation)?;
    let mut members = Vec::new();
    for member in &snapshot.members {
        if member.id == snapshot.primary_id {
            continue;
        }
        let observation = observations
            .get(&member.id)
            .ok_or_else(|| format!("replica {} is unavailable for frozen config", member.id))?;
        let is_target =
            member.id == target_id && member.instance_id == target_instance_id(operation)?;
        members.push(ConfigurationMemberDescriptor {
            id: member.id,
            instance_id: ReplicaInstanceId::new(member.instance_id.clone()),
            role: Role::ActiveSecondary,
            status: ReplicaStatus::Up,
            replicator_address: observation.replicator_address.clone(),
            must_catch_up: target_must_catch_up && is_target,
            progress: if is_target {
                ConfigurationProgressSource::BuildCopyLsn
            } else {
                ConfigurationProgressSource::Frozen {
                    current_progress: observation.status.current_progress,
                    catch_up_capability: observation
                        .status
                        .catch_up_capability
                        .unwrap_or(observation.status.current_progress),
                }
            },
        });
    }
    members.sort_by_key(|member| member.id);
    Ok(ConfigurationDescriptor {
        members,
        write_quorum: snapshot.write_quorum,
    })
}

fn descriptor_status(descriptor: &ConfigurationDescriptor) -> ConfigurationDescriptorStatus {
    ConfigurationDescriptorStatus {
        members: descriptor
            .members
            .iter()
            .map(|member| {
                let (progress_source, current_progress, catch_up_capability) = match member.progress
                {
                    ConfigurationProgressSource::Frozen {
                        current_progress,
                        catch_up_capability,
                    } => (
                        ConfigurationProgressSourceStatus::Frozen,
                        current_progress,
                        catch_up_capability,
                    ),
                    ConfigurationProgressSource::BuildCopyLsn => {
                        (ConfigurationProgressSourceStatus::BuildCopyLsn, 0, 0)
                    }
                };
                ConfigurationMemberDescriptorStatus {
                    id: member.id,
                    instance_id: member.instance_id.to_string(),
                    role: format!("{:?}", member.role),
                    status: format!("{:?}", member.status),
                    replicator_address: member.replicator_address.clone(),
                    must_catch_up: member.must_catch_up,
                    progress_source,
                    current_progress,
                    catch_up_capability,
                }
            })
            .collect(),
        write_quorum: descriptor.write_quorum,
    }
}

fn core_descriptor(
    descriptor: &ConfigurationDescriptorStatus,
) -> Result<ConfigurationDescriptor, String> {
    let mut ids = HashSet::new();
    let mut instances = HashSet::new();
    let members = descriptor
        .members
        .iter()
        .map(|member| {
            if member.id <= 0 || !ids.insert(member.id) {
                return Err("configuration descriptor has duplicate replica ID".to_string());
            }
            if member.instance_id.is_empty() || !instances.insert(member.instance_id.clone()) {
                return Err("configuration descriptor has duplicate incarnation".to_string());
            }
            if member.role != "ActiveSecondary" || member.status != "Up" {
                return Err("configuration descriptor has unsupported role/status".to_string());
            }
            Ok(ConfigurationMemberDescriptor {
                id: member.id,
                instance_id: ReplicaInstanceId::new(member.instance_id.clone()),
                role: Role::ActiveSecondary,
                status: ReplicaStatus::Up,
                replicator_address: member.replicator_address.clone(),
                must_catch_up: member.must_catch_up,
                progress: match member.progress_source {
                    ConfigurationProgressSourceStatus::Frozen => {
                        ConfigurationProgressSource::Frozen {
                            current_progress: member.current_progress,
                            catch_up_capability: member.catch_up_capability,
                        }
                    }
                    ConfigurationProgressSourceStatus::BuildCopyLsn => {
                        ConfigurationProgressSource::BuildCopyLsn
                    }
                },
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    Ok(ConfigurationDescriptor {
        members,
        write_quorum: descriptor.write_quorum,
    })
}

pub(crate) fn core_intent(operation: &DurableOperationStatus) -> Result<AddReplicaIntent, String> {
    let intent = operation
        .add_intent
        .as_ref()
        .ok_or_else(|| "add operation has no frozen intent".to_string())?;
    let core = AddReplicaIntent {
        operation_id: operation.operation_id.clone(),
        attempt_id: intent.attempt_id.clone(),
        mode: match operation.add_mode {
            Some(DurableAddMode::ScaleUp) => AddReplicaMode::ScaleUp,
            Some(DurableAddMode::Rebuild) => AddReplicaMode::Rebuild,
            None => return Err("add operation has no mode".to_string()),
        },
        epoch: epoch(&operation.previous_snapshot.epoch),
        primary_replica_id: operation.old_primary_id,
        primary_instance_id: ReplicaInstanceId::new(intent.primary_instance_id.clone()),
        primary_agent_generation: kuberic_core::types::AgentGeneration::parse(
            intent.primary_agent_generation.clone(),
        )?,
        primary_control_address: intent.primary_control_address.clone(),
        target_replica_id: target_replica_id(operation)?,
        target_instance_id: ReplicaInstanceId::new(target_instance_id(operation)?.to_string()),
        target_agent_generation: kuberic_core::types::AgentGeneration::parse(
            intent.target_agent_generation.clone(),
        )?,
        target_control_address: intent.target_control_address.clone(),
        target_replicator_address: intent.target_replicator_address.clone(),
        retired_instance_id: operation
            .retired_instance_id
            .as_ref()
            .map(|value| ReplicaInstanceId::new(value.clone())),
        previous_configuration: core_descriptor(&intent.previous_configuration)?,
        catch_up_configuration: core_descriptor(&intent.catch_up_configuration)?,
        current_configuration: core_descriptor(&intent.current_configuration)?,
        minimum_committed_replicas: operation.minimum_committed_replicas.unwrap_or(1),
        deadline_unix_seconds: intent.deadline_unix_seconds,
        compensation_deadline_unix_seconds: intent.compensation_deadline_unix_seconds,
        target_lifecycle_peer_protocol_version: intent.target_lifecycle_peer_protocol_version,
    };
    core.validate()?;
    Ok(core)
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

fn validate_operation(operation: &DurableOperationStatus) -> Result<(), String> {
    if operation.version != ADD_REPLICA_OPERATION_VERSION {
        return Err(format!(
            "unsupported add-replica operation version {}; expected {}",
            operation.version, ADD_REPLICA_OPERATION_VERSION
        ));
    }
    if operation.kind != DurableOperationKind::AddReplica {
        return Err("add decision received another operation kind".to_string());
    }
    validate_snapshot(&operation.previous_snapshot)?;
    validate_snapshot(&operation.target_snapshot)?;
    if let Some(intent) = &operation.add_intent {
        if intent.action_id != format!("{}:AddReplicaIntent", intent.attempt_id) {
            return Err("frozen add action ID is not deterministic".to_string());
        }
        core_intent(operation)?;
    }
    if let Some(pending) = &operation.pending_action {
        let intent = operation
            .add_intent
            .as_ref()
            .ok_or_else(|| "pending coarse add has no frozen intent".to_string())?;
        if pending.kind != DurableActionKind::AddReplicaIntent
            || pending.action_id != intent.action_id
            || pending.target_id != operation.old_primary_id
            || pending.target_instance_id != intent.primary_instance_id
            || pending.desired_postcondition.kind != DurablePostconditionKind::AddReplicaCoordinated
        {
            return Err("pending coarse add action is not deterministic".to_string());
        }
    }
    Ok(())
}

fn validate_snapshot(snapshot: &StablePartitionSnapshotStatus) -> Result<(), String> {
    if snapshot.members.is_empty() {
        return Err("stable snapshot has no members".to_string());
    }
    let mut ids = HashSet::new();
    let mut instances = HashSet::new();
    let mut primary_count = 0;
    for member in &snapshot.members {
        if member.id <= 0 || !ids.insert(member.id) {
            return Err("stable snapshot has invalid or duplicate replica ID".to_string());
        }
        if member.instance_id.is_empty() || !instances.insert(member.instance_id.clone()) {
            return Err("stable snapshot has missing or duplicate incarnation".to_string());
        }
        if member.role == StableReplicaRoleStatus::Primary {
            primary_count += 1;
            if member.id != snapshot.primary_id {
                return Err("stable snapshot primary role conflicts with primary ID".to_string());
            }
        }
    }
    if primary_count != 1 {
        return Err("stable snapshot must contain exactly one primary".to_string());
    }
    let expected_quorum = snapshot.members.len() as u32 / 2 + 1;
    if snapshot.write_quorum != expected_quorum {
        return Err("stable snapshot has invalid write quorum".to_string());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crd::{OptionalStablePartitionSnapshotStatus, StableReplicaSnapshotStatus};
    use kuberic_core::types::{
        AccessStatus, AgentControlVersion, AgentGeneration, ReplicaAgentStatus, ReplicaStatusInfo,
    };

    fn snapshot() -> StablePartitionSnapshotStatus {
        StablePartitionSnapshotStatus {
            epoch: EpochStatus {
                data_loss_number: 1,
                configuration_number: 7,
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
        instance: &str,
        role: Role,
        epoch: Epoch,
        healthy: bool,
    ) -> super::super::ReplicaObservation {
        super::super::ReplicaObservation {
            status: ReplicaStatusInfo {
                instance_id: ReplicaInstanceId::new(instance),
                role,
                epoch,
                current_progress: 0,
                catch_up_capability: Some(0),
                committed_lsn: 0,
                healthy,
                write_status: if role == Role::Primary {
                    AccessStatus::Granted
                } else {
                    AccessStatus::NotPrimary
                },
                configuration: None,
                election_configuration: None,
                deactivation_info: None,
                active_replica_connections: Vec::new(),
                build_observation: None,
                agent: ReplicaAgentStatus {
                    protocol_version:
                        kuberic_core::replica_agent::CORRELATED_CONTROL_PROTOCOL_VERSION,
                    lifecycle_peer_protocol_version: REPLICA_LIFECYCLE_PEER_PROTOCOL_VERSION,
                    generation: AgentGeneration::parse("0123456789abcdef0123456789abcdef").unwrap(),
                    control_version: AgentControlVersion::default(),
                    current_action: None,
                    retained_terminal_actions: Vec::new(),
                    local_faults: Vec::new(),
                },
            },
            control_address: format!("http://{instance}-control"),
            replicator_address: format!("http://{instance}-data"),
            pod_name: instance.to_string(),
            pod_role_label: Some("bootstrap".to_string()),
        }
    }

    #[test]
    fn start_add_uses_coarse_version_and_phase() {
        let operation = start_add_replica(
            "set",
            snapshot(),
            3,
            "candidate".to_string(),
            "set-2".to_string(),
            DurableAddMode::ScaleUp,
            1,
            10,
        )
        .unwrap();
        assert_eq!(operation.version, ADD_REPLICA_OPERATION_VERSION);
        assert_eq!(operation.phase, DurableOperationPhase::AddFreezeIntent);
        assert!(operation.add_intent.is_none());
        assert!(operation.pending_action.is_none());
        let json = serde_json::to_value(&operation).unwrap();
        assert!(json.get("nextSecondaryIndex").is_none());
        assert!(json.get("targetPodUid").is_none());
    }

    #[test]
    fn add_version_is_independent_from_other_durable_version() {
        assert_ne!(
            ADD_REPLICA_OPERATION_VERSION,
            crate::crd::DURABLE_OPERATION_VERSION
        );
    }

    #[test]
    fn unsupported_add_version_is_rejected() {
        let mut operation = start_add_replica(
            "set",
            snapshot(),
            3,
            "candidate".to_string(),
            "set-2".to_string(),
            DurableAddMode::ScaleUp,
            1,
            10,
        )
        .unwrap();
        operation.version = 2;
        assert!(
            decide_add_replica(
                &operation,
                &OperationObservations::new(),
                &OperationPodIdentities::new(),
                None,
                10,
            )
            .is_err()
        );
    }

    #[test]
    fn optional_snapshot_type_remains_usable() {
        let value = OptionalStablePartitionSnapshotStatus(Some(snapshot()));
        assert!(value.is_some());
    }

    #[test]
    fn destroyed_primary_before_prepare_can_delete_untouched_target() {
        let operation = start_add_replica(
            "set",
            snapshot(),
            3,
            "candidate".to_string(),
            "set-2".to_string(),
            DurableAddMode::ScaleUp,
            1,
            10,
        )
        .unwrap();
        let mut observations = OperationObservations::new();
        observations.insert(
            3,
            observation("candidate", Role::Unknown, Epoch::default(), false),
        );
        let pod_identities = OperationPodIdentities::from([
            (1, "replacement-primary".to_string()),
            (3, "candidate".to_string()),
        ]);
        let Decision::Persist(next) =
            decide_add_replica(&operation, &observations, &pod_identities, None, 20).unwrap()
        else {
            panic!("untouched target should enter exact-UID cleanup");
        };
        assert_eq!(
            next.phase,
            DurableOperationPhase::AddDeleteCompensatedTarget
        );
        assert!(matches!(
            decide_add_replica(&next, &observations, &pod_identities, None, 21).unwrap(),
            Decision::DeletePod { .. }
        ));
        let without_target = OperationPodIdentities::from([(1, "replacement-primary".to_string())]);
        let Decision::Persist(finalizing) = decide_add_replica(
            &next,
            &OperationObservations::new(),
            &without_target,
            None,
            22,
        )
        .unwrap() else {
            panic!("absent target should advance compensation");
        };
        assert_eq!(
            finalizing.phase,
            DurableOperationPhase::AddCompensateFinalize
        );
        assert!(matches!(
            decide_add_replica(
                &finalizing,
                &OperationObservations::new(),
                &without_target,
                None,
                23,
            )
            .unwrap(),
            Decision::Complete {
                compensated: true,
                ..
            }
        ));
    }

    #[test]
    fn freeze_failure_is_bounded_by_phase_deadline() {
        let operation = start_add_replica(
            "set",
            snapshot(),
            3,
            "candidate".to_string(),
            "set-2".to_string(),
            DurableAddMode::ScaleUp,
            1,
            10,
        )
        .unwrap();
        let mut observations = OperationObservations::new();
        observations.insert(
            1,
            observation("primary", Role::Primary, Epoch::new(1, 7), true),
        );
        let pod_identities = OperationPodIdentities::from([
            (1, "primary".to_string()),
            (3, "candidate".to_string()),
        ]);
        assert!(matches!(
            decide_add_replica(&operation, &observations, &pod_identities, None, 20).unwrap(),
            Decision::Wait
        ));
        let Decision::Persist(poisoned) = decide_add_replica(
            &operation,
            &observations,
            &pod_identities,
            None,
            operation.phase_deadline_unix_seconds,
        )
        .unwrap() else {
            panic!("freeze failure must become bounded poison");
        };
        assert_eq!(poisoned.phase, DurableOperationPhase::Poisoned);
    }

    #[test]
    fn committed_degraded_handoff_completes_without_primary_observation() {
        let mut operation = start_add_replica(
            "set",
            snapshot(),
            3,
            "candidate".to_string(),
            "set-2".to_string(),
            DurableAddMode::ScaleUp,
            1,
            10,
        )
        .unwrap();
        operation.committed_snapshot = Some(operation.target_snapshot.clone());
        operation.phase = DurableOperationPhase::AddCommittedDegradedHandoff;
        assert!(matches!(
            decide_add_replica(
                &operation,
                &OperationObservations::new(),
                &OperationPodIdentities::new(),
                None,
                20,
            )
            .unwrap(),
            Decision::CompleteDegraded { .. }
        ));
    }

    #[test]
    fn committed_primary_loss_routes_to_degraded_handoff_before_ambiguity_guards() {
        let mut operation = start_add_replica(
            "set",
            snapshot(),
            3,
            "candidate".to_string(),
            "set-2".to_string(),
            DurableAddMode::ScaleUp,
            1,
            10,
        )
        .unwrap();
        operation.committed_snapshot = Some(operation.target_snapshot.clone());
        operation.phase = DurableOperationPhase::AddPublishTarget;

        let exact_primary_pod = OperationPodIdentities::from([(1, "primary".to_string())]);
        assert!(matches!(
            decide_add_replica(
                &operation,
                &OperationObservations::new(),
                &exact_primary_pod,
                Some("secondary"),
                20,
            )
            .unwrap(),
            Decision::PatchPodRole { ref role, .. } if role == "bootstrap"
        ));
        let Decision::Persist(missing_status) = decide_add_replica(
            &operation,
            &OperationObservations::new(),
            &exact_primary_pod,
            None,
            20,
        )
        .unwrap() else {
            panic!("committed primary status loss must begin degraded handoff");
        };
        assert_eq!(
            missing_status.phase,
            DurableOperationPhase::AddCommittedDegradedHandoff
        );

        let mut observations = OperationObservations::new();
        observations.insert(
            1,
            observation("primary", Role::Unknown, Epoch::default(), false),
        );
        let Decision::Persist(restarted_runtime) =
            decide_add_replica(&operation, &observations, &exact_primary_pod, None, 20).unwrap()
        else {
            panic!("committed primary runtime loss must begin degraded handoff");
        };
        assert_eq!(
            restarted_runtime.phase,
            DurableOperationPhase::AddCommittedDegradedHandoff
        );
    }

    #[test]
    fn add_finalize_attestation_loss_is_bounded_to_degraded_handoff() {
        let mut operation = start_add_replica(
            "set",
            snapshot(),
            3,
            "candidate".to_string(),
            "set-2".to_string(),
            DurableAddMode::ScaleUp,
            1,
            10,
        )
        .unwrap();
        operation.committed_snapshot = Some(operation.target_snapshot.clone());
        operation.phase = DurableOperationPhase::AddFinalize;
        operation.phase_deadline_unix_seconds = 20;
        let mut observations = OperationObservations::new();
        observations.insert(
            1,
            observation("primary", Role::Primary, Epoch::new(1, 7), true),
        );
        let pod_identities = OperationPodIdentities::from([
            (1, "primary".to_string()),
            (3, "candidate".to_string()),
        ]);
        assert!(matches!(
            decide_add_replica(
                &operation,
                &observations,
                &pod_identities,
                Some("secondary"),
                19,
            )
            .unwrap(),
            Decision::Wait
        ));
        let Decision::Persist(next) =
            decide_add_replica(&operation, &observations, &pod_identities, None, 20).unwrap()
        else {
            panic!("final attestation loss must start degraded handoff");
        };
        assert_eq!(
            next.phase,
            DurableOperationPhase::AddCommittedDegradedHandoff
        );
    }
}
