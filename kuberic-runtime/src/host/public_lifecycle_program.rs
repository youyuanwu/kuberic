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

pub(crate) fn repeatable(instruction: PublicInstruction) -> bool {
    !matches!(
        instruction,
        PublicInstruction::Build | PublicInstruction::DataLoss
    )
}

pub(crate) fn replayable(record: &PublicOperationRecord) -> bool {
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
    if let Some(previous) = &input.previous {
        callbacks
            .primary
            .update_catch_up_replica_set_configuration(
                replica_set(input, &input.current),
                replica_set(input, previous),
            )
            .await?;
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
                replica_set(input, &input.current),
                replica_set(input, &previous),
            )
            .await?;
    } else {
        callbacks
            .primary
            .update_current_replica_set_configuration(replica_set(input, &input.current))
            .await?;
    }
    Ok(())
}

pub(super) async fn install_current(
    callbacks: &PublicLifecycleCallbacks,
    input: &PublicConfiguration,
) -> crate::Result<()> {
    callbacks
        .primary
        .update_current_replica_set_configuration(replica_set(input, &input.current))
        .await?;
    Ok(())
}

fn configuration(
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
    let program = intent.program.as_ref().expect("validated program");
    let record = store
        .public_operation_records()
        .await?
        .into_iter()
        .find(|record| record.intent == intent)
        .ok_or_else(|| HostError::Corrupt("missing public program".into()))?;
    let start = record.lifecycle.outcomes.len();
    let mut close_failure = record.lifecycle.outcomes.iter().find_map(|outcome| {
        if let O::CloseFailure { child, error } = outcome {
            Some(format!("{child:?} Close failed: {error}"))
        } else {
            None
        }
    });
    // An interrupted close that already recorded a child failure still needs
    // process-local abort containment; the durable diagnostic is not a proof.
    if record
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
    for (index, instruction) in operation_instructions(&intent)
        .into_iter()
        .enumerate()
        .skip(start)
    {
        #[cfg(all(test, feature = "testing"))]
        if let Some(cut) = &callbacks.cut {
            cut.pause(PublicCutPosition::BeforeInstruction, index).await;
        }
        store
            .public_instruction(&intent, index, instruction, None)
            .await?;
        let mut rebuild_failure = None;
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
                    } else {
                        callbacks.replicator.change_role(epoch, role).await
                    };
                    super::local_callback(
                        result,
                        store.as_ref(),
                        &intent,
                        index,
                        instruction,
                        epoch,
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
                )
                .await?;
                O::Done
            }
            Configuration | StartingConfiguration | RefreshedConfiguration => {
                let configuration = configuration(program, instruction);
                super::local_callback(
                    install(
                        store.as_ref(),
                        &callbacks,
                        configuration,
                        matches!(program, P::CatchUp { .. } | P::Swap { .. }),
                    )
                    .await,
                    store.as_ref(),
                    &intent,
                    index,
                    instruction,
                    configuration.current.epoch,
                )
                .await?;
                O::Done
            }
            FirstCatchUp | SecondCatchUp => {
                // At a recovery cut, the persisted install result is historical.
                // Reinstall the exact input before reevaluating the wait.
                if index == start && start > 0 {
                    let configuration = configuration(program, instruction);
                    super::local_callback(
                        install(store.as_ref(), &callbacks, configuration, true).await,
                        store.as_ref(),
                        &intent,
                        index,
                        instruction,
                        configuration.current.epoch,
                    )
                    .await?;
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
                    build.attempt.clone(),
                    build.replica.clone(),
                    build.replication_address.clone(),
                );
                replica.process_session_id = build.process_session_id.clone();
                replica.storage_incarnation = Some(build.target_storage.clone());
                match callbacks.primary.build_replica(replica).await {
                    Ok(()) => O::Done,
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
                if close_failure.is_some() {
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
        #[cfg(all(test, feature = "testing"))]
        if let Some(cut) = &callbacks.cut {
            cut.pause(PublicCutPosition::CallbackSuccess, index).await;
        }
        store
            .public_instruction(&intent, index, instruction, Some(outcome))
            .await?;
        #[cfg(all(test, feature = "testing"))]
        if let Some(cut) = &callbacks.cut {
            cut.pause(PublicCutPosition::InstructionApplied, index)
                .await;
        }
        if let Some(message) = rebuild_failure {
            return Err(HostError::CommandRejected(message.into()));
        }
    }
    match close_failure {
        Some(error) => Err(HostError::CommandRejected(error)),
        None => Ok(()),
    }
}
