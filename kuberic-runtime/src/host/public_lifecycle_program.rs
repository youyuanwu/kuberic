use super::*;
use crate::RuntimeError;
use crate::host::state::{PublicCloseChild, PublicInstructionOutcome};
use crate::protocol::public_operations::{
    DataLossReplay, PublicCatchUpMode, PublicConfiguration, PublicOperationProgram,
    PublicOperationReplayDisposition, ReplicaRebuildRequired,
};
use crate::replicator::{ReplicaInformation, ReplicaSetConfiguration};

pub(crate) fn operation_instructions(intent: &PublicOperationIntent) -> Vec<PublicInstruction> {
    use PublicInstruction::*;
    use PublicOperationProgram as P;
    match &intent.program {
        Some(P::AgentGoal(goal)) if goal.class().is_terminal() => {
            if matches!(
                goal.desired,
                crate::protocol::public_operations::ReplicaGoalKind::Abort
            ) {
                vec![Revoke, Abort, Cleanup]
            } else {
                vec![Revoke, Cleanup]
            }
        }
        Some(P::AgentGoal(goal)) => goal_programs(goal)
            .iter()
            .flat_map(|program| {
                let mut selected = intent.clone();
                selected.program = Some(program.clone());
                operation_instructions(&selected)
            })
            .collect(),
        Some(P::Open { .. }) => vec![ApplicationOpen, ReplicatorOpen],
        Some(P::Role { .. }) => vec![ReplicatorRole, ApplicationRole],
        Some(P::Epoch { .. }) => vec![ProgramEpoch],
        Some(P::Configuration(_)) => vec![Configuration],
        Some(P::CatchUp { .. }) => vec![Configuration, FirstCatchUp],
        Some(P::Progress { .. }) => vec![Progress],
        Some(P::Swap {
            controller_composed: true,
            ..
        }) => vec![
            Revoke,
            ProgramEpoch,
            RefreshedConfiguration,
            SecondCatchUp,
            ReplicatorRole,
            ApplicationRole,
        ],
        Some(P::Swap { .. }) => vec![
            StartingConfiguration,
            FirstCatchUp,
            Revoke,
            ProgramEpoch,
            RefreshedConfiguration,
            SecondCatchUp,
            ReplicatorRole,
            ApplicationRole,
        ],
        Some(P::Build(_)) => vec![Build],
        Some(P::Remove(_) | P::RetireIdleReplica { .. }) => vec![Remove],
        Some(P::Close) => vec![Revoke, ReplicatorClose, ApplicationClose, Cleanup],
        Some(P::Abort) => vec![Revoke, Abort, Cleanup],
        None => intent
            .lifecycle
            .as_ref()
            .map(instructions)
            .unwrap_or_default(),
    }
}

