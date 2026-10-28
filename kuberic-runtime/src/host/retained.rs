use super::*;

pub(crate) fn process_lifetime_id() -> &'static str {
    static LIFETIME: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    LIFETIME.get_or_init(|| uuid::Uuid::new_v4().to_string())
}

fn complete(record: &PublicOperationRecord) -> bool {
    record.stage == PublicOperationStage::Completed
        && record.disposition == Some(PublicOperationDisposition::Succeeded)
        && record.lifecycle.in_flight.is_none()
        && record
            .ordinary
            .as_ref()
            .is_some_and(|ordinary| record.lifecycle.outcomes.len() == ordinary.actions.len())
}

pub(in crate::host) fn historical<'a>(
    state: &'a AgentState,
    history: &HistoryContext,
    storage: &ReplicaStorageIncarnationContext,
    qualification: &RetainedMemberQualification,
) -> Result<(&'a PublicOperationRecord, u32, String)> {
    let preview = state
        .public_v1_operation_state
        .as_ref()
        .ok_or_else(|| reject("missing retained journal"))?;
    if state.identity.local_identity != storage.replica
        || preview
            .binding
            .as_ref()
            .is_none_or(|binding| binding.state_persistence != StatePersistence::Persisted)
    {
        return Err(reject(
            "retained history/storage or recovery obligations do not qualify",
        ));
    }
    let record = preview
        .operations
        .get(&qualification.admitted_operation_id)
        .ok_or_else(|| reject("missing historical admitted secondary"))?;
    let old = record
        .intent
        .goal()
        .ok_or_else(|| reject("historical admission is not an ordinary goal"))?;
    if record.intent.revision != qualification.admitted_revision
        || record.intent.identity != preview.identity
        || record.intent.process_session_id != qualification.predecessor_process_session_id
        || old.scope.creation_id != qualification.predecessor_creation_id
        || old.scope.history != *history
        || old.scope.storage != *storage
        || record.accepted_history.as_ref() != Some(history)
        || record.accepted_storage.as_ref() != Some(storage)
        || old.scope.configuration.current != qualification.configuration
        || old.scope.configuration.previous.is_some()
        || old.scope.epoch != qualification.configuration.epoch
        || old.role() != Some(ReplicaRole::ActiveSecondary)
        || !complete(record)
        || record
            .ordinary
            .as_ref()
            .is_none_or(|ordinary| !ordinary.accepted)
        || super::super::public_lifecycle::achieved_configuration_from_record(record)
            .is_none_or(|configuration| configuration != old.scope.configuration.ordinary())
    {
        return Err(reject(
            "historical current-only secondary lacks exact accepted receipts",
        ));
    }
    // A configuration-only successor may retain earlier paired role receipts,
    // but never a later failed/selected role or an interrupted PC obligation.
    for application in [false, true] {
        if preview
            .operations
            .values()
            .filter(|candidate| {
                candidate.intent.revision <= record.intent.revision
                    && candidate.accepted_history.as_ref() == Some(history)
                    && candidate.accepted_storage.as_ref() == Some(storage)
                    && candidate.intent.goal().is_some_and(|goal| {
                        goal.scope.process_session_id
                            == qualification.predecessor_process_session_id
                            && goal.scope.creation_id == qualification.predecessor_creation_id
                    })
                    && complete(candidate)
            })
            .filter_map(|candidate| {
                super::super::public_action_state::achieved_role(candidate, application)
                    .map(|role| (candidate.intent.revision, role))
            })
            .max_by_key(|(revision, _)| *revision)
            .map(|(_, role)| role)
            != Some(ReplicaRole::ActiveSecondary)
        {
            return Err(reject(
                "historical native/application secondary roles are not paired",
            ));
        }
    }
    let (process_id, lifetime) = preview
        .operations
        .values()
        .filter(|candidate| {
            candidate.intent.revision <= record.intent.revision
                && candidate.accepted_history.as_ref() == Some(history)
                && candidate.accepted_storage.as_ref() == Some(storage)
                && candidate.intent.goal().is_some_and(|goal| {
                    goal.scope.process_session_id == qualification.predecessor_process_session_id
                        && goal.scope.creation_id == qualification.predecessor_creation_id
                })
                && complete(candidate)
        })
        .flat_map(|candidate| {
            super::super::public_lifecycle::record_instructions(candidate)
                .into_iter()
                .zip(&candidate.lifecycle.outcomes)
        })
        .find_map(|(instruction, outcome)| match (instruction, outcome) {
            (
                PublicInstruction::ReplicatorOpen,
                PublicInstructionOutcome::BuiltinNativeEndpoint {
                    process_id,
                    process_lifetime,
                    storage_owner,
                    ..
                },
            ) if *process_id != 0
                && !process_lifetime.is_empty()
                && storage_owner.as_ref()
                    == Some(&qualification.predecessor_process_session_id) =>
            {
                Some((*process_id, process_lifetime.clone()))
            }
            _ => None,
        })
        .ok_or_else(|| reject("historical creation lacks actual builtin factory registration"))?;
    Ok((record, process_id, lifetime))
}

