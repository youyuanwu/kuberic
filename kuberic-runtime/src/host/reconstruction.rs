use super::*;

pub(crate) struct Predecessor {
    pub(crate) process_id: u32,
    pub(crate) process_lifetime: String,
}

pub(crate) struct InspectionCustody {
    pub(crate) dependencies: std::collections::BTreeSet<OperationId>,
    pub(crate) predecessors: Vec<(ProcessSessionId, ReplicatorCreationId, Predecessor)>,
}

pub(crate) fn inspection_open_selected(state: &AgentState, process: &ProcessSessionId) -> bool {
    state
        .public_v1_operation_state
        .as_ref()
        .is_some_and(|preview| {
            preview.operations.values().any(|record| {
                let Some(goal) = record.intent.goal() else {
                    return false;
                };
                record.intent.process_session_id == *process
                    && record.stage == PublicOperationStage::Running
                    && record.lifecycle.in_flight == Some(PublicInstruction::ApplicationOpen)
                    && record.superseded_by.is_none()
                    && inspection_custody(state, goal).is_some()
                    && super::validate_creation_owner(state, record).is_ok_and(|owner| {
                        owner.process_id == std::process::id()
                            && owner.process_lifetime == retained::process_lifetime_id()
                    })
            })
        })
}

/// Permission to inspect excluded retained storage, never to resume an old
/// operation, clear its failure, or admit a replication/serving root.
pub(crate) fn inspection_custody(
    state: &AgentState,
    goal: &ReplicaGoal,
) -> Option<InspectionCustody> {
    if !matches!(goal.desired, ReplicaGoalKind::Discover)
        || state.application_open_mode() != OpenMode::Existing
        || state.reported_fault == Some(FaultType::Permanent)
        || state.application_storage.as_ref()?.paths.is_empty()
    {
        return None;
    }
    let preview = state.public_v1_operation_state.as_ref()?;
    if preview.history.as_ref() != Some(&goal.scope.history)
        || preview.storage_incarnation.as_ref() != Some(&goal.scope.storage)
        || preview.binding.as_ref()?.state_persistence != StatePersistence::Persisted
        || preview
            .retired_storage_incarnations
            .contains(&goal.scope.storage)
        || !preview.history_barriers.is_empty()
        || preview.session_renewal.is_some()
        || preview.data_loss_recovery.is_some()
    {
        return None;
    }
    let original = goal
        .scope
        .configuration
        .previous
        .as_ref()
        .unwrap_or(&goal.scope.configuration.current);
    let historical = preview.operations.values().any(|record| {
        succeeded(record)
            && record.lifecycle.outcomes.iter().any(|outcome| {
                matches!(outcome, PublicInstructionOutcome::NativeLifecycle(facts)
                if facts.scope.history == goal.scope.history
                    && facts.scope.storage == goal.scope.storage
                    && facts.scope.process_session_id != goal.scope.process_session_id
                    && facts.scope.creation_id != goal.scope.creation_id
                    && facts.scope.configuration.current == *original
                    && facts.deactivation.is_some()
                    && original.members.iter().any(|member|
                        member.identity == goal.scope.storage.replica))
            })
    });
    if !historical {
        return None;
    }
    let mut predecessors = std::collections::BTreeMap::new();
    let mut dependencies = std::collections::BTreeSet::new();
    for record in preview.operations.values() {
        if record.intent.process_session_id == goal.scope.process_session_id {
            if record.intent.goal().is_none_or(|old| {
                old.scope.creation_id != goal.scope.creation_id
                    || old.scope.history != goal.scope.history
                    || old.scope.storage != goal.scope.storage
                    || !(matches!(
                        old.desired,
                        ReplicaGoalKind::Discover
                            | ReplicaGoalKind::Prepare {
                                rebuild: Some(_),
                                ..
                            }
                            | ReplicaGoalKind::Reconfigure { .. }
                    ) || matches!(&old.desired, ReplicaGoalKind::Build { remote }
                        if remote.original_member.is_some() && remote.source.scope == old.scope))
            }) {
                return None;
            }
            continue;
        }
        if record.accepted_history.as_ref() != Some(&goal.scope.history)
            || record.accepted_storage.as_ref() != Some(&goal.scope.storage)
            || record.intent.class == PublicOperationClass::PermanentFault
        {
            return None;
        }
        dependencies.insert(record.intent.operation_id.clone());
        if let Some(old) = record.intent.goal() {
            let owner = super::validate_creation_owner(state, record).ok()?;
            if old.scope.creation_id == goal.scope.creation_id
                || old.scope.history != goal.scope.history
                || old.scope.storage != goal.scope.storage
            {
                return None;
            }
            predecessors.insert(
                (
                    old.scope.process_session_id.clone(),
                    old.scope.creation_id.clone(),
                ),
                Predecessor {
                    process_id: owner.process_id,
                    process_lifetime: owner.process_lifetime.clone(),
                },
            );
        }
    }
    if predecessors.is_empty()
        || preview.operations.values().any(|record| {
            dependencies.contains(&record.intent.operation_id)
                && !predecessors
                    .keys()
                    .any(|(process, _)| *process == record.intent.process_session_id)
        })
    {
        return None;
    }
    Some(InspectionCustody {
        dependencies,
        predecessors: predecessors
            .into_iter()
            .map(|((process, creation), owner)| (process, creation, owner))
            .collect(),
    })
}

