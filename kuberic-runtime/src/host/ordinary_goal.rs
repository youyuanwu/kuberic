use super::state::*;
use super::{HostError, Result};
use crate::protocol::public_operations::*;
use crate::protocol::types::*;

#[path = "ordered_addition.rs"]
pub(crate) mod addition;
#[path = "reconstruction.rs"]
pub(crate) mod reconstruction;
#[path = "retained.rs"]
pub(crate) mod retained;

fn reject(message: &str) -> HostError {
    HostError::CommandRejected(message.into())
}

pub(crate) fn remote_key(remote: &RemoteReplicaSelection) -> String {
    serde_json::to_string(&(
        &remote.target.scope.storage,
        &remote.target.scope.process_session_id,
        &remote.target.scope.creation_id,
    ))
    .expect("serializable remote incarnation")
}

fn voter(role: ReplicaRole) -> bool {
    matches!(role, ReplicaRole::Primary | ReplicaRole::ActiveSecondary)
}

pub(crate) fn same_fence_swap(previous: &ReplicaGoal, goal: &ReplicaGoal) -> bool {
    let Some(original) = previous.scope.configuration.previous.as_ref() else {
        return false;
    };
    previous.scope.history == goal.scope.history
        && previous.scope.storage == goal.scope.storage
        && previous.scope.process_session_id == goal.scope.process_session_id
        && previous.scope.creation_id == goal.scope.creation_id
        && previous.scope.epoch == goal.scope.epoch
        && original.epoch < goal.scope.epoch
        && original.epoch.data_loss_number == goal.scope.epoch.data_loss_number
        && previous.scope.configuration.current.primary_id == original.primary_id
        && goal.scope.configuration.current.primary_id != original.primary_id
        && goal.scope.configuration.current.write_quorum == original.write_quorum
        && goal.scope.configuration.current.members.len() == original.members.len()
        && original.members.iter().all(|member| {
            goal.scope.configuration.current.members.iter().any(|next| {
                next.identity == member.identity
                    && next.role
                        == if next.identity.replica_id
                            == goal.scope.configuration.current.primary_id
                        {
                            ReplicaRole::Primary
                        } else {
                            ReplicaRole::ActiveSecondary
                        }
            })
        })
        && goal.scope.configuration.previous.as_ref() == Some(original)
        && matches!(
            previous.desired,
            ReplicaGoalKind::Reconfigure {
                role: ReplicaRole::ActiveSecondary,
                catch_up: None,
            }
        )
        && matches!(
            goal.desired,
            ReplicaGoalKind::Discover
                | ReplicaGoalKind::Reconfigure {
                    role: ReplicaRole::Primary | ReplicaRole::ActiveSecondary,
                    ..
                }
        )
}

fn normal_current_bridge<'a>(
    state: &'a AgentState,
    preparation: &PublicOperationRecord,
    goal: &ReplicaGoal,
) -> Option<&'a NativeLifecycleFacts> {
    use PublicInstruction as I;
    use PublicInstructionOutcome as O;
    if let Some(facts) = normal_secondary_bridge(state, preparation, goal) {
        return Some(facts);
    }
    let previous = preparation.intent.goal()?;
    let preview = state.public_v1_operation_state.as_ref()?;
    if goal.scope.configuration.previous.is_some()
        || goal.desired
            != (ReplicaGoalKind::Reconfigure {
                role: ReplicaRole::Primary,
                catch_up: Some(PublicCatchUpMode::WriteQuorum),
            })
        || goal.scope.configuration.current.primary_id != goal.scope.storage.replica.replica_id
        || !settled_native_record(state, preparation)
    {
        return None;
    }
    preview.operations.values().find_map(|record| {
        let joint = record.intent.goal()?;
        let ordinary = record.ordinary.as_ref()?;
        let input = ordinary.native_input.as_ref()?;
        let source = input.source.as_ref()?;
        let query = input.promotion.as_ref()?;
        let original = joint.scope.configuration.previous.as_ref()?;
        let O::NativeLifecycle(facts) = record.lifecycle.outcomes.last()? else {
            return None;
        };
        let boundary = source.deactivation.as_ref()?;
        let source_member = original.members.iter().find(|member| {
            member.identity.replica_id == original.primary_id && member.role == ReplicaRole::Primary
        })?;
        let query_record = preview.operations.get(&query.input.operation_id)?;
        (previous.revision < joint.revision
            && joint.revision < goal.revision
            && same_fence_swap(previous, joint)
            && joint.identity == goal.identity
            && joint.validate().is_ok()
            && joint.scope.history == goal.scope.history
            && joint.scope.storage == goal.scope.storage
            && joint.scope.process_session_id == goal.scope.process_session_id
            && joint.scope.creation_id == goal.scope.creation_id
            && joint.scope.epoch == goal.scope.epoch
            && joint.scope.configuration.current == goal.scope.configuration.current
            && joint.desired == goal.desired
            && settled_native_record(state, record)
            && ordinary.actions
                == [
                    I::ReplicatorRole,
                    I::ProgramEpoch,
                    I::ApplicationRole,
                    I::NativeDataLossDecision,
                    I::Configuration,
                    I::FirstCatchUp,
                    I::NativeComplete,
                ]
            && matches!(
                record.lifecycle.outcomes.as_slice(),
                [
                    O::Done,
                    O::Done,
                    O::ApplicationRole(_),
                    O::Done,
                    O::Done,
                    O::Done,
                    O::NativeLifecycle(_),
                ]
            )
            && !input.fresh_initial_primary
            && input.native_rebuild.is_none()
            && input.lifecycle.state == NativeReplicaState::Ready
            && input.lifecycle.native_role == ReplicaRole::ActiveSecondary
            && input.lifecycle.application_role == Some(ReplicaRole::ActiveSecondary)
            && input.lifecycle.applied_primary_fence == Some(goal.scope.epoch)
            && input
                .lifecycle
                .deactivation
                .as_ref()
                .is_some_and(|history| {
                    history.primary_fence < goal.scope.epoch
                        && history.primary_fence.data_loss_number
                            == goal.scope.epoch.data_loss_number
                })
            && settled_native_record(state, query_record)
            && query_record.intent.goal().is_some_and(|selected| {
                selected.desired == ReplicaGoalKind::Discover
                    && previous.revision < selected.revision
                    && selected.revision < joint.revision
                    && selected.scope.history == joint.scope.history
                    && selected.scope.storage == joint.scope.storage
                    && selected.scope.process_session_id == joint.scope.process_session_id
                    && selected.scope.creation_id == joint.scope.creation_id
                    && selected.scope.epoch == joint.scope.epoch
                    && selected.scope.configuration.current == joint.scope.configuration.current
                    && selected.scope.configuration.previous == joint.scope.configuration.previous
            })
            && matches!(
                query.epoch,
                NativeInspectionEpoch::Applied | NativeInspectionEpoch::AlreadyApplied
            )
            && query.input.state == NativeReplicaState::Ready
            && query.input.native_role == ReplicaRole::ActiveSecondary
            && query.input.application_role == Some(ReplicaRole::ActiveSecondary)
            && query.input.applied_primary_fence == Some(goal.scope.epoch)
            && query.input.deactivation == input.lifecycle.deactivation
            && query
                .progress
                .value()
                .is_some_and(|last| last >= boundary.catch_up_lsn)
            && query.capability.value().is_some_and(|first| first >= 0)
            && source.scope.validate().is_ok()
            && source.operation_id != joint.operation_id
            && source.revision > 0
            && source.scope.history == joint.scope.history
            && source.scope.storage.replica == source_member.identity
            && source.scope.epoch == joint.scope.epoch
            && source.scope.configuration.current == joint.scope.configuration.current
            && source.scope.configuration.previous == joint.scope.configuration.previous
            && source.state == NativeReplicaState::Ready
            && source.native_role == ReplicaRole::ActiveSecondary
            && source.application_role == Some(ReplicaRole::ActiveSecondary)
            && source.applied_primary_fence == Some(goal.scope.epoch)
            && boundary.primary_fence == goal.scope.epoch
            && boundary.catch_up_lsn >= 0
            && joint.scope.configuration.replicas.iter().any(|peer| {
                peer.identity == source.scope.storage.replica
                    && peer.storage_incarnation == source.scope.storage
                    && peer.process_session_id == source.scope.process_session_id
                    && peer.is_up
                    && peer.is_ready
                    && peer.current_progress == boundary.catch_up_lsn
            })
            && facts.state == NativeReplicaState::Ready
            && facts.native_role == ReplicaRole::Primary
            && facts.application_role == Some(ReplicaRole::Primary)
            && facts.applied_primary_fence == Some(goal.scope.epoch)
            && facts.deactivation.as_ref().is_some_and(|history| {
                history.primary_fence == goal.scope.epoch
                    && Some(history.catch_up_lsn) == query.progress.value()
                    && history.catch_up_lsn >= boundary.catch_up_lsn
            }))
        .then_some(facts.as_ref())
    })
}