fn eligible(
    state: &AgentState,
    history: &HistoryContext,
    storage: &ReplicaStorageIncarnationContext,
) -> Result<()> {
    let preview = state
        .public_v1_operation_state
        .as_ref()
        .ok_or_else(|| reject("missing retained journal"))?;
    if preview.history.as_ref() != Some(history)
        || preview.storage_incarnation.as_ref() != Some(storage)
        || state.application_open_mode() != OpenMode::Existing
        || state.reported_fault == Some(FaultType::Permanent)
        || preview.retired_storage_incarnations.contains(storage)
        || !preview.history_barriers.is_empty()
        || preview.session_renewal.is_some()
        || preview.data_loss_recovery.is_some()
        || !preview.active_builds.is_empty()
    {
        return Err(reject(
            "retained scope has current history/storage or unresolved recovery obligations",
        ));
    }
    Ok(())
}

fn notice_in(record: &PublicOperationRecord) -> Option<&RetainedRecoveryExitNotice> {
    record
        .lifecycle
        .outcomes
        .iter()
        .find_map(|outcome| match outcome {
            PublicInstructionOutcome::RecoveryExitRequired(notice) => Some(notice.as_ref()),
            _ => None,
        })
}

pub(crate) fn fault_dependency_replaced(
    preview: &PublicV1OperationState,
    fault: &PublicOperationRecord,
    process: &ProcessSessionId,
) -> bool {
    let mut current = fault;
    for _ in 0..preview.operations.len() {
        if current.intent.class != PublicOperationClass::TransientFault
            || &current.intent.process_session_id == process
        {
            return false;
        }
        let Some(notice) = notice_in(current) else {
            return false;
        };
        let Some(successor) = current
            .superseded_by
            .as_ref()
            .and_then(|id| preview.operations.get(id))
        else {
            return false;
        };
        let Some(goal) = successor.intent.goal() else {
            return false;
        };
        let Some(receipt) = successor
            .ordinary
            .as_ref()
            .and_then(|ordinary| ordinary.retained.as_ref())
        else {
            return false;
        };
        if !matches!(goal.desired, ReplicaGoalKind::RestoreRetainedMember { .. })
            || receipt.qualification != notice.qualification
            || receipt.successor_process != goal.scope.process_session_id
            || receipt.successor_creation != goal.scope.creation_id
            || receipt.owner_process_lifetime.is_empty()
            || receipt.predecessor_process_lifetime != notice.process_lifetime
            || receipt.owner_process_lifetime == notice.process_lifetime
            || goal.scope.history != notice.history
            || goal.scope.storage != notice.storage
        {
            return false;
        }
        if &goal.scope.process_session_id == process {
            return true;
        }
        if !complete(successor) {
            return false;
        }
        let Some(next) = preview.operations.values().find(|record| {
            record.intent.class == PublicOperationClass::TransientFault
                && record.intent.process_session_id == goal.scope.process_session_id
                && notice_in(record).is_some_and(|notice| {
                    notice.qualification.predecessor_creation_id == goal.scope.creation_id
                        && record.superseded_by.is_some()
                })
        }) else {
            return false;
        };
        current = next;
    }
    false
}

pub(crate) fn validate_notice(
    state: &AgentState,
    notice: &RetainedRecoveryExitNotice,
) -> Result<()> {
    if notice.native_rebuild.is_some() {
        return super::super::original_member::validate_restart_notice(state, notice, true);
    }
    validate_persisted_notice(state, notice)?;
    if state.reported_fault != Some(FaultType::Transient)
        || state
            .public_v1_operation_state
            .as_ref()
            .is_none_or(|preview| !preview.terminal || !preview.writes_revoked)
    {
        return Err(reject(
            "current fault policy is not retained transient recovery",
        ));
    }
    Ok(())
}