fn succeeded(record: &PublicOperationRecord) -> bool {
    record.stage == PublicOperationStage::Completed
        && record.disposition == Some(PublicOperationDisposition::Succeeded)
        && record.lifecycle.in_flight.is_none()
        && record
            .ordinary
            .as_ref()
            .is_some_and(|ordinary| record.lifecycle.outcomes.len() == ordinary.actions.len())
}

pub(crate) fn claim(goal: &ReplicaGoal) -> Option<&ContainedCreationRecovery> {
    match &goal.desired {
        ReplicaGoalKind::RecoverContainedCreation { recovery }
            if recovery
                .addition
                .as_ref()
                .is_none_or(|addition| addition.phase == MembershipAdditionPhase::Bootstrap) =>
        {
            Some(recovery)
        }
        _ => None,
    }
}

fn open_only_admission<'a>(
    state: &'a AgentState,
    process: &ProcessSessionId,
    creation: &ReplicatorCreationId,
) -> Result<&'a ReplicaGoal> {
    let preview = state
        .public_v1_operation_state
        .as_ref()
        .ok_or_else(|| reject("Open reconstruction lacks retained history"))?;
    let records = preview
        .operations
        .values()
        .filter(|record| &record.intent.process_session_id == process)
        .collect::<Vec<_>>();
    let first = records
        .iter()
        .filter_map(|record| record.intent.goal())
        .min_by_key(|goal| goal.revision)
        .ok_or_else(|| reject("Open reconstruction lacks an accepted Discover"))?;
    let mut factory = None;
    for record in &records {
        let Some(old) = record.intent.goal() else {
            if record.intent.class != PublicOperationClass::TransientFault {
                return Err(reject(
                    "Open reconstruction has a nonlocal-fault dependency",
                ));
            }
            continue;
        };
        let owner = super::validate_creation_owner(state, record)?;
        if !matches!(old.desired, ReplicaGoalKind::Discover)
            || &old.scope.creation_id != creation
            || old.scope.history != first.scope.history
            || old.scope.storage != first.scope.storage
            || old.scope.epoch != first.scope.epoch
            || old.scope.configuration.current != first.scope.configuration.current
            || old.scope.configuration.previous.is_some()
            || !old.scope.configuration.replicas.is_empty()
            || record.ordinary.as_ref().is_none_or(|ordinary| {
                ordinary.actions
                    != [
                        PublicInstruction::ApplicationOpen,
                        PublicInstruction::ReplicatorOpen,
                    ]
            })
        {
            return Err(reject(
                "Open-only reconstruction cannot hide a selected native transition",
            ));
        }
        let selected = owner.builtin_factory.as_ref().ok_or_else(|| {
            reject("Open predecessor was not bound to retained builtin construction")
        })?;
        if factory.as_ref().is_some_and(|factory| factory != selected) {
            return Err(reject("Open predecessor changed its bound factory history"));
        }
        factory = Some(selected.clone());
    }
    let historical = preview
        .operations
        .values()
        .filter(|record| {
            record.intent.revision < first.revision
                && record.accepted_history.as_ref() == Some(&first.scope.history)
                && record.accepted_storage.as_ref() == Some(&first.scope.storage)
        })
        .filter_map(|record| {
            record
                .intent
                .goal()
                .filter(|goal| goal.role().is_some())
                .map(|goal| (record, goal))
        })
        .max_by_key(|(_, goal)| goal.revision)
        .ok_or_else(|| reject("Open reconstruction lacks a prior selected role"))?;
    let (record, admitted) = historical;
    if admitted.role() != Some(ReplicaRole::ActiveSecondary)
        || admitted.scope.configuration.previous.is_some()
        || admitted.scope.configuration.current != first.scope.configuration.current
        || admitted.scope.epoch != first.scope.epoch
        || record
            .ordinary
            .as_ref()
            .is_none_or(|ordinary| !ordinary.host_settled)
    {
        return Err(reject(
            "Open reconstruction lacks exact joined current-only secondary admission",
        ));
    }
    let qualification = RetainedMemberQualification {
        predecessor_process_session_id: admitted.scope.process_session_id.clone(),
        predecessor_creation_id: admitted.scope.creation_id.clone(),
        admitted_operation_id: admitted.operation_id.clone(),
        admitted_revision: admitted.revision,
        configuration: admitted.scope.configuration.current.clone(),
    };
    retained::historical(
        state,
        &first.scope.history,
        &first.scope.storage,
        &qualification,
    )?;
    let factory = factory
        .and_then(|id| preview.operations.get(&id))
        .and_then(|record| record.intent.goal())
        .ok_or_else(|| reject("Open reconstruction lost its actual factory source"))?;
    if factory.scope.process_session_id != admitted.scope.process_session_id
        || factory.scope.creation_id != admitted.scope.creation_id
    {
        return Err(reject(
            "Open factory provenance is not the admitted member's native creation",
        ));
    }
    Ok(admitted)
}