fn normal_secondary_bridge<'a>(
    state: &'a AgentState,
    preparation: &PublicOperationRecord,
    goal: &ReplicaGoal,
) -> Option<&'a NativeLifecycleFacts> {
    use PublicInstruction as I;
    use PublicInstructionOutcome as O;
    let previous = preparation.intent.goal()?;
    let preview = state.public_v1_operation_state.as_ref()?;
    if goal.scope.configuration.previous.is_some()
        || goal.desired
            != (ReplicaGoalKind::Reconfigure {
                role: ReplicaRole::ActiveSecondary,
                catch_up: None,
            })
        || !settled_native_record(state, preparation)
    {
        return None;
    }
    preview.operations.values().find_map(|record| {
        let joint = record.intent.goal()?;
        let ordinary = record.ordinary.as_ref()?;
        let input = ordinary.native_input.as_ref()?;
        let query = input.promotion.as_ref()?;
        let query_record = preview.operations.get(&query.input.operation_id)?;
        let O::NativeLifecycle(facts) = record.lifecycle.outcomes.last()? else {
            return None;
        };
        (previous.revision < joint.revision
            && joint.revision < goal.revision
            && same_fence_swap(previous, joint)
            && joint.identity == goal.identity
            && joint.scope.history == goal.scope.history
            && joint.scope.storage == goal.scope.storage
            && joint.scope.process_session_id == goal.scope.process_session_id
            && joint.scope.creation_id == goal.scope.creation_id
            && joint.scope.epoch == goal.scope.epoch
            && joint.scope.configuration.current == goal.scope.configuration.current
            && joint.desired == goal.desired
            && settled_native_record(state, record)
            && ordinary.actions
                == [
                    I::ReplicatorRole,
                    I::ApplicationRole,
                    I::ProgramEpoch,
                    I::Configuration,
                    I::NativeComplete,
                ]
            && matches!(
                record.lifecycle.outcomes.as_slice(),
                [
                    O::Done,
                    O::ApplicationRole(_),
                    O::Done,
                    O::Done,
                    O::NativeLifecycle(_),
                ]
            )
            && !input.fresh_initial_primary
            && input.native_rebuild.is_none()
            && input.source.is_none()
            && input.lifecycle.state == NativeReplicaState::Ready
            && input.lifecycle.native_role == ReplicaRole::ActiveSecondary
            && input.lifecycle.application_role == Some(ReplicaRole::ActiveSecondary)
            && input.lifecycle.applied_primary_fence == Some(goal.scope.epoch)
            && settled_native_record(state, query_record)
            && query_record.intent.goal().is_some_and(|selected| {
                selected.desired == ReplicaGoalKind::Discover
                    && previous.revision < selected.revision
                    && selected.revision < joint.revision
                    && selected.scope == query.input.scope
                    && selected.scope.history == joint.scope.history
                    && selected.scope.storage == joint.scope.storage
                    && selected.scope.process_session_id == joint.scope.process_session_id
                    && selected.scope.creation_id == joint.scope.creation_id
                    && selected.scope.epoch == joint.scope.epoch
                    && selected.scope.configuration.current == joint.scope.configuration.current
                    && selected.scope.configuration.previous == joint.scope.configuration.previous
            })
            && matches!(
                query.epoch,
                NativeInspectionEpoch::Applied | NativeInspectionEpoch::AlreadyApplied
            )
            && query.input.state == NativeReplicaState::Ready
            && query.input.native_role == ReplicaRole::ActiveSecondary
            && query.input.application_role == Some(ReplicaRole::ActiveSecondary)
            && query.input.applied_primary_fence == Some(goal.scope.epoch)
            && query.input.deactivation == input.lifecycle.deactivation
            && query.progress.value().is_some_and(|last| last >= 0)
            && query.capability.value().is_some_and(|first| first >= 0)
            && facts.state == NativeReplicaState::Ready
            && facts.native_role == ReplicaRole::ActiveSecondary
            && facts.application_role == Some(ReplicaRole::ActiveSecondary)
            && facts.applied_primary_fence == Some(goal.scope.epoch)
            && facts.deactivation == input.lifecycle.deactivation)
            .then_some(facts.as_ref())
    })
}

pub(super) fn normal_current_secondary<'a>(
    state: &'a AgentState,
    goal: &ReplicaGoal,
) -> Option<&'a NativeLifecycleFacts> {
    state
        .public_v1_operation_state
        .as_ref()?
        .operations
        .values()
        .find_map(|record| normal_secondary_bridge(state, record, goal))
}

pub(super) fn normal_current_owner<'a>(
    state: &'a AgentState,
    goal: &ReplicaGoal,
) -> Option<&'a NativeLifecycleFacts> {
    state
        .public_v1_operation_state
        .as_ref()?
        .operations
        .values()
        .find_map(|record| normal_current_bridge(state, record, goal))
}

fn settled_native_record(state: &AgentState, record: &PublicOperationRecord) -> bool {
    record.intent.goal().is_some_and(|goal| {
        goal.validate().is_ok()
            && record.intent == goal.intent()
            && record.accepted_history.as_ref() == Some(&goal.scope.history)
            && record.accepted_storage.as_ref() == Some(&goal.scope.storage)
    }) && record.stage == PublicOperationStage::Completed
        && record.disposition == Some(PublicOperationDisposition::Succeeded)
        && record.superseded_by.is_none()
        && record.retry_failure.is_none()
        && record.lifecycle.in_flight.is_none()
        && record.ordinary.as_ref().is_some_and(|ordinary| {
            ordinary.action_format == ORDINARY_ACTION_FORMAT
                && ordinary.accepted
                && ordinary.host_settled
                && !ordinary.logically_retired
                && ordinary.native_input.is_some()
                && {
                    let mut expected =
                        super::public_lifecycle::operation_instructions(&record.intent);
                    if ordinary.already_open {
                        if record.intent.goal().is_none_or(|goal| {
                            !matches!(
                                goal.desired,
                                ReplicaGoalKind::Prepare { .. } | ReplicaGoalKind::Discover
                            ) || !opened_creation(state, goal)
                        }) {
                            return false;
                        }
                        expected.retain(|action| {
                            !matches!(
                                action,
                                PublicInstruction::ApplicationOpen
                                    | PublicInstruction::ReplicatorOpen
                            )
                        });
                    }
                    ordinary.actions == expected
                }
                && record.lifecycle.outcomes.len() == ordinary.actions.len()
        })
        && super::native_history::validate_record(state, record).is_ok()
}

pub(super) fn validate_live_normal_current(
    state: &AgentState,
    goal: &ReplicaGoal,
    live: impl Fn(&NativeLifecycleFacts) -> bool,
) -> Result<()> {
    if goal.class().is_terminal() {
        return Ok(());
    }
    let Some(preview) = state.public_v1_operation_state.as_ref() else {
        return Ok(());
    };
    for record in preview.operations.values() {
        let Some(previous) = record.intent.goal() else {
            continue;
        };
        if previous.revision < goal.revision
            && previous.scope.process_session_id == goal.scope.process_session_id
            && !previous.scope.compatible_with(&goal.scope)
            && !matches!(previous.desired, ReplicaGoalKind::Discover)
            && !same_fence_swap(previous, goal)
            && normal_current_bridge(state, record, goal).is_none_or(|facts| !live(facts))
        {
            return Err(reject(
                "normal current-only continuation lost its actual joint native owner",
            ));
        }
    }
    Ok(())
}

