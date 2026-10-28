use super::state::AgentState;
use super::*;
use crate::protocol::public_operations::*;
use crate::protocol::types::{ProcessSessionId, ReplicaRole};

fn complete(record: &state::PublicOperationRecord, accepted: bool) -> bool {
    record.stage == state::PublicOperationStage::Completed
        && record.disposition == Some(state::PublicOperationDisposition::Succeeded)
        && record.superseded_by.is_none()
        && record.lifecycle.in_flight.is_none()
        && record.ordinary.as_ref().is_some_and(|ordinary| {
            ordinary.actions.len() == record.lifecycle.outcomes.len()
                && (!accepted || ordinary.accepted && ordinary.host_settled)
        })
}

pub(crate) fn round(state: &AgentState, goal: &ReplicaGoal) -> bool {
    state
        .public_v1_operation_state
        .as_ref()
        .is_some_and(|preview| round_in(preview, goal))
}

fn round_in(preview: &state::PublicV1OperationState, goal: &ReplicaGoal) -> bool {
    preview.operations.values().any(|record| {
        let Some(old) = record.intent.goal() else {
            return false;
        };
        old.scope.process_session_id == goal.scope.process_session_id
            && old.scope.creation_id == goal.scope.creation_id
            && old.scope.history == goal.scope.history
            && old.scope.storage == goal.scope.storage
            && old.scope.epoch == goal.scope.epoch
            && old.scope.configuration.current == goal.scope.configuration.current
            && complete(record, true)
            && record.lifecycle.outcomes.iter().any(|outcome| {
                matches!(outcome, state::PublicInstructionOutcome::NativeLifecycle(facts)
                    if facts.operation_id == old.operation_id && facts.scope == old.scope
                        && Some(facts.native_role) == old.role()
                        && facts.application_role == old.role()
                        && facts.deactivation.is_some()
                        && (facts.state == NativeReplicaState::Ready && old.role() == Some(ReplicaRole::Primary)
                            || facts.state == NativeReplicaState::InBuild && old.role() == Some(ReplicaRole::IdleSecondary)))
            })
            && (matches!(
                old.desired,
                ReplicaGoalKind::Prepare {
                    rebuild: Some(_),
                    ..
                }
            ) || matches!(
                old.desired,
                ReplicaGoalKind::Reconfigure {
                    role: ReplicaRole::Primary,
                    catch_up: None
                }
            ) && old
                .scope
                .configuration
                .previous
                .as_ref()
                .is_some_and(|previous| {
                    previous.primary_id != old.scope.configuration.current.primary_id
                }))
    })
}

pub(crate) fn successful_final(state: &AgentState, goal: &ReplicaGoal) -> bool {
    round(state, goal)
        && state
            .public_v1_operation_state
            .as_ref()
            .and_then(|preview| preview.operations.get(&goal.operation_id))
            .is_some_and(|record| final_record(record, true))
}

pub(crate) fn finished_current(record: &state::PublicOperationRecord) -> bool {
    final_record(record, false)
}

fn final_record(record: &state::PublicOperationRecord, accepted: bool) -> bool {
    let Some(goal) = record.intent.goal() else {
        return false;
    };
    goal.scope.configuration.previous.is_none()
        && matches!(
            goal.desired,
            ReplicaGoalKind::Reconfigure {
                role: ReplicaRole::Primary,
                catch_up: Some(PublicCatchUpMode::WriteQuorum)
            } | ReplicaGoalKind::Reconfigure {
                role: ReplicaRole::ActiveSecondary,
                catch_up: None
            }
        )
        && complete(record, accepted)
        && record
            .ordinary
            .as_ref()
            .is_some_and(|ordinary| ordinary.actions == final_actions(goal))
        && record.lifecycle.outcomes.iter().any(|outcome| {
            matches!(outcome, state::PublicInstructionOutcome::NativeLifecycle(facts)
                if facts.operation_id == goal.operation_id && facts.revision == goal.revision
                    && facts.scope == goal.scope && facts.state == NativeReplicaState::Ready
                    && Some(facts.native_role) == goal.role()
                    && facts.application_role == goal.role()
                    && facts.applied_primary_fence == Some(goal.scope.epoch)
                    && facts.deactivation.as_ref().is_some_and(|history|
                        history.primary_fence == goal.scope.epoch))
        })
}