pub(crate) fn validate_persisted_notice(
    state: &AgentState,
    notice: &RetainedRecoveryExitNotice,
) -> Result<()> {
    if notice.native_rebuild.is_some() {
        return super::super::original_member::validate_restart_notice(state, notice, false);
    }
    let (_, process_id, lifetime) = historical(
        state,
        &notice.history,
        &notice.storage,
        &notice.qualification,
    )?;
    let record = state
        .public_v1_operation_state
        .as_ref()
        .unwrap()
        .operations
        .get(&notice.fault_operation_id)
        .ok_or_else(|| reject("missing exact recovery fault"))?;
    if process_id != notice.process_id
        || lifetime != notice.process_lifetime
        || record.intent.class != PublicOperationClass::TransientFault
        || record.intent.revision != notice.fault_revision
        || record.intent.revision <= notice.qualification.admitted_revision
        || record.intent.process_session_id != notice.qualification.predecessor_process_session_id
        || record.accepted_history.as_ref() != Some(&notice.history)
        || record.accepted_storage.as_ref() != Some(&notice.storage)
        || record.intent.program.is_some()
        || record.intent.lifecycle.is_some()
        || notice_in(record) != Some(notice)
        || record.containment != PublicOperationContainment::Pending
    {
        return Err(reject(
            "recovery exit cause differs from exact durable transient fault",
        ));
    }
    Ok(())
}

fn dependencies(
    state: &AgentState,
    qualification: &RetainedMemberQualification,
    fault_being_selected: Option<&OperationId>,
) -> Result<()> {
    let preview = state.public_v1_operation_state.as_ref().unwrap();
    for record in preview.operations.values().filter(|record| {
        record.intent.process_session_id == qualification.predecessor_process_session_id
    }) {
        if record.intent.class == PublicOperationClass::TransientFault {
            if fault_being_selected == Some(&record.intent.operation_id) {
                continue;
            }
            let notice = notice_in(record)
                .ok_or_else(|| reject("opaque predecessor fault lacks scoped recovery cause"))?;
            validate_notice(state, notice)?;
            if &notice.qualification != qualification {
                return Err(reject("predecessor fault qualifies another admitted scope"));
            }
        } else if record.intent.class.is_terminal()
            || !complete(record)
            || record.intent.revision > qualification.admitted_revision
        {
            return Err(reject(
                "predecessor has explicit terminal or unresolved role/epoch/Build/configuration work",
            ));
        }
    }
    Ok(())
}

pub(crate) fn validate_restore(state: &AgentState, goal: &ReplicaGoal) -> Result<(u32, String)> {
    let ReplicaGoalKind::RestoreRetainedMember { qualification } = &goal.desired else {
        return Err(reject("not a retained-member restoration"));
    };
    eligible(state, &goal.scope.history, &goal.scope.storage)?;
    let (_, process_id, lifetime) = historical(
        state,
        &goal.scope.history,
        &goal.scope.storage,
        qualification,
    )?;
    dependencies(state, qualification, None)?;
    if goal.scope.process_session_id == qualification.predecessor_process_session_id
        || goal.scope.creation_id == qualification.predecessor_creation_id
        || goal.scope.epoch != qualification.configuration.epoch
        || goal.scope.configuration.current != qualification.configuration
        || goal.scope.configuration.previous.is_some()
    {
        return Err(reject(
            "retained restoration crossed its exact historical member",
        ));
    }
    let preview = state.public_v1_operation_state.as_ref().unwrap();
    if preview.operations.values().any(|record| {
        record.intent.operation_id != goal.operation_id
            && record.intent.process_session_id != qualification.predecessor_process_session_id
            && record.intent.process_session_id != goal.scope.process_session_id
            && !complete(record)
            && !fault_dependency_replaced(
                preview,
                record,
                &qualification.predecessor_process_session_id,
            )
    }) {
        return Err(reject(
            "another predecessor retains an unresolved scoped dependency",
        ));
    }
    if !preview.operations.contains_key(&goal.operation_id)
        && preview.operations.values().any(|record| {
            record.intent.operation_id != goal.operation_id
                && record.intent.process_session_id == goal.scope.process_session_id
        })
    {
        return Err(reject(
            "restoration requires a fresh unopened process and actual creation",
        ));
    }
    Ok((process_id, lifetime))
}