fn retained_source_member(
    state: &AgentState,
    goal: &ReplicaGoal,
    peer: &ReplicaPeerDescription,
) -> bool {
    let Some(record) = state
        .public_v1_operation_state
        .as_ref()
        .and_then(|preview| {
            preview
                .operations
                .values()
                .filter(|record| {
                    record.intent.revision < goal.revision
                        && record.intent.process_session_id != goal.scope.process_session_id
                        && record.accepted_history.as_ref() == Some(&goal.scope.history)
                        && record.accepted_storage.as_ref() == Some(&goal.scope.storage)
                        && record.intent.goal().is_some_and(|old| {
                            !matches!(
                                old.desired,
                                ReplicaGoalKind::Build { .. } | ReplicaGoalKind::Remove { .. }
                            )
                        })
                })
                .max_by_key(|record| record.intent.revision)
        })
    else {
        return false;
    };
    let Some(old) = record.intent.goal() else {
        return false;
    };
    // Historical membership only permits attempting native reformation. It
    // supplies neither Ready nor receiver authority; that actual callback must
    // prove and join the higher-epoch retained receiver before access can open.
    record.stage == PublicOperationStage::Completed
        && record.disposition == Some(PublicOperationDisposition::Succeeded)
        && record.lifecycle.in_flight.is_none()
        && old.scope.creation_id != goal.scope.creation_id
        && old.scope.epoch.data_loss_number == goal.scope.epoch.data_loss_number
        && old.scope.epoch.configuration_number < goal.scope.epoch.configuration_number
        && old.role() == Some(ReplicaRole::Primary)
        && old.scope.configuration.previous.is_none()
        && goal.scope.configuration.previous.as_ref() == Some(&old.scope.configuration.current)
        && goal.scope.configuration.current.primary_id == old.scope.configuration.current.primary_id
        && goal.scope.configuration.current.members == old.scope.configuration.current.members
        && super::public_lifecycle::achieved_configuration_from_record(record).is_some()
        && old.scope.configuration.replicas.iter().any(|known| {
            known.identity == peer.identity
                && known.storage_incarnation == peer.storage_incarnation
                && known.process_session_id == peer.process_session_id
                && known.replication_address == peer.replication_address
                && known.role == ReplicaRole::ActiveSecondary
                && peer.role == ReplicaRole::ActiveSecondary
        })
}

fn retained_election_member(
    state: &AgentState,
    goal: &ReplicaGoal,
    peer: &ReplicaPeerDescription,
) -> bool {
    let Some(previous) = goal.scope.configuration.previous.as_ref() else {
        return false;
    };
    let current = &goal.scope.configuration.current;
    if !matches!(
        goal.desired,
        ReplicaGoalKind::Reconfigure {
            role: ReplicaRole::Primary,
            catch_up: Some(_)
        }
    ) || current.primary_id != goal.scope.storage.replica.replica_id
        || current.primary_id == previous.primary_id
        || current.write_quorum != previous.write_quorum
        || current.epoch.data_loss_number != previous.epoch.data_loss_number
        || current.epoch.configuration_number <= previous.epoch.configuration_number
        || !previous.members.iter().any(|member| {
            member.identity == goal.scope.storage.replica
                && member.role == ReplicaRole::ActiveSecondary
        })
        || !current.members.iter().all(|member| {
            previous
                .members
                .iter()
                .any(|old| old.identity == member.identity)
                && (member.identity == goal.scope.storage.replica
                    || member.role == ReplicaRole::ActiveSecondary)
        })
        || !current.members.iter().any(|member| {
            member.identity == peer.identity && member.role == ReplicaRole::ActiveSecondary
        })
        || !previous.members.iter().any(|member| {
            member.identity == peer.identity && member.role == ReplicaRole::ActiveSecondary
        })
        || peer.role != ReplicaRole::ActiveSecondary
    {
        return false;
    }
    state
        .public_v1_operation_state
        .as_ref()
        .is_some_and(|preview| {
            preview.operations.values().any(|record| {
                let Some(old) = record.intent.goal() else {
                    return false;
                };
                record.intent.revision < goal.revision
                    && record.stage == PublicOperationStage::Completed
                    && record.disposition == Some(PublicOperationDisposition::Succeeded)
                    && record.lifecycle.in_flight.is_none()
                    && record
                        .ordinary
                        .as_ref()
                        .is_some_and(|ordinary| ordinary.accepted)
                    && record.accepted_history.as_ref() == Some(&goal.scope.history)
                    && record.accepted_storage.as_ref() == Some(&goal.scope.storage)
                    && old.scope.process_session_id == goal.scope.process_session_id
                    && old.scope.creation_id == goal.scope.creation_id
                    && old.scope.configuration.current == *previous
                    && old.scope.configuration.previous.is_none()
                    && old.role() == Some(ReplicaRole::ActiveSecondary)
                    && super::public_action_state::achieved_role(record, false)
                        == Some(ReplicaRole::ActiveSecondary)
                    && super::public_action_state::achieved_role(record, true)
                        == Some(ReplicaRole::ActiveSecondary)
                    && super::public_lifecycle::achieved_configuration_from_record(record)
                        == Some(old.scope.configuration.ordinary())
                    && old.scope.configuration.replicas.iter().any(|known| {
                        known.identity == peer.identity
                            && known.storage_incarnation == peer.storage_incarnation
                            && known.role == ReplicaRole::ActiveSecondary
                            && known.process_session_id != peer.process_session_id
                    })
            })
        })
}

fn live_source_retained_member(
    state: &AgentState,
    goal: &ReplicaGoal,
    peer: &ReplicaPeerDescription,
) -> bool {
    let Some(previous) = goal.scope.configuration.previous.as_ref() else {
        return false;
    };
    let current = &goal.scope.configuration.current;
    if !matches!(
        goal.desired,
        ReplicaGoalKind::Reconfigure {
            role: ReplicaRole::Primary,
            catch_up: Some(_)
        }
    ) || current.primary_id != goal.scope.storage.replica.replica_id
        || current.primary_id != previous.primary_id
        || current.members != previous.members
        || current.write_quorum != previous.write_quorum
        || current.epoch.data_loss_number != previous.epoch.data_loss_number
        || current.epoch.configuration_number <= previous.epoch.configuration_number
        || peer.role != ReplicaRole::ActiveSecondary
    {
        return false;
    }
    state
        .public_v1_operation_state
        .as_ref()
        .is_some_and(|preview| {
            preview.operations.values().any(|record| {
                let Some(old) = record.intent.goal() else {
                    return false;
                };
                record.intent.revision < goal.revision
                    && record.stage == PublicOperationStage::Completed
                    && record.disposition == Some(PublicOperationDisposition::Succeeded)
                    && record.lifecycle.in_flight.is_none()
                    && record
                        .ordinary
                        .as_ref()
                        .is_some_and(|ordinary| ordinary.accepted)
                    && record.accepted_history.as_ref() == Some(&goal.scope.history)
                    && record.accepted_storage.as_ref() == Some(&goal.scope.storage)
                    && old.scope.process_session_id == goal.scope.process_session_id
                    && old.scope.creation_id == goal.scope.creation_id
                    && old.role() == Some(ReplicaRole::Primary)
                    && old.scope.configuration.previous.is_none()
                    && old.scope.configuration.current == *previous
                    && super::public_lifecycle::achieved_configuration_from_record(record)
                        == Some(old.scope.configuration.ordinary())
                    && old.scope.configuration.replicas.len()
                        == goal.scope.configuration.replicas.len()
                    && old.scope.configuration.replicas.iter().all(|known| {
                        goal.scope.configuration.replicas.iter().any(|next| {
                            next.identity == known.identity
                                && next.storage_incarnation == known.storage_incarnation
                                && next.role == known.role
                                && (next.identity == peer.identity
                                    || (next.process_session_id == known.process_session_id
                                        && next.replication_address == known.replication_address))
                        })
                    })
                    && old.scope.configuration.replicas.iter().any(|known| {
                        known.identity == peer.identity
                            && known.storage_incarnation == peer.storage_incarnation
                            && known.role == ReplicaRole::ActiveSecondary
                            && known.process_session_id != peer.process_session_id
                    })
            })
        })
}