pub(crate) fn goal_programs(
    goal: &crate::protocol::public_operations::ReplicaGoal,
) -> Vec<PublicOperationProgram> {
    use crate::protocol::public_operations::ReplicaGoalKind as G;
    use PublicOperationProgram as P;
    let mut programs = Vec::new();
    match &goal.desired {
        G::Discover => programs.push(P::Open {
            replica: goal.scope.storage.replica.clone(),
            existing: true,
        }),
        G::Prepare { role } => {
            programs.push(P::Open {
                replica: goal.scope.storage.replica.clone(),
                existing: true,
            });
            programs.push(P::Role {
                epoch: goal.scope.epoch,
                role: *role,
            });
            programs.push(P::Epoch {
                epoch: goal.scope.epoch,
            });
            if *role != ReplicaRole::IdleSecondary {
                programs.push(P::Configuration(goal.scope.configuration.ordinary()));
            }
        }
        G::Reconfigure { role, catch_up } => {
            programs.push(P::Role {
                epoch: goal.scope.epoch,
                role: *role,
            });
            programs.push(P::Epoch {
                epoch: goal.scope.epoch,
            });
            if *role != ReplicaRole::IdleSecondary {
                programs.push(match catch_up {
                    Some(mode) => P::CatchUp {
                        configuration: goal.scope.configuration.ordinary(),
                        mode: *mode,
                    },
                    None => P::Configuration(goal.scope.configuration.ordinary()),
                });
            }
        }
        G::Build { remote } | G::Remove { remote } => {
            let input = crate::protocol::public_operations::PublicBuildInput {
                attempt: goal.operation_id.clone(),
                epoch: goal.scope.epoch,
                current_configuration_id: Some(
                    goal.scope.configuration.current.configuration_id.clone(),
                ),
                replica: remote.target.scope.storage.replica.clone(),
                target_storage: remote.target.scope.storage.clone(),
                process_session_id: remote.target.scope.process_session_id.clone(),
                source_address: remote.source.replication_address.clone(),
                replication_address: remote.target.replication_address.clone(),
                history: Some(goal.scope.history.clone()),
            };
            programs.push(if matches!(goal.desired, G::Build { .. }) {
                P::Build(input)
            } else {
                P::Remove(input)
            });
        }
        G::Close => programs.push(P::Close),
        // Abort always retains both ordinary child Close calls.
        G::Abort => {
            programs.push(P::Abort);
        }
    }
    programs
}

pub(crate) fn selected_program(
    intent: &PublicOperationIntent,
    instruction: PublicInstruction,
) -> Option<PublicOperationProgram> {
    let program = intent.program.as_ref()?;
    let PublicOperationProgram::AgentGoal(goal) = program else {
        return Some(program.clone());
    };
    goal_programs(goal).into_iter().find(|program| {
        let mut local = intent.clone();
        local.program = Some(program.clone());
        operation_instructions(&local).contains(&instruction)
    })
}

pub(crate) fn repeatable(instruction: PublicInstruction) -> bool {
    !matches!(
        instruction,
        PublicInstruction::Build
            | PublicInstruction::DataLoss
            | PublicInstruction::ReplicatorClose
            | PublicInstruction::ApplicationClose
    )
}

pub(crate) fn replayable(record: &PublicOperationRecord) -> bool {
    if record.intent.goal().is_some() {
        // Ordinary selected calls belong to one process. A persisted selection
        // without its receipt can never recreate that future.
        return record.lifecycle.in_flight.is_none();
    }
    if record.intent.lifecycle.is_none() && record.intent.program.is_none() {
        return false;
    }
    record
        .lifecycle
        .in_flight
        .is_none_or(|instruction| match record.intent.replay_disposition {
            PublicOperationReplayDisposition::RetainBuildSuccessRetireAmbiguity
                if instruction == PublicInstruction::Build =>
            {
                false
            }
            PublicOperationReplayDisposition::DataLoss(DataLossReplay::ReplaceOnAmbiguity)
                if instruction == PublicInstruction::DataLoss =>
            {
                false
            }
            PublicOperationReplayDisposition::DataLoss(DataLossReplay::Convergent) => true,
            PublicOperationReplayDisposition::Convergent
            | PublicOperationReplayDisposition::RepeatableRead
            | PublicOperationReplayDisposition::ReevaluateCatchUp
            | PublicOperationReplayDisposition::RetainBuildSuccessRetireAmbiguity
            | PublicOperationReplayDisposition::DataLoss(DataLossReplay::ReplaceOnAmbiguity)
            | PublicOperationReplayDisposition::ExactRemoval
            | PublicOperationReplayDisposition::Containment => repeatable(instruction),
        })
}

fn mode(selection: PublicCatchUpMode) -> ReplicaSetQuorumMode {
    match selection {
        PublicCatchUpMode::WriteQuorum => ReplicaSetQuorumMode::WriteQuorum,
        PublicCatchUpMode::All => ReplicaSetQuorumMode::All,
    }
}