pub(crate) fn validate_receipt(state: &AgentState, record: &PublicOperationRecord) -> Result<()> {
    validate_persisted_receipt(state, record)?;
    let goal = record.intent.goal().unwrap();
    validate_restore(state, goal)?;
    Ok(())
}

pub(crate) fn validate_persisted_receipt(
    state: &AgentState,
    record: &PublicOperationRecord,
) -> Result<()> {
    let goal = record
        .intent
        .goal()
        .ok_or_else(|| reject("restoration receipt has no goal"))?;
    let ReplicaGoalKind::RestoreRetainedMember { qualification } = &goal.desired else {
        return Err(reject("restoration receipt attached to another goal"));
    };
    let (_, old_pid, lifetime) = historical(
        state,
        &goal.scope.history,
        &goal.scope.storage,
        qualification,
    )?;
    let receipt = record
        .ordinary
        .as_ref()
        .and_then(|ordinary| ordinary.retained.as_ref())
        .ok_or_else(|| reject("restoration lacks process-bound exclusive storage receipt"))?;
    if receipt.qualification != **qualification
        || receipt.successor_process != goal.scope.process_session_id
        || receipt.successor_creation != goal.scope.creation_id
        || receipt.predecessor_process_id != old_pid
        || receipt.owner_process_id == 0
        || receipt.predecessor_process_lifetime != lifetime
        || receipt.owner_process_lifetime.is_empty()
        || receipt.owner_process_lifetime == lifetime
    {
        return Err(reject(
            "restoration physical custody receipt crossed its exact scope",
        ));
    }
    let actions = &record.ordinary.as_ref().unwrap().actions;
    for (instruction, outcome) in actions.iter().zip(&record.lifecycle.outcomes) {
        if *instruction == PublicInstruction::ReplicatorRole
            && !matches!(outcome, PublicInstructionOutcome::RetainedNativeInitialized(scope) if scope.as_ref() == &goal.scope)
        {
            return Err(reject(
                "persisted restored native role has no actual initialization receipt",
            ));
        }
        if let PublicInstructionOutcome::BuiltinNativeEndpoint {
            process_id,
            process_lifetime,
            ..
        } = outcome
            && (*process_id != receipt.owner_process_id
                || *process_lifetime != receipt.owner_process_lifetime)
        {
            return Err(reject(
                "builtin registration belongs to another physical process",
            ));
        }
    }
    Ok(())
}

pub(crate) fn supersedes_fault(
    state: &AgentState,
    goal: &ReplicaGoal,
    predecessor: &PublicOperationRecord,
) -> bool {
    let ReplicaGoalKind::RestoreRetainedMember { qualification } = &goal.desired else {
        return false;
    };
    predecessor.intent.class == PublicOperationClass::TransientFault
        && predecessor.intent.process_session_id == qualification.predecessor_process_session_id
        && notice_in(predecessor).is_some_and(|notice| {
            &notice.qualification == qualification.as_ref()
                && validate_notice(state, notice).is_ok()
        })
        && validate_restore(state, goal).is_ok()
}

pub(crate) fn recovery_cause(
    state: &AgentState,
    intent: &PublicOperationIntent,
    process_id: u32,
) -> Option<RetainedRecoveryExitNotice> {
    if intent.class != PublicOperationClass::TransientFault
        || intent.program.is_some()
        || intent.lifecycle.is_some()
        || process_id == 0
    {
        return None;
    }
    let preview = state.public_v1_operation_state.as_ref()?;
    let record = preview
        .operations
        .values()
        .filter(|record| record.intent.process_session_id == intent.process_session_id)
        .filter(|record| record.intent.goal().is_some() && complete(record))
        .max_by_key(|record| record.intent.revision)?;
    let goal = record.intent.goal()?;
    eligible(state, &goal.scope.history, &goal.scope.storage).ok()?;
    let qualification = RetainedMemberQualification {
        predecessor_process_session_id: goal.scope.process_session_id.clone(),
        predecessor_creation_id: goal.scope.creation_id.clone(),
        admitted_operation_id: goal.operation_id.clone(),
        admitted_revision: goal.revision,
        configuration: goal.scope.configuration.current.clone(),
    };
    let (_, historical_pid, lifetime) = historical(
        state,
        &goal.scope.history,
        &goal.scope.storage,
        &qualification,
    )
    .ok()?;
    if historical_pid != process_id
        || intent.revision <= goal.revision
        || lifetime != process_lifetime_id()
        || dependencies(state, &qualification, Some(&intent.operation_id)).is_err()
    {
        return None;
    }
    Some(RetainedRecoveryExitNotice {
        native_rebuild: None,
        fault_operation_id: intent.operation_id.clone(),
        fault_revision: intent.revision,
        process_id,
        process_lifetime: lifetime,
        history: goal.scope.history.clone(),
        storage: goal.scope.storage.clone(),
        qualification,
    })
}