fn new_voters<'a>(
    state: &AgentState,
    goal: &'a ReplicaGoal,
) -> Result<Vec<&'a ReplicaPeerDescription>> {
    // Initial Prepare recovers the bootstrap topology without granting access.
    // Subsequent voting additions must cross the accepted idle-Ready boundary.
    if goal.role() != Some(ReplicaRole::Primary)
        || !matches!(
            goal.desired,
            ReplicaGoalKind::Reconfigure { .. } | ReplicaGoalKind::RefreshPrimary { .. }
        ) && !addition::selection(goal).is_some_and(|selected| {
            matches!(
                selected.phase,
                MembershipAdditionPhase::Joint | MembershipAdditionPhase::Current
            )
        })
    {
        return Ok(Vec::new());
    }
    let previous = state
        .public_v1_operation_state
        .as_ref()
        .and_then(|preview| {
            preview
                .operations
                .values()
                .filter(|record| {
                    record.intent.revision < goal.revision
                        && record.intent.process_session_id == goal.scope.process_session_id
                        && record.accepted_history.as_ref() == Some(&goal.scope.history)
                        && record.accepted_storage.as_ref() == Some(&goal.scope.storage)
                        && record
                            .intent
                            .goal()
                            .is_some_and(|old| old.scope.creation_id == goal.scope.creation_id)
                })
                .filter_map(|record| {
                    super::public_lifecycle::achieved_configuration_from_record(record)
                        .map(|configuration| (record.intent.revision, configuration))
                })
                .max_by_key(|(revision, _)| *revision)
                .map(|(_, configuration)| configuration)
        });
    let members = std::iter::once(&goal.scope.configuration.current)
        .chain(goal.scope.configuration.previous.iter())
        .flat_map(|set| set.members.iter())
        .filter(|member| voter(member.role) && member.identity != goal.scope.storage.replica)
        .map(|member| &member.identity)
        .collect::<std::collections::BTreeSet<_>>();
    members
        .into_iter()
        .map(|identity| {
            goal.scope
                .configuration
                .replicas
                .iter()
                .find(|peer| &peer.identity == identity)
                .ok_or_else(|| reject("new voter is missing its exact peer descriptor"))
        })
        .collect::<Result<Vec<_>>>()
        .map(|peers| {
            peers
                .into_iter()
                .filter(|peer| {
                    if !peer.is_up || !peer.is_ready {
                        return false;
                    }
                    if retained_source_member(state, goal, peer)
                        || live_source_retained_member(state, goal, peer)
                    {
                        return false;
                    }
                    // This is an original same-storage voter, not an idle
                    // addition. Only an attempt is admitted here: the actual
                    // native higher-epoch reformation still requires fresh
                    // checked target receipts/binding/ACKs before any quorum,
                    // successful configuration or serving publication.
                    if retained_election_member(state, goal, peer) {
                        return previous.as_ref().is_some_and(|old| {
                            old.replicas.iter().any(|known| {
                                known.identity == peer.identity && (!known.is_up || !known.is_ready)
                            })
                        });
                    }
                    previous.as_ref().is_none_or(|old| {
                        !std::iter::once(&old.current)
                            .chain(old.previous.iter())
                            .any(|set| {
                                set.members.iter().any(|member| {
                                    member.identity == peer.identity && voter(member.role)
                                })
                            })
                            || !old.replicas.iter().any(|known| {
                                known.identity == peer.identity
                                    && known.storage_incarnation.as_ref()
                                        == Some(&peer.storage_incarnation)
                                    && ((known.process_session_id == peer.process_session_id
                                        && known.replication_address == peer.replication_address)
                                        // An unchanged current member is not a fresh voter.
                                        // The ordinary native configuration callback must
                                        // validate its exact recovered session before its
                                        // receipt/publication can be accepted.
                                        || (old.current == goal.scope.configuration.current
                                            && old.previous.is_none()
                                            && goal.scope.configuration.previous.is_none()
                                            && known.role == ReplicaRole::ActiveSecondary
                                            && peer.role == ReplicaRole::ActiveSecondary
                                            && known.process_session_id != peer.process_session_id))
                            })
                    })
                })
                .collect()
        })
}

fn matches_peer(remote: &RemoteReplicaSelection, peer: &ReplicaPeerDescription) -> bool {
    remote.target.scope.storage.replica == peer.identity
        && remote.target.scope.storage == peer.storage_incarnation
        && remote.target.scope.process_session_id == peer.process_session_id
        && remote.target.replication_address == peer.replication_address
}

fn matches_source(remote: &RemoteReplicaSelection, goal: &ReplicaGoal) -> bool {
    remote.source.scope.history == goal.scope.history
        && remote.source.scope.storage == goal.scope.storage
        && remote.source.scope.process_session_id == goal.scope.process_session_id
        && remote.source.scope.creation_id == goal.scope.creation_id
        && remote.source.scope.epoch == goal.scope.epoch
}

pub(crate) fn admission_ready(
    state: &AgentState,
    goal: &ReplicaGoal,
) -> Result<Vec<RemoteReplicaSelection>> {
    let source = publication(
        state,
        &goal.scope.process_session_id,
        &goal.scope.creation_id,
        false,
    );
    let remotes = remote_observations(state, source.as_ref());
    new_voters(state, goal)?
        .into_iter()
        .map(|peer| {
            remotes
                .iter()
                .find(|remote| {
                    remote.state == RemoteReplicaState::Ready
                        && remote.accepted
                        && matches_peer(&remote.selection, peer)
                        && (matches_source(&remote.selection, goal)
                            || addition::matches_build(state, goal, &remote.selection))
                })
                .map(|remote| remote.selection.clone())
                .ok_or_else(|| reject("new voter admission requires accepted exact Ready"))
        })
        .collect()
}

pub(crate) fn validate_frozen_admissions(
    state: &AgentState,
    record: &PublicOperationRecord,
) -> Result<()> {
    let goal = record
        .intent
        .goal()
        .ok_or_else(|| reject("missing canonical goal"))?;
    let ordinary = record
        .ordinary
        .as_ref()
        .ok_or_else(|| reject("missing frozen actions"))?;
    let peers = new_voters(state, goal)?;
    if peers.len() != ordinary.ready_admissions.len()
        || peers.iter().any(|peer| {
            ordinary
                .ready_admissions
                .iter()
                .filter(|remote| matches_peer(remote, peer))
                .count()
                != 1
        })
        || ordinary.ready_admissions.iter().any(|remote| {
            if !matches_source(remote, goal) && !addition::matches_build(state, goal, remote) {
                return true;
            }
            let Some(preview) = state.public_v1_operation_state.as_ref() else {
                return true;
            };
            !preview.operations.values().any(|build| {
                build.intent.revision < goal.revision
                    && build.intent.goal().is_some_and(|old| {
                        matches!(old.desired, ReplicaGoalKind::Build { .. })
                            && old.remote() == Some(remote)
                    })
                    && build.stage == PublicOperationStage::Completed
                    && build.disposition == Some(PublicOperationDisposition::Succeeded)
                    && build.ordinary.as_ref().is_some_and(|ordinary| {
                        ordinary.accepted
                            && build.lifecycle.in_flight.is_none()
                            && build.lifecycle.outcomes.len() == ordinary.actions.len()
                    })
            }) || preview.operations.values().any(|remove| {
                remove.intent.revision < goal.revision
                    && remove.intent.goal().is_some_and(|old| {
                        matches!(old.desired, ReplicaGoalKind::Remove { .. })
                            && old.remote() == Some(remote)
                    })
            })
        })
    {
        return Err(reject(
            "frozen voter admission lacks exact accepted Ready evidence",
        ));
    }
    Ok(())
}

fn validate_goal_identity<'a>(
    state: &'a AgentState,
    goal: &ReplicaGoal,
) -> Result<&'a PublicV1OperationState> {
    goal.validate().map_err(reject)?;
    let preview = state
        .public_v1_operation_state
        .as_ref()
        .ok_or_else(|| reject("missing goal store"))?;
    if preview.identity != goal.identity
        || preview.history.as_ref() != Some(&goal.scope.history)
        || preview.storage_incarnation.as_ref() != Some(&goal.scope.storage)
        || state.identity.local_identity != goal.scope.storage.replica
    {
        return Err(reject("goal changed exact host history or storage"));
    }
    Ok(preview)
}