pub(crate) fn final_actions(goal: &ReplicaGoal) -> Vec<state::PublicInstruction> {
    let mut actions = public_lifecycle::operation_instructions(&goal.intent());
    if matches!(
        goal.desired,
        ReplicaGoalKind::Reconfigure {
            role: ReplicaRole::Primary,
            catch_up: Some(PublicCatchUpMode::WriteQuorum)
        }
    ) && goal.scope.configuration.previous.is_none()
        && let Some(index) = actions
            .iter()
            .position(|action| *action == state::PublicInstruction::NativeComplete)
    {
        actions.insert(index, state::PublicInstruction::CurrentConfiguration);
    }
    actions
}

pub(crate) fn final_peer(goal: &ReplicaGoal, peer: &ReplicaAgentSnapshot) -> bool {
    let Some(publication) = peer.publication.as_ref() else {
        return false;
    };
    let Some(facts) = peer.native_lifecycle.as_ref() else {
        return false;
    };
    let expected = if goal.role() == Some(ReplicaRole::Primary) {
        ReplicaRole::ActiveSecondary
    } else {
        ReplicaRole::Primary
    };
    !peer.withdrawn
        && peer.format == ORDINARY_ACTION_FORMAT
        && peer.history == goal.scope.history
        && publication.scope.storage == peer.storage
        && publication.scope.process_session_id == peer.process_session_id
        && publication.scope.creation_id == peer.creation_id
        && publication.scope.epoch == goal.scope.epoch
        && publication.scope.configuration.current == goal.scope.configuration.current
        && publication.scope.configuration.previous.is_none()
        && publication.stage == ReplicaPublicationStage::AchievedScope
        && publication.role == expected
        && facts.scope == publication.scope
        && facts.state == NativeReplicaState::Ready
        && facts.native_role == expected
        && facts.application_role == Some(expected)
        && facts.applied_primary_fence == Some(goal.scope.epoch)
        && facts
            .deactivation
            .as_ref()
            .is_some_and(|history| history.primary_fence == goal.scope.epoch)
        && peer.goals.iter().any(|entry| {
            entry.goal.operation_id == facts.operation_id
                && entry.goal.scope == facts.scope
                && entry.goal.revision == publication.generation
                && entry.status == ReplicaGoalStatus::Achieved
                && entry.host_settled
                && entry.selected_call.is_none()
                && entry.receipts == final_actions(&entry.goal).len()
                && matches!(
                    entry.goal.desired,
                    ReplicaGoalKind::Reconfigure {
                        role: ReplicaRole::Primary,
                        catch_up: Some(PublicCatchUpMode::WriteQuorum)
                    } | ReplicaGoalKind::Reconfigure {
                        role: ReplicaRole::ActiveSecondary,
                        catch_up: None
                    }
                )
        })
}

pub(crate) fn dependency_replaced(
    preview: &state::PublicV1OperationState,
    old: &state::PublicOperationRecord,
    process: &ProcessSessionId,
) -> bool {
    old.superseded_by
        .as_ref()
        .and_then(|id| preview.operations.get(id))
        .is_some_and(|record| {
            let Some(goal) = record.intent.goal() else {
                return false;
            };
            &goal.scope.process_session_id == process
                && old.intent.process_session_id != *process
                && old.accepted_history.as_ref() == Some(&goal.scope.history)
                && old.accepted_storage.as_ref() == Some(&goal.scope.storage)
                && round_in(preview, goal)
                && final_record(record, true)
        })
}

pub(crate) fn restart_close(goal: &ReplicaGoal) -> bool {
    matches!(goal.desired, ReplicaGoalKind::Close)
        && goal
            .scope
            .configuration
            .previous
            .as_ref()
            .is_some_and(|previous| {
                goal.scope.epoch > previous.epoch
                    && goal.scope.epoch.data_loss_number == previous.epoch.data_loss_number
                    && goal.scope.configuration.current.primary_id != previous.primary_id
                    && goal.scope.configuration.current.write_quorum == previous.write_quorum
                    && [previous, &goal.scope.configuration.current]
                        .into_iter()
                        .all(|configuration| {
                            configuration.members.iter().any(|member| {
                                member.identity == goal.scope.storage.replica
                                    && member.role == ReplicaRole::ActiveSecondary
                            })
                        })
            })
}