pub(super) fn replica_set(
    input: &PublicConfiguration,
    configuration: &crate::protocol::types::ConfigurationDescriptor,
    current: bool,
) -> ReplicaSetConfiguration {
    ReplicaSetConfiguration {
        configuration: configuration.clone(),
        replicas: input
            .replicas
            .iter()
            .filter(|replica| {
                configuration
                    .members
                    .iter()
                    .any(|member| member.identity == replica.identity)
            })
            .cloned()
            .map(|replica| {
                let mut information: ReplicaInformation = replica.into();
                if !current {
                    information.must_catchup = false;
                }
                information.role = configuration
                    .members
                    .iter()
                    .find(|member| member.identity == information.identity)
                    .expect("filtered configuration member")
                    .role;
                information
            })
            .collect(),
    }
}

pub(super) async fn install(
    store: &dyn AgentStore,
    callbacks: &PublicLifecycleCallbacks,
    input: &PublicConfiguration,
    catch_up: bool,
    instruction: PublicInstruction,
) -> crate::Result<()> {
    input
        .validate()
        .map_err(|error| RuntimeError::AuthorityMismatch(error.into()))?;
    let state = store
        .load_state()
        .await
        .map_err(|error| RuntimeError::Application(error.to_string()))?;
    let local = &state.identity.local_identity;
    if !std::iter::once(&input.current)
        .chain(input.previous.iter())
        .flat_map(|configuration| &configuration.members)
        .any(|member| &member.identity == local)
    {
        return Err(RuntimeError::AuthorityNotAdmitted);
    }
    if let Some(preview) = &state.public_v1_operation_state
        && input.replicas.iter().any(|replica| {
            replica
                .storage_incarnation
                .as_ref()
                .is_some_and(|storage| preview.retired_storage_incarnations.contains(storage))
        })
    {
        return Err(RuntimeError::AuthorityNotAdmitted);
    }
    if input.current.primary_id != local.replica_id {
        // The persisted host instruction installs secondary transport authority.
        // Classic native V1 has no secondary UpdateReplicaSetConfiguration call.
        // Admission is subsequently rechecked against this journal per frame.
        return Ok(());
    }
    let result = if let Some(previous) = &input.previous {
        callbacks
            .primary
            .update_catch_up_replica_set_configuration(
                replica_set(input, &input.current, true),
                replica_set(input, previous, false),
            )
            .await
    } else if catch_up {
        let previous = crate::protocol::types::ConfigurationDescriptor::new(
            input.current.epoch,
            input.current.primary_id,
            Vec::new(),
            0,
        );
        callbacks
            .primary
            .update_catch_up_replica_set_configuration(
                replica_set(input, &input.current, true),
                replica_set(input, &previous, false),
            )
            .await
    } else {
        callbacks
            .primary
            .update_current_replica_set_configuration(replica_set(input, &input.current, true))
            .await
    };
    if let Err(error) = &result {
        callbacks.retry_proof.returned_error(
            PublicCallbackFamily::Configuration,
            instruction,
            error,
        );
    }
    result
}

pub(super) async fn install_current(
    callbacks: &PublicLifecycleCallbacks,
    input: &PublicConfiguration,
    instruction: PublicInstruction,
) -> crate::Result<()> {
    let result = callbacks
        .primary
        .update_current_replica_set_configuration(replica_set(input, &input.current, true))
        .await;
    if let Err(error) = &result {
        callbacks.retry_proof.returned_error(
            PublicCallbackFamily::Configuration,
            instruction,
            error,
        );
    }
    result
}

pub(crate) fn configuration(
    program: &PublicOperationProgram,
    instruction: PublicInstruction,
) -> &PublicConfiguration {
    use PublicInstruction::*;
    match program {
        PublicOperationProgram::Configuration(configuration)
        | PublicOperationProgram::CatchUp { configuration, .. } => configuration,
        PublicOperationProgram::Swap {
            starting,
            refreshed,
            ..
        } => {
            if matches!(instruction, StartingConfiguration | FirstCatchUp) {
                starting
            } else {
                refreshed
            }
        }
        _ => unreachable!("validated configuration instruction"),
    }
}