fn validate_remote_selection(preview: &PublicV1OperationState, goal: &ReplicaGoal) -> Result<()> {
    if let Some(remote) = goal.remote() {
        let key = remote_key(remote);
        if preview
            .remote_watermarks
            .get(&key)
            .is_some_and(|watermark| (goal.scope.epoch, remote.generation) <= *watermark)
        {
            return Err(reject(
                "remote selection or replay is below the retained rejection watermark",
            ));
        }
        if matches!(goal.desired, ReplicaGoalKind::Build { .. })
            && preview.operations.values().any(|record| {
                record.intent.goal().is_some_and(|old| {
                    matches!(old.desired, ReplicaGoalKind::Remove { .. })
                        && old.remote() == Some(remote)
                })
            })
        {
            return Err(reject(
                "remote Build selection or replay has been tombstoned",
            ));
        }
    }
    Ok(())
}

pub(crate) fn validate_admission(state: &AgentState, goal: &ReplicaGoal) -> Result<()> {
    let preview = validate_goal_identity(state, goal)?;
    validate_remote_selection(preview, goal)?;
    if let Some(existing) = preview.operations.get(&goal.operation_id) {
        return if existing.intent == goal.intent() {
            Ok(())
        } else {
            Err(HostError::DurableEffectConflict(
                "goal ID reused with changed input".into(),
            ))
        };
    }
    if preview
        .operations
        .values()
        .any(|record| record.intent.revision >= goal.revision)
    {
        return Err(reject(
            "goal revision or epoch is stale: revision does not exceed the complete journal high-watermark",
        ));
    }
    if matches!(goal.desired, ReplicaGoalKind::RestoreRetainedMember { .. }) {
        retained::validate_restore(state, goal)?;
    }
    if matches!(
        goal.desired,
        ReplicaGoalKind::RecoverContainedCreation { .. }
    ) {
        addition::validate(state, goal)?;
        if reconstruction::claim(goal).is_some() {
            reconstruction::validate(state, goal)?;
        }
    } else if addition::pending(state, &goal.scope.process_session_id)
        && !goal.class().is_terminal()
        && !addition::permits_copy(state, goal)
    {
        return Err(reject(
            "ordered addition permits only its selected phase/copy continuation",
        ));
    }
    if goal.class().is_terminal()
        && preview.operations.values().any(|record| {
            record.intent.goal().is_some()
                && record.intent.process_session_id == goal.scope.process_session_id
                && record.intent.class.is_terminal()
        })
    {
        return Err(reject(
            "terminal creation already has an immutable goal owner",
        ));
    }
    for record in preview
        .operations
        .values()
        .filter(|record| record.intent.goal().is_some())
    {
        let previous = record.intent.goal().unwrap();
        if previous.scope.epoch > goal.scope.epoch || previous.revision >= goal.revision {
            return Err(reject("goal revision or epoch is stale"));
        }
        if !goal.class().is_terminal() {
            if previous.scope.process_session_id != goal.scope.process_session_id
                && previous.scope.history == goal.scope.history
                && previous.scope.storage == goal.scope.storage
            {
                continue;
            }
            if (previous.class().is_terminal()
                || terminal_for_process(preview, &goal.scope.process_session_id))
                && reconstruction::inspection_custody(state, goal).is_none()
                && super::original_member::custody(state, goal).is_none()
            {
                return Err(reject(
                    "withdrawn creation requires contained process replacement",
                ));
            }
            if !previous.scope.compatible_with(&goal.scope)
                && !matches!(previous.desired, ReplicaGoalKind::Discover)
                && !same_fence_swap(previous, goal)
                && normal_current_bridge(state, record, goal).is_none()
            {
                return Err(reject(
                    "incompatible scope requires whole-creation withdrawal",
                ));
            }
            if record.stage != PublicOperationStage::Completed
                && !matches!(previous.desired, ReplicaGoalKind::Build { .. })
            {
                return Err(reject("earlier ordinary action is unresolved"));
            }
            if matches!(
                record.disposition,
                Some(
                    PublicOperationDisposition::Failed(_)
                        | PublicOperationDisposition::Ambiguous(_)
                )
            ) || record.lifecycle.in_flight.is_some()
                && record.intent.process_session_id != goal.scope.process_session_id
            {
                return Err(reject("selected ordinary call remains uncertain"));
            }
        }
    }
    if matches!(goal.desired, ReplicaGoalKind::RefreshPrimary { .. }) {
        let mut completed = preview
            .operations
            .values()
            .filter(|record| {
                record.intent.process_session_id == goal.scope.process_session_id
                    && record.accepted_history.as_ref() == Some(&goal.scope.history)
                    && record.accepted_storage.as_ref() == Some(&goal.scope.storage)
                    && record
                        .intent
                        .goal()
                        .is_some_and(|old| old.scope.creation_id == goal.scope.creation_id)
                    && record.stage == PublicOperationStage::Completed
                    && record.disposition == Some(PublicOperationDisposition::Succeeded)
                    && record.lifecycle.in_flight.is_none()
            })
            .collect::<Vec<_>>();
        completed.sort_by_key(|record| record.intent.revision);
        for application in [false, true] {
            if completed
                .iter()
                .rev()
                .find_map(|record| super::public_action_state::achieved_role(record, application))
                != Some(ReplicaRole::Primary)
            {
                return Err(reject(
                    "Primary refresh requires actual unchanged native and application Primary roles",
                ));
            }
        }
        if matches!(
            goal.desired,
            ReplicaGoalKind::RefreshPrimary { catch_up: Some(_) }
        ) && goal.scope.configuration.previous.is_none()
            && !completed.iter().any(|record| {
                record.intent.goal().is_some_and(|old| {
                    matches!(
                        old.desired,
                        ReplicaGoalKind::RefreshPrimary {
                            catch_up: Some(PublicCatchUpMode::WriteQuorum),
                        }
                    ) && old.scope.configuration.previous.is_some()
                        && old.scope.epoch == goal.scope.epoch
                        && old.scope.configuration.current == goal.scope.configuration.current
                        && goal.scope.configuration.replicas.iter().all(|peer| {
                            old.scope.configuration.replicas.iter().any(|prior| {
                                prior.identity == peer.identity
                                    && prior.storage_incarnation == peer.storage_incarnation
                                    && prior.process_session_id == peer.process_session_id
                                    && prior.replication_address == peer.replication_address
                                    && prior.role == peer.role
                            })
                        })
                }) && record.ordinary.as_ref().is_some_and(|ordinary| {
                    ordinary.actions.contains(&PublicInstruction::FirstCatchUp)
                        && record.lifecycle.outcomes.len() == ordinary.actions.len()
                })
            })
        {
            return Err(reject(
                "current-only Primary refresh lacks its exact completed joint WriteQuorum",
            ));
        }
    }
    super::original_member::validate_idle(state, goal)?;
    if matches!(goal.desired, ReplicaGoalKind::Prepare { .. })
        && preview.operations.values().any(|record| {
            record.intent.goal().is_some_and(|previous| {
                matches!(previous.desired, ReplicaGoalKind::Prepare { .. })
                    && previous.scope.process_session_id == goal.scope.process_session_id
            })
        })
    {
        return Err(reject("creation already has a preparation goal"));
    }
    if let Some(remote) = goal.remote() {
        let key = remote_key(remote);
        if preview
            .remote_watermarks
            .get(&key)
            .is_some_and(|watermark| (goal.scope.epoch, remote.generation) <= *watermark)
        {
            return Err(reject(
                "remote generation is below retained rejection watermark",
            ));
        }
        let retained = preview
            .operations
            .values()
            .filter(|record| {
                record.intent.goal().is_some_and(|old| {
                    old.remote().is_some() && old.scope.creation_id == goal.scope.creation_id
                })
            })
            .collect::<Vec<_>>();
        if matches!(goal.desired, ReplicaGoalKind::Remove { .. })
            && retained.iter().any(|record| {
                record.intent.goal().is_some_and(|previous| {
                    matches!(previous.desired, ReplicaGoalKind::Build { .. })
                        && previous.remote() == Some(remote)
                }) && record.stage != PublicOperationStage::Completed
            })
        {
            return Err(reject(
                "InBuild cancellation requires a whole-creation Abort goal, never Remove",
            ));
        }
        if retained.len() >= MAX_REMOTE_SELECTIONS
            || retained
                .iter()
                .filter_map(|record| {
                    record
                        .intent
                        .goal()
                        .and_then(ReplicaGoal::remote)
                        .and_then(|previous| {
                            (remote_key(previous) == key
                                && previous.source.scope.epoch == goal.scope.epoch)
                                .then_some(previous.generation)
                        })
                })
                .collect::<std::collections::BTreeSet<_>>()
                .len()
                >= MAX_REMOTE_GENERATIONS
        {
            return Err(HostError::Backpressure(
                "retained remote-result budget exhausted; unresolved work cannot be reclaimed"
                    .into(),
            ));
        }
        if matches!(goal.desired, ReplicaGoalKind::Remove { .. })
            && preview.operations.values().any(|record| {
                record.intent.goal().is_some_and(|previous| {
                    matches!(previous.desired, ReplicaGoalKind::Remove { .. })
                        && previous.remote() == goal.remote()
                })
            })
        {
            return Err(reject("remote removal already has an immutable goal owner"));
        }
        let current_generation = retained
            .iter()
            .filter_map(|record| {
                let previous = record.intent.goal()?.remote()?;
                (remote_key(previous) == key && previous.source.scope.epoch == goal.scope.epoch)
                    .then_some(previous.generation)
            })
            .chain(
                preview
                    .remote_watermarks
                    .get(&key)
                    .and_then(|(epoch, generation)| {
                        (*epoch == goal.scope.epoch).then_some(*generation)
                    }),
            )
            .max()
            .unwrap_or(0);
        let removes_ready_generation = matches!(goal.desired, ReplicaGoalKind::Remove { .. })
            && retained.iter().any(|record| {
                record.intent.goal().is_some_and(|previous| {
                    matches!(previous.desired, ReplicaGoalKind::Build { .. })
                        && previous.remote() == Some(remote)
                })
            });
        if !removes_ready_generation && current_generation.checked_add(1) != Some(remote.generation)
        {
            return Err(reject(
                "remote generation differs from the next host-local selection",
            ));
        }
        if matches!(goal.desired, ReplicaGoalKind::Build { .. }) {
            let current = publication(
                state,
                &goal.scope.process_session_id,
                &goal.scope.creation_id,
                false,
            );
            if current.as_ref() != Some(&remote.source) {
                return Err(reject(
                    "Build source publication is no longer exact and achieved",
                ));
            }
            if retained.iter().any(|record| {
                record
                    .intent
                    .goal()
                    .and_then(ReplicaGoal::remote)
                    .is_some_and(|previous| {
                        remote_key(previous) == key
                            && (matches!(
                                record.intent.goal().unwrap().desired,
                                ReplicaGoalKind::Build { .. }
                            ) || previous.generation >= remote.generation
                                || record.stage != PublicOperationStage::Completed
                                || record
                                    .ordinary
                                    .as_ref()
                                    .is_none_or(|ordinary| !ordinary.logically_retired))
                    })
            }) {
                return Err(reject("remote generation is active or tombstoned"));
            }
        } else if publication(
            state,
            &goal.scope.process_session_id,
            &goal.scope.creation_id,
            false,
        )
        .as_ref()
        .is_none_or(|current| {
            current.scope != goal.scope
                || current.stage != ReplicaPublicationStage::AchievedScope
                || current.role != ReplicaRole::Primary
        }) {
            return Err(reject(
                "ordinary Remove requires a live achieved Primary source",
            ));
        }
    }
    Ok(())
}