pub(crate) fn restart_notice(
    state: &AgentState,
    record: &state::PublicOperationRecord,
) -> Result<RetainedRecoveryExitNotice> {
    let goal = record
        .intent
        .goal()
        .filter(|goal| restart_close(goal))
        .ok_or_else(|| HostError::CommandRejected("not native restart Close".into()))?;
    let reason = record
        .ordinary
        .as_ref()
        .and_then(|ordinary| ordinary.native_input.as_ref())
        .and_then(|input| input.native_rebuild.as_ref())
        .ok_or_else(|| {
            HostError::CommandRejected("missing selected native restart decision".into())
        })?;
    let preview = state.public_v1_operation_state.as_ref().unwrap();
    let admitted = preview
        .operations
        .values()
        .filter(|old| {
            old.intent.goal().is_some_and(|old_goal| {
                old_goal.scope.process_session_id == goal.scope.process_session_id
                    && old_goal.scope.creation_id == goal.scope.creation_id
                    && old_goal.scope.configuration.previous.is_none()
                    && Some(&old_goal.scope.configuration.current)
                        == goal.scope.configuration.previous.as_ref()
                    && old_goal.role() == Some(ReplicaRole::ActiveSecondary)
            }) && old.stage == state::PublicOperationStage::Completed
                && old.disposition == Some(state::PublicOperationDisposition::Succeeded)
                && old.ordinary.as_ref().is_some_and(|ordinary| {
                    ordinary.accepted
                        && ordinary.host_settled
                        && ordinary.actions.len() == old.lifecycle.outcomes.len()
                })
        })
        .max_by_key(|old| old.intent.revision)
        .ok_or_else(|| {
            HostError::CommandRejected("native restart original accepted owner missing".into())
        })?;
    let qualification = RetainedMemberQualification {
        predecessor_process_session_id: goal.scope.process_session_id.clone(),
        predecessor_creation_id: goal.scope.creation_id.clone(),
        admitted_operation_id: admitted.intent.operation_id.clone(),
        admitted_revision: admitted.intent.revision,
        configuration: goal.scope.configuration.previous.clone().unwrap(),
    };
    let (_, pid, lifetime) = ordinary_goal::retained::historical(
        state,
        &goal.scope.history,
        &goal.scope.storage,
        &qualification,
    )?;
    if pid != std::process::id() || lifetime != ordinary_goal::retained::process_lifetime_id() {
        return Err(HostError::CommandRejected(
            "native restart lost its actual process owner".into(),
        ));
    }
    Ok(RetainedRecoveryExitNotice {
        fault_operation_id: goal.operation_id.clone(),
        fault_revision: goal.revision,
        process_id: pid,
        process_lifetime: lifetime,
        history: goal.scope.history.clone(),
        storage: goal.scope.storage.clone(),
        qualification,
        native_rebuild: Some(reason.clone()),
    })
}

