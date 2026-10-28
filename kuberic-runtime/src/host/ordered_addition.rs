use super::*;
use crate::protocol::public_operations::MembershipAdditionPhase as Phase;

pub(crate) fn selection(goal: &ReplicaGoal) -> Option<&MembershipAdditionRecovery> {
    recovery(goal)?.addition.as_deref()
}

pub(crate) fn recovery(goal: &ReplicaGoal) -> Option<&ContainedCreationRecovery> {
    match &goal.desired {
        ReplicaGoalKind::RecoverContainedCreation { recovery } => Some(recovery),
        _ => None,
    }
}

fn completed(record: &PublicOperationRecord, accepted: bool) -> bool {
    record.stage == PublicOperationStage::Completed
        && record.disposition == Some(PublicOperationDisposition::Succeeded)
        && record.lifecycle.in_flight.is_none()
        && record.ordinary.as_ref().is_some_and(|ordinary| {
            ordinary.actions
                == super::super::public_lifecycle::operation_instructions(&record.intent)
                && ordinary.actions.len() == record.lifecycle.outcomes.len()
                && (!accepted || ordinary.accepted && ordinary.host_settled)
        })
}

pub(crate) fn successful_final(record: &PublicOperationRecord, accepted: bool) -> bool {
    record
        .intent
        .goal()
        .and_then(selection)
        .is_some_and(|addition| addition.phase == Phase::Current)
        && completed(record, accepted)
}

fn same_template(left: &MembershipAdditionRecovery, right: &MembershipAdditionRecovery) -> bool {
    let mut left = left.clone();
    left.phase = right.phase;
    left == *right
}

pub(crate) fn phase_record<'a>(
    state: &'a AgentState,
    goal: &ReplicaGoal,
    phase: Phase,
) -> Option<&'a PublicOperationRecord> {
    let selected = selection(goal)?;
    state
        .public_v1_operation_state
        .as_ref()?
        .operations
        .values()
        .find(|record| {
            record.intent.revision < goal.revision
                && record.intent.goal().is_some_and(|old| {
                    selection(old).is_some_and(|old_selection| {
                        old_selection.phase == phase && same_template(old_selection, selected)
                    }) && recovery(old).is_some_and(|old_recovery| {
                        let mut expected = recovery(goal).unwrap().clone();
                        expected.addition = old_recovery.addition.clone();
                        expected.role = old_recovery.role;
                        expected.catch_up = old_recovery.catch_up;
                        *old_recovery == expected
                    })
                })
                && completed(record, true)
                && record.superseded_by.is_none()
        })
}