pub(crate) fn journal_dependencies(state: &AgentState) -> Vec<CreationRecoveryDependencies> {
    let Some(preview) = state.public_v1_operation_state.as_ref() else {
        return Vec::new();
    };
    let creations = preview
        .operations
        .values()
        .filter_map(|record| record.intent.goal())
        .map(|goal| {
            (
                goal.scope.process_session_id.clone(),
                goal.scope.creation_id.clone(),
            )
        })
        .collect::<std::collections::BTreeSet<_>>();
    creations
        .into_iter()
        .filter(|(process, creation)| {
            // Historical builtin candidate inventory only, not a physical or
            // readiness certificate. Independent engines retain their own
            // recovery obligations rather than receiving an impossible goal.
            preview.operations.values().any(|record| {
                record.intent.goal().is_some_and(|goal| {
                    &goal.scope.process_session_id == process && &goal.scope.creation_id == creation
                }) && record.lifecycle.outcomes.iter().any(|outcome| {
                    matches!(
                        outcome,
                        PublicInstructionOutcome::BuiltinNativeEndpoint { .. }
                    )
                }) || open_only_admission(state, process, creation).is_ok()
            })
        })
        .map(|(process_session_id, creation_id)| {
            let operations = preview
                .operations
                .values()
                .filter(|record| record.intent.process_session_id == process_session_id)
                .map(|record| record.intent.operation_id.clone())
                .collect();
            CreationRecoveryDependencies {
                process_session_id,
                creation_id,
                operations,
            }
        })
        .collect()
}

fn descriptors_match(left: &ReplicaConfigurationScope, right: &ReplicaConfigurationScope) -> bool {
    left.current == right.current
        && left.previous == right.previous
        && left.replicas.len() == right.replicas.len()
        && left.replicas.iter().all(|old| {
            right.replicas.iter().any(|new| {
                old.identity == new.identity
                    && old.storage_incarnation == new.storage_incarnation
                    && old.process_session_id == new.process_session_id
                    && old.replication_address == new.replication_address
                    && old.role == new.role
            })
        })
}

pub(crate) fn validate(state: &AgentState, goal: &ReplicaGoal) -> Result<Predecessor> {
    validate_context(state, goal, true)
}