pub(crate) fn validate_restart_notice(
    state: &AgentState,
    notice: &RetainedRecoveryExitNotice,
    completed: bool,
) -> Result<()> {
    let reject = || HostError::CommandRejected("crossed or incomplete native restart Close".into());
    let reason = notice.native_rebuild.as_ref().ok_or_else(reject)?;
    let preview = state
        .public_v1_operation_state
        .as_ref()
        .ok_or_else(reject)?;
    let record = preview
        .operations
        .get(&notice.fault_operation_id)
        .ok_or_else(reject)?;
    let goal = record
        .intent
        .goal()
        .filter(|goal| restart_close(goal))
        .ok_or_else(reject)?;
    let (_, pid, lifetime) = ordinary_goal::retained::historical(
        state,
        &notice.history,
        &notice.storage,
        &notice.qualification,
    )?;
    let input = record
        .ordinary
        .as_ref()
        .and_then(|ordinary| ordinary.native_input.as_ref())
        .ok_or_else(reject)?;
    if pid != notice.process_id
        || lifetime != notice.process_lifetime
        || record.intent.revision != notice.fault_revision
        || goal.scope.history != notice.history
        || goal.scope.storage != notice.storage
        || goal.scope.process_session_id != notice.qualification.predecessor_process_session_id
        || goal.scope.creation_id != notice.qualification.predecessor_creation_id
        || input.native_rebuild.as_ref() != Some(reason)
        || input.lifecycle.scope != goal.scope
        || input.lifecycle.state != NativeReplicaState::Ready
        || input.lifecycle.native_role != ReplicaRole::ActiveSecondary
        || input.lifecycle.application_role != Some(ReplicaRole::ActiveSecondary)
        || reason.source.scope.history != goal.scope.history
        || reason.source.scope.epoch != goal.scope.epoch
        || reason.source.role != ReplicaRole::Primary
        || reason.source.stage != ReplicaPublicationStage::AchievedScope
        || reason.source.scope.configuration.current != goal.scope.configuration.current
        || reason.source.scope.configuration.previous != goal.scope.configuration.previous
        || reason.source_inspection.scope.storage != reason.source.scope.storage
        || reason.source_inspection.scope.process_session_id
            != reason.source.scope.process_session_id
        || reason.source_inspection.scope.creation_id != reason.source.scope.creation_id
        || reason.source_inspection.scope.epoch != reason.source.scope.epoch
        || reason.source_inspection.scope.configuration.current
            != reason.source.scope.configuration.current
        || reason.source_inspection.scope.configuration.previous
            != reason.source.scope.configuration.previous
        || !matches!(reason.source_inspection.desired, ReplicaGoalKind::Discover)
        || reason.target_inspection.scope.storage != goal.scope.storage
        || reason.target_inspection.scope.process_session_id != goal.scope.process_session_id
        || reason.target_inspection.scope.creation_id != goal.scope.creation_id
        || reason.target_inspection.scope.epoch != goal.scope.epoch
        || input.promotion.as_ref().is_none_or(|status| {
            status.input.operation_id != reason.target_inspection.operation_id
                || status.input.revision != reason.target_inspection.revision
                || status.input.scope != reason.target_inspection.scope
                || status.progress.value() != Some(reason.target_last)
        })
        || record.ordinary.as_ref().is_none_or(|ordinary| {
            ordinary.actions != public_lifecycle::operation_instructions(&goal.intent())
        })
        || reason.source_last <= reason.target_last
        || reason.target_last < 0
        || reason.source_first < 0
        || reason.source_first > 0 && reason.source_first <= reason.target_last.saturating_add(1)
        || !preview.terminal
        || !preview.writes_revoked
        || completed
            && (record.stage != state::PublicOperationStage::Completed
                || record.disposition != Some(state::PublicOperationDisposition::Succeeded)
                || record.lifecycle.in_flight.is_some()
                || record.ordinary.as_ref().is_none_or(|ordinary| {
                    ordinary.actions.len() != record.lifecycle.outcomes.len()
                }))
    {
        return Err(reject());
    }
    Ok(())
}

pub(crate) fn target_admitted(
    remote: &RemoteReplicaSelection,
    snapshot: &ReplicaAgentSnapshot,
    copied: bool,
) -> bool {
    let Some(prepare) = remote.original_member.as_deref() else {
        return false;
    };
    if snapshot.withdrawn
        || snapshot.history != remote.target.scope.history
        || snapshot.storage != remote.target.scope.storage
        || snapshot.process_session_id != remote.target.scope.process_session_id
        || snapshot.creation_id != remote.target.scope.creation_id
        || !snapshot.goals.iter().any(|entry| {
            entry.goal == *prepare
                && entry.status == ReplicaGoalStatus::Achieved
                && entry.host_settled
                && entry.selected_call.is_none()
        })
    {
        return false;
    }
    let Some(publication) = snapshot.publication.as_ref() else {
        return false;
    };
    let Some(facts) = snapshot.native_lifecycle.as_ref().filter(|facts| {
        facts.scope == publication.scope
            && facts.scope.epoch == remote.target.scope.epoch
            && facts.scope.configuration.current == remote.target.scope.configuration.current
            && facts.scope.configuration.previous == remote.target.scope.configuration.previous
    }) else {
        return false;
    };
    if publication == &remote.target {
        return facts.state == NativeReplicaState::InBuild
            && facts.native_role == ReplicaRole::IdleSecondary
            && facts.application_role == Some(ReplicaRole::IdleSecondary);
    }
    copied
        && publication.replication_address == remote.target.replication_address
        && publication.stage == ReplicaPublicationStage::AchievedScope
        && publication.role == ReplicaRole::ActiveSecondary
        && facts.state == NativeReplicaState::Ready
        && facts.native_role == ReplicaRole::ActiveSecondary
        && facts.application_role == Some(ReplicaRole::ActiveSecondary)
        && facts.applied_primary_fence == Some(remote.target.scope.epoch)
        && snapshot.goals.iter().any(|entry| {
            entry.goal.scope == publication.scope
                && entry.goal.operation_id == facts.operation_id
                && entry.goal.revision == publication.generation
                && matches!(
                    entry.goal.desired,
                    ReplicaGoalKind::Reconfigure {
                        role: ReplicaRole::ActiveSecondary,
                        catch_up: None
                    }
                )
                && entry.status == ReplicaGoalStatus::Achieved
                && entry.host_settled
                && entry.selected_call.is_none()
        })
}