pub(crate) fn bootstrap(state: &AgentState, goal: &ReplicaGoal) -> Result<()> {
    let selected = selection(goal).ok_or_else(|| reject("missing addition template"))?;
    let preview = state
        .public_v1_operation_state
        .as_ref()
        .ok_or_else(|| reject("missing addition journal"))?;
    let admitted = preview
        .operations
        .get(&selected.admitted.operation_id)
        .filter(|record| {
            record.intent.goal() == Some(selected.admitted.as_ref())
                && completed(record, true)
                && super::super::public_lifecycle::achieved_configuration_from_record(record)
                    == Some(selected.admitted.scope.configuration.ordinary())
        })
        .ok_or_else(|| reject("addition bootstrap lacks its actual accepted original A"))?;
    let interrupted = preview
        .operations
        .get(&selected.interrupted.operation_id)
        .filter(|record| record.intent.goal() == Some(selected.interrupted.as_ref()))
        .ok_or_else(|| reject("addition recovery changed its immutable pending C"))?;
    let last = preview
        .operations
        .values()
        .filter_map(|record| record.intent.goal())
        .filter(|old| {
            old.scope.process_session_id == selected.interrupted.scope.process_session_id
                && old.role().is_some()
        })
        .max_by_key(|old| old.revision);
    if last != Some(selected.interrupted.as_ref())
        || interrupted.lifecycle.outcomes.len()
            >= interrupted
                .ordinary
                .as_ref()
                .ok_or_else(|| reject("pending addition lacks frozen actions"))?
                .actions
                .len()
        || !recovery(goal)
            .unwrap()
            .predecessor_operations
            .contains(&admitted.intent.operation_id)
        || !recovery(goal)
            .unwrap()
            .predecessor_operations
            .contains(&interrupted.intent.operation_id)
    {
        return Err(reject(
            "addition recovery requires the still-outstanding selected C",
        ));
    }
    let max_epoch = preview
        .operations
        .values()
        .filter(|record| {
            record.intent.revision <= selected.interrupted.revision
                || record.intent.process_session_id == selected.interrupted.scope.process_session_id
        })
        .filter_map(|record| record.intent.goal().map(|old| old.scope.epoch))
        .chain(
            recovery(goal)
                .unwrap()
                .receiver_settlements
                .iter()
                .map(|receiver| receiver.terminal_goal.scope.epoch),
        )
        .max()
        .ok_or_else(|| reject("addition lacks original native fence"))?;
    if max_epoch.data_loss_number != selected.bootstrap_epoch.data_loss_number
        || max_epoch.configuration_number.checked_add(1)
            != Some(selected.bootstrap_epoch.configuration_number)
        || !recovery(goal)
            .unwrap()
            .receiver_settlements
            .iter()
            .any(|settlement| {
                let target = &settlement.terminal_goal.scope;
                target.storage == selected.receiver.storage_incarnation
                    && target.process_session_id != selected.receiver.process_session_id
                    && target.creation_id != selected.receiver.replicator_creation_id
            })
    {
        return Err(reject(
            "addition requires a fresh exact fence and independently excluded receiver",
        ));
    }
    Ok(())
}

pub(crate) fn build_record<'a>(
    state: &'a AgentState,
    goal: &ReplicaGoal,
) -> Option<&'a PublicOperationRecord> {
    let selected = selection(goal)?;
    let bootstrap = phase_record(state, goal, Phase::Bootstrap)?.intent.goal()?;
    state
        .public_v1_operation_state
        .as_ref()?
        .operations
        .values()
        .find(|record| {
            record.intent.revision < goal.revision
                && completed(record, true)
                && record.intent.goal().is_some_and(|build| {
                    let Some(remote) = build.remote() else {
                        return false;
                    };
                    matches!(build.desired, ReplicaGoalKind::Build { .. })
                        && remote.source.scope == bootstrap.scope
                        && remote.target.scope.storage == selected.receiver.storage_incarnation
                        && remote.target.scope.process_session_id
                            == selected.receiver.process_session_id
                        && remote.target.scope.creation_id
                            == selected.receiver.replicator_creation_id
                        && remote.target.scope.configuration == bootstrap.scope.configuration
                        && remote.validate().is_ok()
                })
        })
}

pub(crate) fn matches_build(
    state: &AgentState,
    goal: &ReplicaGoal,
    remote: &RemoteReplicaSelection,
) -> bool {
    selection(goal).is_some_and(|selected| selected.phase == Phase::Joint)
        && build_record(state, goal)
            .is_some_and(|build| build.intent.goal().unwrap().remote() == Some(remote))
}