pub(crate) fn terminal_for_process(
    preview: &PublicV1OperationState,
    process: &ProcessSessionId,
) -> bool {
    if !preview.terminal {
        return false;
    }
    if reconstruction::attempting_process(preview, process) {
        return false;
    }
    let terminals = preview
        .operations
        .values()
        .filter(|record| record.intent.class.is_terminal())
        .collect::<Vec<_>>();
    if !terminals.is_empty()
        && terminals.iter().all(|record| {
            retained::fault_dependency_replaced(preview, record, process)
                || reconstruction::dependency_replaced(preview, record, process)
                || super::original_member::dependency_replaced(preview, record, process)
        })
    {
        return false;
    }
    // An older clean Close cannot clear a later preparation or join failure.
    let settled_boundary = terminals
        .iter()
        .map(|record| record.intent.revision)
        .max()
        .unwrap_or(0);
    terminals.is_empty()
        || preview.operations.values().any(|record| {
            record.intent.revision > settled_boundary
                && (record.stage != PublicOperationStage::Completed
                    || record.disposition != Some(PublicOperationDisposition::Succeeded)
                    || record.lifecycle.in_flight.is_some())
        })
        || terminals.iter().any(|record| {
            if retained::fault_dependency_replaced(preview, record, process)
                || reconstruction::dependency_replaced(preview, record, process)
            {
                return false;
            }
            record.intent.goal().is_none()
                || &record.intent.process_session_id == process
                || record.stage != PublicOperationStage::Completed
                || record.disposition != Some(PublicOperationDisposition::Succeeded)
                || record.lifecycle.in_flight.is_some()
        })
}

pub(crate) fn builtin_construction_source(
    state: &AgentState,
    goal: &ReplicaGoal,
) -> Option<OperationId> {
    state
        .public_v1_operation_state
        .as_ref()?
        .operations
        .values()
        .filter(|record| {
            record.intent.revision < goal.revision
                && record.accepted_history.as_ref() == Some(&goal.scope.history)
                && record.accepted_storage.as_ref() == Some(&goal.scope.storage)
        })
        .filter_map(|record| {
            record
                .ordinary
                .as_ref()?
                .actions
                .iter()
                .zip(&record.lifecycle.outcomes)
                .find(|(instruction, _)| **instruction == PublicInstruction::ReplicatorOpen)
                .map(|(_, outcome)| (record, outcome))
        })
        .max_by_key(|(record, _)| record.intent.revision)
        .and_then(|(record, outcome)| {
            matches!(outcome,
                PublicInstructionOutcome::BuiltinNativeEndpoint {
                    process_id, process_lifetime, storage_owner, ..
                } if *process_id != 0 && !process_lifetime.is_empty()
                    && storage_owner.as_ref() == Some(&record.intent.process_session_id)
            )
            .then(|| record.intent.operation_id.clone())
        })
}

pub(crate) fn capture_creation_owner(
    state: &AgentState,
    goal: &ReplicaGoal,
) -> super::state::OrdinaryCreationOwner {
    super::state::OrdinaryCreationOwner {
        process_id: std::process::id(),
        process_lifetime: retained::process_lifetime_id().to_owned(),
        process_session_id: goal.scope.process_session_id.clone(),
        creation_id: goal.scope.creation_id.clone(),
        builtin_factory: builtin_construction_source(state, goal),
    }
}

pub(crate) fn validate_creation_owner<'a>(
    state: &AgentState,
    record: &'a PublicOperationRecord,
) -> Result<&'a super::state::OrdinaryCreationOwner> {
    let goal = record
        .intent
        .goal()
        .ok_or_else(|| reject("owner stamp lacks an ordinary goal"))?;
    let owner = record
        .ordinary
        .as_ref()
        .and_then(|ordinary| ordinary.creation_owner.as_ref())
        .ok_or_else(|| reject("ordinary21 goal lacks its pre-effect physical owner stamp"))?;
    if owner.process_id == 0
        || owner.process_lifetime.is_empty()
        || owner.process_session_id != goal.scope.process_session_id
        || owner.creation_id != goal.scope.creation_id
        || owner.builtin_factory != builtin_construction_source(state, goal)
        || record.lifecycle.outcomes.iter().any(|outcome| {
            matches!(outcome, PublicInstructionOutcome::BuiltinNativeEndpoint {
                process_id, process_lifetime, ..
            } if *process_id != owner.process_id || *process_lifetime != owner.process_lifetime)
        })
    {
        return Err(reject(
            "ordinary owner or bound builtin provenance crossed its selected creation",
        ));
    }
    Ok(owner)
}