pub(super) async fn execute(
    store: Arc<dyn AgentStore>,
    intent: PublicOperationIntent,
    callbacks: PublicLifecycleCallbacks,
) -> Result<()> {
    use PublicInstruction::*;
    use PublicInstructionOutcome as O;
    use PublicOperationProgram as P;
    let record = store
        .public_operation_records()
        .await?
        .into_iter()
        .find(|record| record.intent == intent)
        .ok_or_else(|| HostError::Corrupt("missing public program".into()))?;
    let actions = if let Some(ordinary) = &record.ordinary {
        if ordinary.action_format != crate::protocol::public_operations::ORDINARY_ACTION_FORMAT {
            return Err(HostError::Corrupt(
                "incompatible frozen ordinary action format".into(),
            ));
        }
        ordinary.actions.clone()
    } else {
        operation_instructions(&intent)
    };
    let start = record.lifecycle.outcomes.len();
    let mut close_failure = record.lifecycle.outcomes.iter().find_map(|outcome| {
        if let O::CloseFailure { child, error } = outcome {
            Some(format!("{child:?} Close failed: {error}"))
        } else {
            None
        }
    });
    let recovered_terminal = intent.class.is_terminal() && start > 1;
    // Recorded terminal effects still need process-local Abort containment;
    // neither a child diagnostic nor an old Abort receipt is a live proof.
    if recovered_terminal
        || record
            .lifecycle
            .outcomes
            .iter()
            .any(|outcome| matches!(outcome, O::CloseFailure { .. } | O::RebuildRequired(_)))
    {
        if record
            .lifecycle
            .outcomes
            .iter()
            .any(|outcome| matches!(outcome, O::RebuildRequired(_)))
        {
            return Err(HostError::CommandRejected(
                "retained replica rebuild requirement".into(),
            ));
        }
        callbacks.abort_once();
    }
    for (index, instruction) in actions.into_iter().enumerate().skip(start) {
        if intent.goal().is_some()
            && !intent.class.is_terminal()
            && *callbacks.aborted.lock().unwrap()
        {
            return Err(HostError::CommandRejected(
                "ordinary creation was withdrawn".into(),
            ));
        }
        let mut selected_program =
            selected_program(&intent, instruction).expect("validated local program");
        if instruction == ApplicationOpen
            && let P::Open { existing, .. } = &mut selected_program
            && let Some(ordinary) = &record.ordinary
        {
            *existing = ordinary.open_existing.ok_or_else(|| {
                HostError::Corrupt("ordinary Open lacks its frozen local storage mode".into())
            })?;
        }
        let program = &selected_program;
        #[cfg(feature = "testing")]
        if let Some(cut) = &callbacks.cut {
            cut.pause(PublicCutPosition::BeforeInstruction, index).await;
        }
        store
            .public_instruction(&intent, index, instruction, None)
            .await?;
        if let Some(owner) = &callbacks.access_owner {
            owner.selected(instruction);
        }
        #[cfg(feature = "testing")]
        if let Some(cut) = &callbacks.cut {
            cut.pause(PublicCutPosition::InstructionSelected, index)
                .await;
        }
        let mut rebuild_failure = None;
        let mut ineligible_return = None;
        let outcome = match instruction {
            ApplicationOpen => {
                let P::Open { replica, existing } = program else {
                    unreachable!()
                };
                let context = callbacks.open_context.clone().ok_or_else(|| {
                    HostError::CommandRejected("open context is not bound".into())
                })?;
                if &context.identity != replica
                    || (context.mode == crate::application::OpenMode::Existing) != *existing
                {
                    return Err(HostError::IdentityMismatch("open context changed".into()));
                }
                let control = match callbacks.application.clone().open(context).await {
                    Ok(control) => control,
                    Err(error) => {
                        callbacks.abort_once();
                        return Err(error.into());
                    }
                };
                if !callbacks.application_control_matches(&control) {
                    callbacks.abort_once();
                    return Err(HostError::IdentityMismatch(
                        "application returned a different Replicator".into(),
                    ));
                }
                O::Done
            }
            ReplicatorOpen => match callbacks.replicator.open().await {
                Ok(endpoint) if !endpoint.trim().is_empty() => O::Endpoint(endpoint),
                Ok(_) => {
                    callbacks.abort_once();
                    return Err(HostError::CommandRejected(
                        "Replicator Open returned an empty replication endpoint".into(),
                    ));
                }
                Err(error) => {
                    callbacks.abort_once();
                    return Err(error.into());
                }
            },
            ReplicatorRole | ApplicationRole => {
                let (epoch, role) = match program {
                    P::Role { epoch, role } => (*epoch, *role),
                    P::Swap { epoch, handoff, .. } => (*epoch, *handoff),
                    _ => unreachable!(),
                };
                if instruction == ReplicatorRole {
                    let state = store.load_state().await?;
                    let replacement_replay = state
                        .public_v1_operation_state
                        .as_ref()
                        .and_then(|preview| preview.session_renewal.as_ref())
                        .is_some_and(|renewal| {
                            super::is_replacement_role_replay(
                                renewal,
                                &state.identity.local_identity,
                                &intent,
                            )
                        });
                    let result = if replacement_replay {
                        // A replacement may already have this native role. Only the
                        // exact renewal may advance its epoch without same-role ChangeRole.
                        crate::host::service::replay_role_at_epoch(
                            callbacks.replicator.as_ref(),
                            callbacks
                                .native_role_epoch
                                .as_ref()
                                .map(|observe| observe())
                                .transpose()?
                                .flatten(),
                            epoch,
                            role,
                        )
                        .await
                    } else if role == ReplicaRole::Primary {
                        callbacks.change_primary_role(epoch, instruction).await
                    } else {
                        let result = callbacks.replicator.change_role(epoch, role).await;
                        if let Err(error) = &result {
                            callbacks.retry_proof.returned_error(
                                PublicCallbackFamily::ReplicatorRole,
                                instruction,
                                error,
                            );
                        }
                        result
                    };
                    super::local_callback(
                        result,
                        store.as_ref(),
                        &intent,
                        index,
                        instruction,
                        epoch,
                        None,
                    )
                    .await?;
                    O::Done
                } else {
                    O::ApplicationRole(
                        super::local_callback(
                            callbacks.application.change_role(role).await,
                            store.as_ref(),
                            &intent,
                            index,
                            instruction,
                            epoch,
                            Some((
                                &callbacks.retry_proof,
                                PublicCallbackFamily::ApplicationRole,
                            )),
                        )
                        .await?
                        .service_address,
                    )
                }
            }
            ProgramEpoch => {
                let (P::Epoch { epoch } | P::Swap { epoch, .. }) = program else {
                    unreachable!()
                };
                super::local_callback(
                    callbacks.replicator.update_epoch(*epoch).await,
                    store.as_ref(),
                    &intent,
                    index,
                    instruction,
                    *epoch,
                    Some((&callbacks.retry_proof, PublicCallbackFamily::Epoch)),
                )
                .await?;
                O::Done
            }
            Configuration | StartingConfiguration | RefreshedConfiguration => {
                let configuration = configuration(program, instruction);
                if intent.goal().is_some() {
                    validate_admission_boundary(store.as_ref(), &record, &callbacks).await?;
                }
                super::local_callback(
                    install(
                        store.as_ref(),
                        &callbacks,
                        configuration,
                        matches!(program, P::CatchUp { .. } | P::Swap { .. }),
                        instruction,
                    )
                    .await,
                    store.as_ref(),
                    &intent,
                    index,
                    instruction,
                    configuration.current.epoch,
                    None,
                )
                .await?;
                if intent.goal().is_some() {
                    // A successful engine return after invalidation is uncertain,
                    // not an admission receipt or idle-ownership retirement.
                    validate_admission_boundary(store.as_ref(), &record, &callbacks).await?;
                }
                O::Done
            }
            FirstCatchUp | SecondCatchUp => {
                // At a recovery cut, the persisted install result is historical.
                // Reinstall the exact input before reevaluating the wait.
                if index == start && start > 0 {
                    let configuration = configuration(program, instruction);
                    super::local_callback(
                        install(store.as_ref(), &callbacks, configuration, true, instruction).await,
                        store.as_ref(),
                        &intent,
                        index,
                        instruction,
                        configuration.current.epoch,
                        None,
                    )
                    .await?;
                    if let Some(owner) = &callbacks.access_owner {
                        owner.reinstalled(store.as_ref(), configuration).await?;
                    }
                }
                let (P::CatchUp { mode: selected, .. } | P::Swap { mode: selected, .. }) = program
                else {
                    unreachable!()
                };
                let configuration = configuration(program, instruction);
                super::local_callback(
                    callbacks
                        .primary
                        .wait_for_catch_up_quorum(mode(*selected))
                        .await,
                    store.as_ref(),
                    &intent,
                    index,
                    instruction,
                    configuration.current.epoch,
                    Some((&callbacks.retry_proof, PublicCallbackFamily::CatchUp)),
                )
                .await?;
                O::Done
            }
            Progress => {
                let P::Progress { capability } = program else {
                    unreachable!()
                };
                O::Progress(if *capability {
                    callbacks.replicator.catch_up_capability().await?
                } else {
                    callbacks.replicator.current_progress().await?
                })
            }
            Build => {
                let P::Build(build) = program else {
                    unreachable!()
                };
                let mut replica = ReplicaInformation::idle_build(
                    if intent.goal().is_some() {
                        crate::protocol::types::OperationId::default()
                    } else {
                        build.attempt.clone()
                    },
                    build.replica.clone(),
                    build.replication_address.clone(),
                );
                replica.process_session_id = build.process_session_id.clone();
                replica.storage_incarnation = Some(build.target_storage.clone());
                if let Some(goal) = intent.goal() {
                    validate_remote(store.as_ref(), goal, &callbacks).await?;
                }
                match callbacks.primary.build_replica(replica).await {
                    Ok(()) => {
                        if let Some(goal) = intent.goal()
                            && let Err(error) =
                                validate_remote(store.as_ref(), goal, &callbacks).await
                        {
                            ineligible_return = Some(error.to_string());
                        }
                        O::Done
                    }
                    Err(crate::RuntimeError::ReplicaRebuildRequired(failure))
                        if matches!(
                            failure.as_ref(),
                            ReplicaRebuildRequired::RemoteBuildTarget(_)
                        ) =>
                    {
                        let state = store.load_state().await?;
                        let source_storage = state
                            .public_v1_operation_state
                            .as_ref()
                            .and_then(|preview| preview.storage_incarnation.as_ref())
                            .ok_or_else(|| {
                                HostError::CommandRejected(
                                    "public-v1 build lacks source storage incarnation".into(),
                                )
                            })?;
                        let canonical = super::enrich_remote_rebuild(
                            &state.identity,
                            source_storage,
                            &intent,
                            build,
                            *failure,
                        )?;
                        rebuild_failure = Some("remote build target requires replacement");
                        O::RebuildRequired(Box::new(canonical))
                    }
                    Err(crate::RuntimeError::ReplicaRebuildRequired(failure))
                        if matches!(failure.as_ref(), ReplicaRebuildRequired::LocalProvider(_)) =>
                    {
                        let state = store.load_state().await?;
                        let canonical = super::enrich_local_rebuild_at_epoch(
                            &state.identity,
                            &intent,
                            build.epoch,
                            &state.identity.local_identity,
                            *failure,
                        )?;
                        rebuild_failure = Some("local build source requires replacement");
                        O::RebuildRequired(Box::new(canonical))
                    }
                    Err(crate::RuntimeError::ReplicaRebuildRequired(_)) => unreachable!(),
                    Err(error) => return Err(error.into()),
                }
            }
            Remove => {
                let replica_id = match program {
                    P::Remove(build) => build.replica.replica_id,
                    P::RetireIdleReplica { replica_id } => *replica_id,
                    _ => unreachable!(),
                };
                callbacks.primary.remove_replica(replica_id).await?;
                O::Done
            }
            ReplicatorClose | ApplicationClose => {
                if (recovered_terminal || close_failure.is_some()) && intent.goal().is_none() {
                    O::Done
                } else {
                    let (child, result) = if instruction == ReplicatorClose {
                        (
                            PublicCloseChild::Replicator,
                            callbacks.replicator.close().await,
                        )
                    } else {
                        (
                            PublicCloseChild::Application,
                            callbacks.application.close().await,
                        )
                    };
                    match result {
                        Ok(()) => O::Done,
                        Err(error) => {
                            callbacks.abort_once();
                            close_failure = Some(format!("{child:?} Close failed: {error}"));
                            O::CloseFailure {
                                child,
                                error: error.to_string(),
                            }
                        }
                    }
                }
            }
            Abort => {
                callbacks.abort_once();
                O::Done
            }
            Cleanup => {
                let cleanup = callbacks.terminal_cleanup.as_ref().ok_or_else(|| {
                    HostError::CommandRejected("terminal cleanup is not bound".into())
                })?;
                if let Err(error) = cleanup.wait().await {
                    return Err(HostError::CommandRejected(match &close_failure {
                        Some(failure) => format!("{failure}; cleanup failed: {error}"),
                        None => format!("terminal cleanup failed: {error}"),
                    }));
                }
                O::Done
            }
            Revoke => {
                (callbacks.revoke_access)()?;
                O::Done
            }
            _ => unreachable!("role instruction in public program"),
        };
        if let Some(owner) = &callbacks.access_owner {
            owner.callback_succeeded(instruction);
        }
        #[cfg(feature = "testing")]
        if let Some(cut) = &callbacks.cut {
            cut.pause(PublicCutPosition::CallbackSuccess, index).await;
        }
        store
            .public_instruction(&intent, index, instruction, Some(outcome))
            .await?;
        if let Some(owner) = &callbacks.access_owner {
            owner.receipt(store.as_ref(), instruction).await?;
        }
        #[cfg(feature = "testing")]
        if let Some(cut) = &callbacks.cut {
            cut.pause(PublicCutPosition::InstructionApplied, index)
                .await;
        }
        if let Some(message) = rebuild_failure {
            return Err(HostError::CommandRejected(message.into()));
        }
        if let Some(message) = ineligible_return {
            return Err(HostError::CommandRejected(format!(
                "ordinary callback returned successfully but its scope is no longer eligible: {message}"
            )));
        }
    }
    // A durable Cleanup receipt belongs to the previous owner, not this
    // process. Even an otherwise fully recorded program must join its owner.
    if intent.class.is_terminal() {
        let cleanup = callbacks
            .terminal_cleanup
            .as_ref()
            .ok_or_else(|| HostError::CommandRejected("terminal cleanup is not bound".into()))?;
        if let Err(error) = cleanup.wait().await {
            return Err(HostError::CommandRejected(match close_failure {
                Some(failure) => format!("{failure}; cleanup failed: {error}"),
                None => format!("terminal cleanup failed: {error}"),
            }));
        }
    }
    if let Some(failure) = close_failure {
        return Err(HostError::CommandRejected(failure));
    }
    Ok(())
}