fn validate_context(state: &AgentState, goal: &ReplicaGoal, active: bool) -> Result<Predecessor> {
    goal.validate().map_err(reject)?;
    let recovery = claim(goal).ok_or_else(|| reject("not a contained reconstruction"))?;
    let preview = state
        .public_v1_operation_state
        .as_ref()
        .ok_or_else(|| reject("contained reconstruction lacks an ordinary journal"))?;
    if active
        && (preview.history.as_ref() != Some(&goal.scope.history)
            || preview.storage_incarnation.as_ref() != Some(&goal.scope.storage)
            || state.identity.local_identity != goal.scope.storage.replica
            || state.application_open_mode() != OpenMode::Existing
            || state
                .application_storage
                .as_ref()
                .is_none_or(|binding| binding.paths.is_empty())
            || state.reported_fault == Some(FaultType::Permanent)
            || preview
                .binding
                .as_ref()
                .is_none_or(|binding| binding.state_persistence != StatePersistence::Persisted)
            || preview
                .retired_storage_incarnations
                .contains(&goal.scope.storage)
            || !preview.history_barriers.is_empty()
            || preview.session_renewal.is_some()
            || preview.data_loss_recovery.is_some())
    {
        return Err(reject(
            "contained reconstruction crossed retained storage/history or recovery policy",
        ));
    }
    let mut predecessors = preview
        .operations
        .values()
        .filter(|record| {
            record.intent.process_session_id == recovery.predecessor_process_session_id
                && record.intent.revision < goal.revision
        })
        .collect::<Vec<_>>();
    predecessors.sort_by_key(|record| record.intent.revision);
    let exact = predecessors
        .iter()
        .map(|record| &record.intent.operation_id)
        .collect::<std::collections::BTreeSet<_>>();
    if exact != recovery.predecessor_operations.iter().collect()
        || predecessors.is_empty()
        || predecessors.iter().any(|record| {
            record.intent.revision >= goal.revision
                || record.intent.identity != goal.identity
                || record.accepted_history.as_ref() != Some(&goal.scope.history)
                || record.accepted_storage.as_ref() != Some(&goal.scope.storage)
                || record.intent.class == PublicOperationClass::PermanentFault
                || record.intent.goal().is_some_and(|old| {
                    old.scope.creation_id != recovery.predecessor_creation_id
                        || old.scope.history != goal.scope.history
                        || old.scope.storage != goal.scope.storage
                })
                || record.superseded_by.as_ref().is_some_and(|id| {
                    id != &goal.operation_id
                        && !recovery.addition.as_ref().is_some_and(|addition| {
                            preview.operations.get(id).is_some_and(|successor| {
                                successor
                                    .intent
                                    .goal()
                                    .and_then(super::addition::selection)
                                    .is_some_and(|final_selection| {
                                        let mut expected = addition.as_ref().clone();
                                        expected.phase = MembershipAdditionPhase::Current;
                                        *final_selection == expected
                                    })
                            })
                        })
                })
        })
    {
        return Err(reject(
            "contained reconstruction must name the complete exact predecessor journal",
        ));
    }
    let selected = predecessors
        .iter()
        .rev()
        .filter_map(|record| record.intent.goal())
        .find(|old| old.role().is_some())
        .map(Ok)
        .unwrap_or_else(|| {
            open_only_admission(
                state,
                &recovery.predecessor_process_session_id,
                &recovery.predecessor_creation_id,
            )
        })?;
    if active
        && preview.operations.values().any(|record| {
            record.intent.operation_id != goal.operation_id
                && record.intent.process_session_id != recovery.predecessor_process_session_id
                && (!succeeded(record)
                    || record.intent.process_session_id == goal.scope.process_session_id)
                && !dependency_replaced(preview, record, &recovery.predecessor_process_session_id)
                && !retained::fault_dependency_replaced(
                    preview,
                    record,
                    &recovery.predecessor_process_session_id,
                )
                && !(selected.scope.process_session_id != recovery.predecessor_process_session_id
                    && record.intent.revision < selected.revision
                    && record.intent.process_session_id != goal.scope.process_session_id
                    && (dependency_replaced(preview, record, &selected.scope.process_session_id)
                        || retained::fault_dependency_replaced(
                            preview,
                            record,
                            &selected.scope.process_session_id,
                        )))
        })
    {
        return Err(reject(
            "unrelated or current owners still block contained reconstruction",
        ));
    }
    let highest = predecessors
        .iter()
        .filter_map(|record| record.intent.goal())
        .map(|old| old.scope.epoch)
        .max()
        .ok_or_else(|| reject("predecessor lacks a selected epoch"))?;
    if recovery.addition.is_some() {
        super::addition::bootstrap(state, goal)?;
    } else if selected.role() != Some(recovery.role)
        || highest != goal.scope.epoch
        || !descriptors_match(&selected.scope.configuration, &goal.scope.configuration)
    {
        return Err(reject(
            "reconstruction must retain the selected role, highest epoch and exact PC/CC",
        ));
    }
    let mut needs_catch_up = if recovery.role == ReplicaRole::Primary {
        match &selected.desired {
            ReplicaGoalKind::Reconfigure { catch_up, .. }
            | ReplicaGoalKind::RefreshPrimary { catch_up } => *catch_up,
            ReplicaGoalKind::RecoverContainedCreation { recovery } => recovery.catch_up,
            _ => None,
        }
    } else {
        None
    };
    for record in &predecessors {
        let Some(old) = record.intent.goal() else {
            if !matches!(record.intent.class, PublicOperationClass::TransientFault) {
                return Err(reject(
                    "opaque non-fault predecessor has no frozen recovery recipe",
                ));
            }
            continue;
        };
        let ordinary = record
            .ordinary
            .as_ref()
            .ok_or_else(|| reject("predecessor lacks frozen instructions"))?;
        if ordinary.action_format != ORDINARY_ACTION_FORMAT {
            return Err(reject("predecessor has an incompatible frozen recipe"));
        }
        for (index, instruction) in ordinary.actions.iter().enumerate() {
            let pending = record.lifecycle.outcomes.get(index).is_none();
            if pending
                && let Some(configuration) = super::super::public_action_state::configuration_at(
                    &record.intent,
                    *instruction,
                )
            {
                if let Some(addition) = &recovery.addition
                    && (configuration != addition.admitted.scope.configuration.ordinary()
                        && configuration != addition.interrupted.scope.configuration.ordinary()
                        || super::super::public_lifecycle::program::ordinary_handoff(old).is_some())
                {
                    return Err(reject(
                        "addition cannot flatten unrelated frozen root obligations",
                    ));
                }
                if recovery.addition.is_none()
                    && (configuration.current != goal.scope.configuration.current
                        || configuration.previous != goal.scope.configuration.previous)
                {
                    return Err(reject(
                        "distinct unresolved frozen PC/CC obligations require ordered native history recovery",
                    ));
                }
                if matches!(
                    instruction,
                    PublicInstruction::FirstCatchUp | PublicInstruction::SecondCatchUp
                ) {
                    let program = super::super::public_lifecycle::program::selected_program(
                        &record.intent,
                        *instruction,
                    )
                    .ok_or_else(|| reject("catch-up lacks its frozen program"))?;
                    let mode = match program {
                        PublicOperationProgram::CatchUp { mode, .. }
                        | PublicOperationProgram::Swap { mode, .. } => mode,
                        _ => return Err(reject("invalid frozen catch-up program")),
                    };
                    needs_catch_up = Some(match (needs_catch_up, mode) {
                        (Some(PublicCatchUpMode::All), _) | (_, PublicCatchUpMode::All) => {
                            PublicCatchUpMode::All
                        }
                        _ => PublicCatchUpMode::WriteQuorum,
                    });
                }
            }
        }
        if let ReplicaGoalKind::Build { remote } = &old.desired {
            let receiver = recovery
                .receiver_settlements
                .iter()
                .find(|receiver| receiver.source_operation_id == old.operation_id)
                .ok_or_else(|| {
                    reject("old Build requires an independently settled exact receiver")
                })?;
            let target = &receiver.terminal_goal.scope;
            if target.history != remote.target.scope.history
                || target.storage != remote.target.scope.storage
                || target.process_session_id != remote.target.scope.process_session_id
                || target.creation_id != remote.target.scope.creation_id
                || receiver.terminal_goal.revision <= remote.target.generation
            {
                return Err(reject("receiver settlement crossed the old Build target"));
            }
        } else if recovery
            .receiver_settlements
            .iter()
            .any(|receiver| receiver.source_operation_id == old.operation_id)
        {
            return Err(reject(
                "receiver claim does not name a source Build dependency",
            ));
        }
    }
    if (recovery.role != ReplicaRole::Primary && recovery.catch_up.is_some())
        || recovery.addition.is_none()
            && needs_catch_up.is_some_and(|needed| recovery.catch_up != Some(needed))
    {
        return Err(reject(
            "reconstruction omits the original frozen catch-up obligation",
        ));
    }
    let registration = predecessors
        .iter()
        .flat_map(|record| {
            record
                .ordinary
                .iter()
                .flat_map(|ordinary| ordinary.actions.iter())
                .zip(&record.lifecycle.outcomes)
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
                && storage_owner.as_ref() == Some(&recovery.predecessor_process_session_id) =>
            {
                Some(Predecessor {
                    process_id: *process_id,
                    process_lifetime: process_lifetime.clone(),
                })
            }
            _ => None,
        });
    let registration = match registration {
        Some(registration) => registration,
        None => {
            open_only_admission(
                state,
                &recovery.predecessor_process_session_id,
                &recovery.predecessor_creation_id,
            )?;
            let record = predecessors
                .iter()
                .find(|record| record.intent.goal().is_some())
                .ok_or_else(|| reject("Open reconstruction lacks its physical owner record"))?;
            let owner = super::validate_creation_owner(state, record)?;
            Predecessor {
                process_id: owner.process_id,
                process_lifetime: owner.process_lifetime.clone(),
            }
        }
    };
    Ok(registration)
}