pub(crate) fn status(
    state: &AgentState,
    process: &ProcessSessionId,
) -> Option<RetainedRecoveryExitNotice> {
    state
        .public_v1_operation_state
        .as_ref()?
        .operations
        .values()
        .filter(|record| &record.intent.process_session_id == process)
        .filter_map(notice_in)
        .filter(|notice| {
            validate_notice(state, notice).is_ok()
                && (notice.native_rebuild.is_some()
                    || dependencies(state, &notice.qualification, None).is_ok())
        })
        .max_by_key(|notice| notice.fault_revision)
        .cloned()
}

pub(crate) fn qualification_for_process(
    state: &AgentState,
    process: &ProcessSessionId,
) -> Option<RetainedMemberQualification> {
    let preview = state.public_v1_operation_state.as_ref()?;
    if let Some(current) = preview
        .operations
        .values()
        .filter(|record| &record.intent.process_session_id == process)
        .max_by_key(|record| record.intent.revision)
    {
        if current.superseded_by.is_some()
            || matches!(
                current.disposition,
                Some(
                    PublicOperationDisposition::Failed(_)
                        | PublicOperationDisposition::Ambiguous(_)
                        | PublicOperationDisposition::Cancelled
                )
            )
            || current.stage == PublicOperationStage::ContainmentPending
        {
            return None;
        }
        let goal = current.intent.goal()?;
        let ReplicaGoalKind::RestoreRetainedMember { qualification } = &goal.desired else {
            return None;
        };
        validate_receipt(state, current).ok()?;
        return Some(qualification.as_ref().clone());
    }
    let record = preview
        .operations
        .values()
        .filter(|record| {
            complete(record)
                && record
                    .ordinary
                    .as_ref()
                    .is_some_and(|ordinary| ordinary.accepted)
        })
        .max_by_key(|record| record.intent.revision)?;
    let goal = record.intent.goal()?;
    let qualification = RetainedMemberQualification {
        predecessor_process_session_id: goal.scope.process_session_id.clone(),
        predecessor_creation_id: goal.scope.creation_id.clone(),
        admitted_operation_id: goal.operation_id.clone(),
        admitted_revision: goal.revision,
        configuration: goal.scope.configuration.current.clone(),
    };
    eligible(state, &goal.scope.history, &goal.scope.storage).ok()?;
    let (_, _, predecessor_lifetime) = historical(
        state,
        &goal.scope.history,
        &goal.scope.storage,
        &qualification,
    )
    .ok()?;
    // Historical admission alone does not select physical reconstruction. A
    // clean host reopen within this OS process must use normal retained
    // transfer/renewal; failed owners still require the store's actual locks
    // and distinct-process checks before any reconstruction effect.
    if predecessor_lifetime == process_lifetime_id() {
        return None;
    }
    dependencies(state, &qualification, None).ok()?;
    if preview.operations.values().any(|record| {
        record.intent.process_session_id != qualification.predecessor_process_session_id
            && !complete(record)
            && !fault_dependency_replaced(
                preview,
                record,
                &qualification.predecessor_process_session_id,
            )
    }) {
        return None;
    }
    Some(qualification)
}

pub(crate) fn restored_process(state: &AgentState, process: &ProcessSessionId) -> bool {
    state
        .public_v1_operation_state
        .as_ref()
        .is_some_and(|preview| {
            preview.operations.values().any(|record| {
                &record.intent.process_session_id == process
                    && record.intent.goal().is_some_and(|goal| {
                        matches!(goal.desired, ReplicaGoalKind::RestoreRetainedMember { .. })
                    })
                    && validate_persisted_receipt(state, record).is_ok()
                    && record.superseded_by.is_none()
                    && !matches!(
                        record.disposition,
                        Some(
                            PublicOperationDisposition::Failed(_)
                                | PublicOperationDisposition::Ambiguous(_)
                                | PublicOperationDisposition::Cancelled
                        )
                    )
            })
        })
}

#[cfg(test)]
#[path = "retained_tests.rs"]
mod tests;