pub(crate) fn validate(state: &AgentState, goal: &ReplicaGoal) -> Result<()> {
    let Some(selected) = selection(goal) else {
        return Ok(());
    };
    if selected.phase == Phase::Receiver {
        let terminal = recovery(goal)
            .unwrap()
            .receiver_settlements
            .iter()
            .find(|settlement| settlement.terminal_goal.scope.storage == goal.scope.storage)
            .ok_or_else(|| reject("addition receiver lacks its exact old terminal"))?;
        let preview = state
            .public_v1_operation_state
            .as_ref()
            .ok_or_else(|| reject("missing receiver journal"))?;
        let old = preview
            .operations
            .get(&terminal.terminal_goal.operation_id)
            .ok_or_else(|| reject("receiver terminal is not locally retained"))?;
        if old.intent.goal() != Some(terminal.terminal_goal.as_ref())
            || !completed(old, true)
            || terminal.terminal_goal.scope.process_session_id == goal.scope.process_session_id
            || terminal.terminal_goal.scope.creation_id == goal.scope.creation_id
        {
            return Err(reject(
                "addition receiver has not independently joined its predecessor",
            ));
        }
        return Ok(());
    }
    bootstrap(state, goal)?;
    if selected.phase == Phase::Bootstrap {
        return Ok(());
    }
    let bootstrap = phase_record(state, goal, Phase::Bootstrap).ok_or_else(|| {
        reject("addition continuation precedes actual accepted P0 Configuration/All")
    })?;
    super::reconstruction::validate_receipt(state, bootstrap)?;
    let build = build_record(state, goal)
        .ok_or_else(|| reject("addition continuation lacks fresh accepted Build/copy/Ready"))?;
    if selected.phase == Phase::Current && phase_record(state, goal, Phase::Joint).is_none() {
        return Err(reject(
            "addition current precedes actual accepted original-quorum joint All",
        ));
    }
    let remote = build.intent.goal().unwrap().remote().unwrap();
    if goal.scope.configuration.replicas.len() != 1
        || !goal.scope.configuration.replicas.iter().any(|peer| {
            peer.identity == selected.receiver.identity
                && peer.storage_incarnation == selected.receiver.storage_incarnation
                && peer.process_session_id == selected.receiver.process_session_id
                && peer.replication_address == remote.target.replication_address
                && peer.role == ReplicaRole::ActiveSecondary
                && peer.is_up
                && peer.is_ready
        })
    {
        return Err(reject("addition phase changed its real copied receiver"));
    }
    Ok(())
}

/// Intermediate P0 is not supersession. Only this template's exact next work
/// may coexist with the named historical uncertainty.
pub(crate) fn permits_attempt(
    state: &AgentState,
    goal: &ReplicaGoal,
    old: &PublicOperationRecord,
) -> bool {
    let candidate = if selection(goal).is_some_and(|selection| selection.phase != Phase::Receiver) {
        Some(goal)
    } else {
        copy_bootstrap(state, goal)
    };
    candidate.is_some_and(|candidate| {
        let recovery = recovery(candidate).unwrap();
        recovery.predecessor_process_session_id == old.intent.process_session_id
            && recovery
                .predecessor_operations
                .contains(&old.intent.operation_id)
            && validate(state, candidate).is_ok()
    })
}

pub(crate) fn permits_copy(state: &AgentState, goal: &ReplicaGoal) -> bool {
    copy_bootstrap(state, goal).is_some()
}

fn copy_bootstrap<'a>(state: &'a AgentState, goal: &ReplicaGoal) -> Option<&'a ReplicaGoal> {
    if !matches!(goal.desired, ReplicaGoalKind::Build { .. }) {
        return None;
    }
    state
        .public_v1_operation_state
        .as_ref()
        .and_then(|preview| {
            preview.operations.values().find_map(|record| {
                let bootstrap = record.intent.goal()?;
                let selected = selection(bootstrap)?;
                let remote = goal.remote()?;
                (selected.phase == Phase::Bootstrap
                    && completed(record, true)
                    && remote.source.scope == bootstrap.scope
                    && remote.target.scope.storage == selected.receiver.storage_incarnation
                    && remote.target.scope.process_session_id
                        == selected.receiver.process_session_id
                    && remote.target.scope.creation_id == selected.receiver.replicator_creation_id)
                    .then_some(bootstrap)
            })
        })
}

pub(crate) fn pending(state: &AgentState, process: &ProcessSessionId) -> bool {
    let Some(preview) = &state.public_v1_operation_state else {
        return false;
    };
    preview.operations.values().any(|record| {
        record.intent.goal().is_some_and(|goal| {
            goal.scope.process_session_id == *process
                && selection(goal).is_some_and(|selected| selected.phase == Phase::Bootstrap)
                && !preview.operations.values().any(|final_record| {
                    final_record.intent.goal().is_some_and(|final_goal| {
                        selection(final_goal).is_some_and(|final_selection| {
                            final_selection.phase == Phase::Current
                                && same_template(selection(goal).unwrap(), final_selection)
                        })
                    }) && completed(final_record, true)
                })
        })
    })
}