pub(crate) fn validate_receipt(state: &AgentState, record: &PublicOperationRecord) -> Result<()> {
    let goal = record
        .intent
        .goal()
        .ok_or_else(|| reject("reconstruction receipt lacks a goal"))?;
    let predecessor = validate_context(state, goal, false)?;
    let receipt = record
        .ordinary
        .as_ref()
        .and_then(|ordinary| ordinary.reconstruction.as_ref())
        .ok_or_else(|| reject("reconstruction lacks checked physical custody"))?;
    if receipt.predecessor_process_id != predecessor.process_id
        || receipt.predecessor_process_lifetime != predecessor.process_lifetime
        || receipt.owner_process_id == 0
        || receipt.owner_process_lifetime.is_empty()
        || (receipt.exclusion == CreationExclusion::ExclusiveProcessReacquisition
            && receipt.owner_process_lifetime == predecessor.process_lifetime)
    {
        return Err(reject(
            "reconstruction custody differs from its actual predecessor",
        ));
    }
    for (instruction, outcome) in record
        .ordinary
        .as_ref()
        .unwrap()
        .actions
        .iter()
        .zip(&record.lifecycle.outcomes)
    {
        if *instruction == PublicInstruction::ReplicatorRole
            && !matches!(outcome, PublicInstructionOutcome::ReconstructedNativeInitialized(scope)
                if scope.as_ref() == &goal.scope)
        {
            return Err(reject(
                "reconstruction lacks actual cold native initialization",
            ));
        }
        if let PublicInstructionOutcome::BuiltinNativeEndpoint {
            process_id,
            process_lifetime,
            storage_owner,
            ..
        } = outcome
            && (*process_id != receipt.owner_process_id
                || *process_lifetime != receipt.owner_process_lifetime
                || storage_owner.as_ref() != Some(&goal.scope.process_session_id))
        {
            return Err(reject(
                "reconstruction registration crossed the actual storage owner",
            ));
        }
    }
    Ok(())
}