pub(crate) fn freeze(state: &AgentState, goal: &ReplicaGoal) -> Result<OrdinaryGoalRecord> {
    if let Some(existing) = state
        .public_v1_operation_state
        .as_ref()
        .and_then(|preview| preview.operations.get(&goal.operation_id))
        .filter(|record| record.intent == goal.intent())
    {
        return existing
            .ordinary
            .clone()
            .ok_or_else(|| reject("goal lacks frozen local actions"));
    }
    let mut actions = super::public_lifecycle::operation_instructions(&goal.intent());
    if (super::original_member::round(state, goal) || normal_current_owner(state, goal).is_some())
        && matches!(
            goal.desired,
            ReplicaGoalKind::Reconfigure {
                role: ReplicaRole::Primary,
                catch_up: Some(PublicCatchUpMode::WriteQuorum)
            }
        )
        && goal.scope.configuration.previous.is_none()
    {
        actions = super::original_member::final_actions(goal);
    }
    let already_open = matches!(
        goal.desired,
        ReplicaGoalKind::Prepare { .. } | ReplicaGoalKind::Discover
    ) && opened_creation(state, goal);
    if already_open {
        actions.retain(|instruction| {
            !matches!(
                instruction,
                PublicInstruction::ApplicationOpen | PublicInstruction::ReplicatorOpen
            )
        });
    }
    if let ReplicaGoalKind::Remove { remote } = &goal.desired {
        let ready = state
            .public_v1_operation_state
            .as_ref()
            .is_some_and(|preview| {
                preview.operations.values().any(|record| {
                    record.intent.goal().is_some_and(|old| {
                        matches!(old.desired, ReplicaGoalKind::Build { .. })
                            && old.remote() == Some(remote.as_ref())
                    }) && record.stage == PublicOperationStage::Completed
                        && record.disposition == Some(PublicOperationDisposition::Succeeded)
                })
            });
        if !ready {
            actions.clear();
        }
    }
    Ok(OrdinaryGoalRecord {
        action_format: ORDINARY_ACTION_FORMAT,
        actions,
        open_existing: matches!(
            goal.desired,
            ReplicaGoalKind::Discover
                | ReplicaGoalKind::Prepare { .. }
                | ReplicaGoalKind::RestoreRetainedMember { .. }
                | ReplicaGoalKind::RecoverContainedCreation { .. }
        )
        .then_some(state.application_open_mode() == crate::application::OpenMode::Existing),
        already_open,
        accepted: false,
        logically_retired: false,
        host_settled: false,
        ready_admissions: admission_ready(state, goal)?,
        retained: None,
        reconstruction: None,
        creation_owner: None,
        native_input: None,
    })
}

pub(crate) fn opened_creation(state: &AgentState, goal: &ReplicaGoal) -> bool {
    state
        .public_v1_operation_state
        .as_ref()
        .is_some_and(|preview| {
            preview.operations.values().any(|record| {
                record.intent.operation_id != goal.operation_id
                    && record.intent.goal().is_some_and(|previous| {
                        (previous.scope.compatible_with(&goal.scope)
                            || matches!(previous.desired, ReplicaGoalKind::Discover)
                                && previous.scope.history == goal.scope.history
                                && previous.scope.storage == goal.scope.storage
                                && previous.scope.process_session_id
                                    == goal.scope.process_session_id
                                && previous.scope.creation_id == goal.scope.creation_id
                                && previous.scope.epoch <= goal.scope.epoch)
                            && matches!(
                                previous.desired,
                                ReplicaGoalKind::Discover | ReplicaGoalKind::Prepare { .. }
                            )
                    })
                    && record.stage == PublicOperationStage::Completed
                    && record.disposition == Some(PublicOperationDisposition::Succeeded)
                    && record
                        .lifecycle
                        .outcomes
                        .iter()
                        .any(|outcome| outcome.endpoint().is_some())
            })
        })
}

pub(crate) fn publication(
    state: &AgentState,
    process: &ProcessSessionId,
    creation: &ReplicatorCreationId,
    withdrawn: bool,
) -> Option<ReplicaPublication> {
    let preview = state.public_v1_operation_state.as_ref()?;
    let inspection_only = terminal_for_process(preview, process);
    if withdrawn {
        return None;
    }
    let mut records = preview
        .operations
        .values()
        .filter(|record| {
            record.intent.goal().is_some_and(|goal| {
                &goal.scope.process_session_id == process
                    && &goal.scope.creation_id == creation
                    && Some(&goal.scope.history) == preview.history.as_ref()
                    && Some(&goal.scope.storage) == preview.storage_incarnation.as_ref()
                    && (goal.role().is_some() || matches!(goal.desired, ReplicaGoalKind::Discover))
            })
        })
        .collect::<Vec<_>>();
    records.sort_by_key(|record| record.intent.revision);
    let opened = records.iter().find_map(|record| {
        record
            .lifecycle
            .outcomes
            .iter()
            .find_map(|outcome| outcome.endpoint().map(str::to_owned))
    })?;
    // A later inspection round cannot erase an actually achieved role/scope.
    let record = records
        .iter()
        .rev()
        .find(|record| {
            (!inspection_only
                || record
                    .intent
                    .goal()
                    .is_some_and(|goal| super::original_member::custody(state, goal).is_some()))
                && record
                    .intent
                    .goal()
                    .is_some_and(|goal| goal.role().is_some())
        })
        .copied()
        .or_else(|| records.last().copied())?;
    let goal = record.intent.goal()?;
    if inspection_only
        && super::original_member::custody(state, goal).is_none()
        && (reconstruction::inspection_custody(state, goal).is_none()
            || record.stage != PublicOperationStage::Completed
            || record.disposition != Some(PublicOperationDisposition::Succeeded))
    {
        return None;
    }
    if record.superseded_by.is_some()
        || matches!(
            record.disposition,
            Some(
                PublicOperationDisposition::Failed(_)
                    | PublicOperationDisposition::Cancelled
                    | PublicOperationDisposition::Ambiguous(_)
            )
        )
    {
        return None;
    }
    let achieved = goal.role().is_some()
        && record.stage == PublicOperationStage::Completed
        && record.disposition == Some(PublicOperationDisposition::Succeeded)
        && record.ordinary.as_ref().is_some_and(|ordinary| {
            record.lifecycle.outcomes.len() == ordinary.actions.len()
                && record.lifecycle.in_flight.is_none()
        });
    let mut scope = goal.scope.clone();
    if !achieved
        && let Some(PublicOperationProgram::Swap { refreshed, .. }) =
            super::public_lifecycle::program::ordinary_handoff(goal)
    {
        scope.configuration.current = refreshed.current;
        for peer in &mut scope.configuration.replicas {
            if let Some(member) = scope
                .configuration
                .current
                .members
                .iter()
                .find(|member| member.identity == peer.identity)
            {
                peer.role = member.role;
            }
        }
    }
    Some(ReplicaPublication {
        format: ORDINARY_ACTION_FORMAT,
        scope,
        replication_address: opened,
        generation: goal.revision,
        stage: if achieved {
            ReplicaPublicationStage::AchievedScope
        } else {
            ReplicaPublicationStage::DiscoveryPending
        },
        role: if achieved {
            goal.role()?
        } else {
            ReplicaRole::None
        },
    })
}