async fn validate_admission_boundary(
    store: &dyn AgentStore,
    record: &PublicOperationRecord,
    callbacks: &PublicLifecycleCallbacks,
) -> Result<()> {
    use crate::protocol::public_operations::*;
    let goal = record.intent.goal().expect("ordinary goal");
    let admissions = &record
        .ordinary
        .as_ref()
        .expect("frozen actions")
        .ready_admissions;
    for remote in admissions {
        let validate_source = |mut state: crate::host::state::AgentState| -> Result<()> {
            if *callbacks.aborted.lock().unwrap() {
                return Err(HostError::CommandRejected(
                    "admission source withdrawn".into(),
                ));
            }
            let preview = state
                .public_v1_operation_state
                .as_mut()
                .ok_or_else(|| HostError::Corrupt("missing ordinary state".into()))?;
            let current = preview
                .operations
                .remove(&goal.operation_id)
                .ok_or_else(|| HostError::Corrupt("missing admission owner".into()))?;
            if current.intent != record.intent
                || current.superseded_by.is_some()
                || current.ordinary != record.ordinary
                || super::super::ordinary_goal::publication(
                    &state,
                    &goal.scope.process_session_id,
                    &goal.scope.creation_id,
                    false,
                )
                .as_ref()
                    != Some(&remote.source)
                || !super::super::ordinary_goal::remote_observations(&state, Some(&remote.source))
                    .iter()
                    .any(|observation| {
                        observation.selection == *remote
                            && observation.state == RemoteReplicaState::Ready
                            && observation.accepted
                    })
            {
                return Err(HostError::CommandRejected(
                    "admission source or accepted Ready changed".into(),
                ));
            }
            Ok(())
        };
        validate_source(store.load_state().await?)?;
        let query = ReplicaDiscoveryQuery {
            format: ORDINARY_ACTION_FORMAT,
            history: remote.target.scope.history.clone(),
            replica: remote.target.scope.storage.replica.clone(),
            process_session_id: Some(remote.target.scope.process_session_id.clone()),
        };
        let target = crate::control::resolve_replica_publication(
            &remote.target_control_address,
            callbacks
                .peer_credentials
                .as_ref()
                .ok_or_else(|| HostError::CommandRejected("missing peer credentials".into()))?,
            &query,
        )
        .await?;
        if target.as_ref() != Some(&remote.target) {
            return Err(HostError::CommandRejected(
                "admission target publication changed".into(),
            ));
        }
        validate_source(store.load_state().await?)?;
    }
    Ok(())
}