pub(crate) fn permits_attempt(
    state: &AgentState,
    goal: &ReplicaGoal,
    record: &PublicOperationRecord,
) -> bool {
    claim(goal).is_some_and(|recovery| {
        recovery.predecessor_process_session_id == record.intent.process_session_id
            && recovery
                .predecessor_operations
                .contains(&record.intent.operation_id)
            && validate(state, goal).is_ok()
    })
}

pub(crate) fn dependency_replaced(
    preview: &PublicV1OperationState,
    predecessor: &PublicOperationRecord,
    process: &ProcessSessionId,
) -> bool {
    let mut previous = predecessor;
    for _ in 0..preview.operations.len() {
        let Some(successor) = previous
            .superseded_by
            .as_ref()
            .and_then(|id| preview.operations.get(id))
        else {
            return false;
        };
        if !succeeded(successor)
            || successor
                .intent
                .goal()
                .and_then(super::addition::recovery)
                .is_none_or(|recovery| {
                    recovery
                        .addition
                        .as_ref()
                        .is_some_and(|addition| addition.phase != MembershipAdditionPhase::Current)
                        || recovery.predecessor_process_session_id
                            != previous.intent.process_session_id
                        || !recovery
                            .predecessor_operations
                            .contains(&previous.intent.operation_id)
                })
            || successor.ordinary.as_ref().is_none_or(|ordinary| {
                ordinary.reconstruction.is_none()
                    && !successor.intent.goal().is_some_and(|goal| {
                        super::addition::selection(goal).is_some_and(|addition| {
                            addition.phase == MembershipAdditionPhase::Current
                        }) && ordinary.accepted
                            && ordinary.host_settled
                    })
            })
        {
            return false;
        }
        if successor.intent.process_session_id == *process {
            return true;
        }
        previous = successor;
    }
    false
}