pub(crate) fn accepted_build(state: &AgentState, remote: &RemoteReplicaSelection) -> bool {
    state
        .public_v1_operation_state
        .as_ref()
        .is_some_and(|preview| {
            preview.operations.values().any(|record| {
                record.intent.goal().is_some_and(|goal| {
                    matches!(goal.desired, ReplicaGoalKind::Build { .. })
                        && goal.remote() == Some(remote)
                }) && record.stage == state::PublicOperationStage::Completed
                    && record.disposition == Some(state::PublicOperationDisposition::Succeeded)
                    && record.lifecycle.in_flight.is_none()
                    && record.ordinary.as_ref().is_some_and(|ordinary| {
                        ordinary.accepted
                            && ordinary.host_settled
                            && ordinary.actions.len() == record.lifecycle.outcomes.len()
                    })
            })
        })
}

pub(crate) fn custody(
    state: &AgentState,
    goal: &ReplicaGoal,
) -> Option<ordinary_goal::reconstruction::InspectionCustody> {
    let preview = state.public_v1_operation_state.as_ref()?;
    let inspection = match &goal.desired {
        ReplicaGoalKind::Build { remote } if remote.original_member.is_some() => {
            let configured = preview.operations.values().find(|record| {
                record.intent.goal().is_some_and(|selected| {
                    selected.scope == goal.scope
                        && selected.scope == remote.source.scope
                        && selected.revision == remote.source.generation
                        && selected.role() == Some(ReplicaRole::Primary)
                }) && record.stage == state::PublicOperationStage::Completed
                    && record.disposition == Some(state::PublicOperationDisposition::Succeeded)
                    && record.lifecycle.in_flight.is_none()
                    && record.ordinary.as_ref().is_some_and(|ordinary| {
                        ordinary.accepted
                            && ordinary.host_settled
                            && ordinary.actions.len() == record.lifecycle.outcomes.len()
                    })
            })?;
            return custody(state, configured.intent.goal()?);
        }
        ReplicaGoalKind::Prepare {
            rebuild: Some(rebuild),
            ..
        } => rebuild.inspection.as_ref(),
        ReplicaGoalKind::Reconfigure { role, .. }
            if matches!(role, ReplicaRole::Primary | ReplicaRole::ActiveSecondary) =>
        {
            if *role == ReplicaRole::Primary {
                preview.operations.values().find_map(|record| {
                    let old = record.intent.goal()?;
                    (matches!(old.desired, ReplicaGoalKind::Discover)
                        && record.lifecycle.outcomes.iter().any(|outcome| {
                            matches!(outcome, state::PublicInstructionOutcome::NativeStatus(status)
                                if status.input.state == NativeReplicaState::StandBy
                                    && status.epoch == NativeInspectionEpoch::StandBy)
                        })
                        && old.scope.process_session_id == goal.scope.process_session_id
                        && old.scope.creation_id == goal.scope.creation_id
                        && old.scope.configuration.current == goal.scope.configuration.current
                        && (old.scope.configuration.previous == goal.scope.configuration.previous
                            || goal.scope.configuration.previous.is_none()
                                && preview.operations.values().any(|joint| {
                                    joint.intent.goal().is_some_and(|joint_goal| {
                                        joint_goal.scope.process_session_id
                                            == goal.scope.process_session_id
                                            && joint_goal.scope.creation_id
                                                == goal.scope.creation_id
                                            && joint_goal.scope.configuration.current
                                                == goal.scope.configuration.current
                                            && joint_goal.scope.configuration.previous
                                                == old.scope.configuration.previous
                                            && matches!(
                                                joint_goal.desired,
                                                ReplicaGoalKind::Reconfigure {
                                                    role: ReplicaRole::Primary,
                                                    catch_up: Some(PublicCatchUpMode::WriteQuorum)
                                                }
                                            )
                                    }) && joint.stage == state::PublicOperationStage::Completed
                                        && joint.disposition
                                            == Some(state::PublicOperationDisposition::Succeeded)
                                        && joint.ordinary.as_ref().is_some_and(|ordinary| {
                                            ordinary.accepted && ordinary.host_settled
                                        })
                                }))
                        && old.scope.epoch == goal.scope.epoch
                        && record.stage == state::PublicOperationStage::Completed
                        && record.disposition == Some(state::PublicOperationDisposition::Succeeded)
                        && record
                            .ordinary
                            .as_ref()
                            .is_some_and(|ordinary| ordinary.accepted && ordinary.host_settled))
                    .then_some(old)
                })?
            } else {
                preview.operations.values().find_map(|record| {
                    let old = record.intent.goal()?;
                    let ReplicaGoalKind::Prepare {
                        rebuild: Some(rebuild),
                        ..
                    } = &old.desired
                    else {
                        return None;
                    };
                    (old.scope.compatible_with(&goal.scope)
                        && old.scope.epoch == goal.scope.epoch
                        && old.scope.configuration.current == goal.scope.configuration.current
                        && record.stage == state::PublicOperationStage::Completed
                        && record.disposition == Some(state::PublicOperationDisposition::Succeeded)
                        && record
                            .ordinary
                            .as_ref()
                            .is_some_and(|ordinary| ordinary.accepted && ordinary.host_settled))
                    .then_some(rebuild.inspection.as_ref())
                })?
            }
        }
        _ => return None,
    };
    validate_inspection(state, inspection).ok()?;
    ordinary_goal::reconstruction::inspection_custody(state, inspection)
}