pub(super) fn completed_handoff_receipts(
    record: &PublicOperationRecord,
) -> Option<Vec<SourceHandoffReceipt>> {
    use PublicInstruction as I;
    use PublicInstructionOutcome as O;
    let ordinary = record.ordinary.as_ref()?;
    let goal = record.intent.goal()?;
    super::public_lifecycle::program::ordinary_handoff(goal)?;
    if !matches!(
        goal.desired,
        ReplicaGoalKind::Reconfigure {
            catch_up: Some(PublicCatchUpMode::All),
            ..
        }
    ) {
        return None;
    }
    if record.stage != PublicOperationStage::Completed
        || ordinary.action_format != ORDINARY_ACTION_FORMAT
        || record.disposition != Some(PublicOperationDisposition::Succeeded)
        || record.superseded_by.is_some()
        || !ordinary.accepted
        || !ordinary.host_settled
        || record.lifecycle.in_flight.is_some()
        || ordinary.actions
            != [
                I::Revoke,
                I::ProgramEpoch,
                I::RefreshedConfiguration,
                I::SecondCatchUp,
                I::ReplicatorRole,
                I::ApplicationRole,
                I::NativeComplete,
            ]
        || !matches!(
            record.lifecycle.outcomes.as_slice(),
            [
                O::Done,
                O::Done,
                O::Done,
                O::Done,
                O::Done,
                O::ApplicationRole(_),
                O::NativeLifecycle(_) | O::NativeUnavailable
            ]
        )
    {
        return None;
    }
    Some(SourceHandoffReceipt::ORDERED.to_vec())
}

pub(crate) fn snapshot(
    state: &AgentState,
    process: ProcessSessionId,
    creation: ReplicatorCreationId,
    withdrawn: bool,
) -> ReplicaAgentSnapshot {
    let publication = publication(state, &process, &creation, withdrawn);
    let goals = state
        .public_v1_operation_state
        .iter()
        .flat_map(|preview| preview.operations.values())
        .filter_map(|record| {
            let goal = record.intent.goal()?.clone();
            let status = match (&record.stage, &record.disposition) {
                _ if record.intent.process_session_id != process
                    && record.stage != PublicOperationStage::Completed =>
                {
                    ReplicaGoalStatus::Uncertain(
                        "ordinary call belongs to a predecessor process".into(),
                    )
                }
                (PublicOperationStage::Completed, Some(PublicOperationDisposition::Succeeded)) => {
                    ReplicaGoalStatus::Achieved
                }
                (_, Some(PublicOperationDisposition::Failed(error))) => {
                    ReplicaGoalStatus::Failed(error.clone())
                }
                (_, Some(PublicOperationDisposition::Ambiguous(error))) => {
                    ReplicaGoalStatus::Uncertain(error.clone())
                }
                (_, Some(PublicOperationDisposition::Cancelled)) => {
                    ReplicaGoalStatus::Uncertain("creation withdrawn".into())
                }
                _ => ReplicaGoalStatus::Pending,
            };
            Some(ReplicaGoalObservation {
                completed_handoff: completed_handoff_receipts(record),
                goal,
                status,
                selected_call: record
                    .lifecycle
                    .in_flight
                    .map(|instruction| format!("{instruction:?}")),
                receipts: record.lifecycle.outcomes.len(),
                host_settled: record
                    .ordinary
                    .as_ref()
                    .is_some_and(|ordinary| ordinary.host_settled),
            })
        })
        .collect();
    let remote_operations = remote_observations(state, publication.as_ref());
    ReplicaAgentSnapshot {
        format: ORDINARY_ACTION_FORMAT,
        identity: state
            .public_v1_operation_state
            .as_ref()
            .unwrap()
            .identity
            .clone(),
        history: state
            .public_v1_operation_state
            .as_ref()
            .unwrap()
            .history
            .clone()
            .unwrap(),
        storage: state
            .public_v1_operation_state
            .as_ref()
            .unwrap()
            .storage_incarnation
            .clone()
            .unwrap(),
        open_mode: state.application_open_mode(),
        read_status: AccessStatus::NotPrimary,
        write_status: AccessStatus::NotPrimary,
        withdrawn,
        provider_writer_held: false,
        process_session_id: process.clone(),
        creation_id: creation,
        publication,
        goals,
        remote_operations,
        progress: ProgressQueryResult::NotQueried("ordinary progress has not been sampled".into()),
        capability: ProgressQueryResult::NotQueried(
            "ordinary capability has not been sampled".into(),
        ),
        native_status: None,
        native_lifecycle: None,
        accepted_revision_high_watermark: state
            .public_v1_operation_state
            .as_ref()
            .into_iter()
            .flat_map(|preview| preview.operations.values())
            .map(|record| record.intent.revision)
            .max()
            .unwrap_or(0),
        retained_recovery_exit: retained::status(state, &process),
        retained_member_qualification: retained::qualification_for_process(state, &process),
        reconstruction_dependencies: reconstruction::journal_dependencies(state),
    }
}

pub(crate) fn remote_observations(
    state: &AgentState,
    source: Option<&ReplicaPublication>,
) -> Vec<RemoteReplicaObservation> {
    let Some(preview) = state.public_v1_operation_state.as_ref() else {
        return Vec::new();
    };
    preview
        .operations
        .values()
        .filter_map(|record| {
            let goal = record.intent.goal()?;
            let remote = goal.remote()?;
            let ordinary = record.ordinary.as_ref()?;
            let removed = preview.operations.values().any(|record| {
                record.intent.goal().is_some_and(|other| {
                    matches!(other.desired, ReplicaGoalKind::Remove { .. })
                        && other.remote() == Some(remote)
                })
            });
            let state = match &goal.desired {
                ReplicaGoalKind::Remove { .. }
                    if record.stage == PublicOperationStage::Completed
                        && record.disposition == Some(PublicOperationDisposition::Succeeded) =>
                {
                    RemoteReplicaState::Dropped
                }
                ReplicaGoalKind::Remove { .. } => RemoteReplicaState::InDrop,
                _ if removed || ordinary.logically_retired => RemoteReplicaState::Dropped,
                _ if source != Some(&remote.source) || record.superseded_by.is_some() => {
                    RemoteReplicaState::Uncertain
                }
                _ if record.stage == PublicOperationStage::Completed
                    && record.disposition == Some(PublicOperationDisposition::Succeeded)
                    && record.lifecycle.in_flight.is_none()
                    && record.lifecycle.outcomes.len() == ordinary.actions.len() =>
                {
                    RemoteReplicaState::Ready
                }
                _ if record.stage == PublicOperationStage::ContainmentPending
                    || record.disposition.is_some() =>
                {
                    RemoteReplicaState::Uncertain
                }
                _ if record.lifecycle.in_flight.is_some() => RemoteReplicaState::InBuild,
                _ => RemoteReplicaState::Absent,
            };
            Some(RemoteReplicaObservation {
                operation_id: goal.operation_id.clone(),
                selection: remote.clone(),
                state,
                accepted: ordinary.accepted,
            })
        })
        .collect()
}

pub(crate) fn validate_acknowledgement(
    state: &AgentState,
    acknowledgement: &ReplicaGoalAcknowledgement,
) -> Result<()> {
    let goal = &acknowledgement.goal;
    let preview = validate_goal_identity(state, goal)?;
    let record = preview
        .operations
        .get(&goal.operation_id)
        .ok_or_else(|| reject("unknown ordinary goal"))?;
    if record.intent != goal.intent()
        || !matches!(
            record.stage,
            PublicOperationStage::Completed | PublicOperationStage::ContainmentPending
        )
        || record.disposition.is_none()
        || (record.lifecycle.in_flight.is_some()
            && record.disposition == Some(PublicOperationDisposition::Succeeded))
    {
        return Err(reject("ordinary goal is changed, uncertain, or unsettled"));
    }
    // A retained Remove result acknowledges local retirement, not a new remote
    // selection. Its own accepted watermark must not hide that exact result.
    if matches!(goal.desired, ReplicaGoalKind::Build { .. }) {
        validate_remote_selection(preview, goal)?;
    }
    let ordinary = record
        .ordinary
        .as_ref()
        .ok_or_else(|| HostError::Corrupt("ordinary goal lacks frozen actions".into()))?;
    if matches!(goal.desired, ReplicaGoalKind::RestoreRetainedMember { .. }) {
        retained::validate_receipt(state, record)?;
        if record.superseded_by.is_some() {
            return Err(reject("restored endpoint was superseded before acceptance"));
        }
    }
    if reconstruction::claim(goal).is_some() {
        reconstruction::validate_receipt(state, record)?;
        if record.superseded_by.is_some() {
            return Err(reject(
                "reconstructed endpoint was superseded before acceptance",
            ));
        }
    }
    if acknowledgement.logically_retired && !ordinary.logically_retired {
        return Err(reject(
            "controller acknowledgement cannot invent local idle retirement",
        ));
    }
    Ok(())
}