async fn validate_remote(
    store: &dyn AgentStore,
    goal: &crate::protocol::public_operations::ReplicaGoal,
    callbacks: &PublicLifecycleCallbacks,
) -> Result<()> {
    use crate::protocol::public_operations::*;
    let remote = goal.remote().expect("remote goal");
    let state = store.load_state().await?;
    if *callbacks.aborted.lock().unwrap()
        || super::super::ordinary_goal::publication(
            &state,
            &goal.scope.process_session_id,
            &goal.scope.creation_id,
            false,
        )
        .as_ref()
            != Some(&remote.source)
    {
        return Err(HostError::CommandRejected(
            "remote source publication was withdrawn".into(),
        ));
    }
    let credentials = callbacks.peer_credentials.as_ref().ok_or_else(|| {
        HostError::CommandRejected("authenticated discovery credentials unavailable".into())
    })?;
    let query = ReplicaDiscoveryQuery {
        format: ORDINARY_ACTION_FORMAT,
        history: remote.target.scope.history.clone(),
        replica: remote.target.scope.storage.replica.clone(),
        process_session_id: Some(remote.target.scope.process_session_id.clone()),
    };
    let target = crate::control::resolve_replica_publication(
        &remote.target_control_address,
        credentials,
        &query,
    )
    .await?;
    if target.as_ref() != Some(&remote.target) {
        return Err(HostError::CommandRejected(
            "remote target publication changed".into(),
        ));
    }
    Ok(())
}