pub(crate) fn validate_idle(state: &AgentState, goal: &ReplicaGoal) -> Result<()> {
    let ReplicaGoalKind::Prepare {
        rebuild: Some(rebuild),
        ..
    } = &goal.desired
    else {
        return Ok(());
    };
    validate_inspection(state, &rebuild.inspection)
}

fn validate_inspection(state: &AgentState, inspection: &ReplicaGoal) -> Result<()> {
    let preview = state
        .public_v1_operation_state
        .as_ref()
        .ok_or_else(|| HostError::CommandRejected("missing rebuild journal".into()))?;
    let inspected = preview
        .operations
        .get(&inspection.operation_id)
        .filter(|record| {
            record.intent.goal() == Some(inspection)
                && record.stage == state::PublicOperationStage::Completed
                && record.disposition == Some(state::PublicOperationDisposition::Succeeded)
                && record.lifecycle.in_flight.is_none()
                && record
                    .ordinary
                    .as_ref()
                    .is_some_and(|ordinary| ordinary.accepted && ordinary.host_settled)
        })
        .ok_or_else(|| {
            HostError::CommandRejected("rebuild lacks exact accepted inspection".into())
        })?;
    let status = inspected
        .lifecycle
        .outcomes
        .iter()
        .find_map(|outcome| match outcome {
            state::PublicInstructionOutcome::NativeStatus(status) => Some(status),
            _ => None,
        })
        .ok_or_else(|| {
            HostError::CommandRejected("rebuild lacks native inspection result".into())
        })?;
    if status.input.scope != inspection.scope
        || status.input.state != NativeReplicaState::StandBy
        || status.input.native_role != ReplicaRole::None
        || status.input.application_role.is_some()
        || status.epoch != NativeInspectionEpoch::StandBy
        || status.input.deactivation.is_none()
        || ordinary_goal::reconstruction::inspection_custody(state, inspection).is_none()
    {
        return Err(HostError::CommandRejected(
            "rebuild inspection or old-owner custody changed".into(),
        ));
    }
    Ok(())
}