pub(crate) fn dependency_replaced_in_process(
    state: &AgentState,
    predecessor: &PublicOperationRecord,
    process: &ProcessSessionId,
) -> bool {
    let Some(preview) = state.public_v1_operation_state.as_ref() else {
        return false;
    };
    if dependency_replaced(preview, predecessor, process) {
        return true;
    }
    preview.operations.values().any(|successor| {
        if successor.intent.process_session_id != *process
            || !succeeded(successor)
            || successor.accepted_history != predecessor.accepted_history
            || successor.accepted_storage != predecessor.accepted_storage
            || successor.ordinary.as_ref().is_none_or(|ordinary| {
                !ordinary.accepted || !ordinary.host_settled || ordinary.reconstruction.is_none()
            })
        {
            return false;
        }
        let Some(recovery) = successor.intent.goal().and_then(claim) else {
            return false;
        };
        let Ok(admitted) = open_only_admission(
            state,
            &recovery.predecessor_process_session_id,
            &recovery.predecessor_creation_id,
        ) else {
            return false;
        };
        predecessor.intent.revision < admitted.revision
            && validate_receipt(state, successor).is_ok()
            && (dependency_replaced(preview, predecessor, &admitted.scope.process_session_id)
                || retained::fault_dependency_replaced(
                    preview,
                    predecessor,
                    &admitted.scope.process_session_id,
                ))
    })
}

pub(crate) fn attempting_process(
    preview: &PublicV1OperationState,
    process: &ProcessSessionId,
) -> bool {
    preview.operations.values().any(|record| {
        record.intent.process_session_id == *process
            && record.intent.goal().and_then(claim).is_some()
            && record
                .ordinary
                .as_ref()
                .is_some_and(|ordinary| ordinary.reconstruction.is_some())
            && record.superseded_by.is_none()
            && !matches!(
                record.disposition,
                Some(
                    PublicOperationDisposition::Failed(_)
                        | PublicOperationDisposition::Ambiguous(_)
                        | PublicOperationDisposition::Cancelled
                )
            )
            && !preview.operations.values().any(|later| {
                later.intent.process_session_id == *process
                    && later.intent.operation_id != record.intent.operation_id
                    && (later.intent.class.is_terminal()
                        || matches!(
                            later.disposition,
                            Some(
                                PublicOperationDisposition::Failed(_)
                                    | PublicOperationDisposition::Ambiguous(_)
                                    | PublicOperationDisposition::Cancelled
                            )
                        ))
            })
    })
}

pub(crate) fn receiver_settled(
    receiver: &ContainedReceiverSettlement,
    snapshot: &ReplicaAgentSnapshot,
) -> bool {
    let goal = &receiver.terminal_goal;
    matches!(
        goal.desired,
        ReplicaGoalKind::Close | ReplicaGoalKind::Abort
    ) && snapshot.format == ORDINARY_ACTION_FORMAT
        && snapshot.identity == goal.identity
        && snapshot.history == goal.scope.history
        && snapshot.storage == goal.scope.storage
        && ((snapshot.process_session_id == goal.scope.process_session_id
            && snapshot.creation_id == goal.scope.creation_id
            && snapshot.withdrawn)
            || (snapshot.process_session_id != goal.scope.process_session_id
                && snapshot.creation_id != goal.scope.creation_id
                && !snapshot.goals.iter().any(|entry| {
                    entry.goal.scope.process_session_id == snapshot.process_session_id
                })))
        && snapshot.publication.is_none()
        && !snapshot.provider_writer_held
        && snapshot.read_status != AccessStatus::Granted
        && snapshot.write_status != AccessStatus::Granted
        && snapshot.goals.iter().any(|observed| {
            &observed.goal == goal.as_ref()
                && observed.status == ReplicaGoalStatus::Achieved
                && observed.host_settled
                && observed.selected_call.is_none()
        })
}
