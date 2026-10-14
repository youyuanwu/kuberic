use super::*;
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
        Some(P::Remove(_)) => vec![Remove],
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
    if record.intent.preview.is_legacy_preview() {
        return record.lifecycle.in_flight.is_none_or(repeatable);
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
                configuration.members.iter().any(|member| {
                    member.identity == replica.identity && member.role == replica.role
                })
            })
            .cloned()
            .map(Into::into)
            .collect(),
    }
}

pub(super) async fn install(
    callbacks: &PublicLifecycleCallbacks,
    input: &PublicConfiguration,
) -> crate::Result<()> {
    if let Some(previous) = &input.previous {
        callbacks
            .primary
            .update_catch_up_replica_set_configuration(
                replica_set(input, &input.current),
                replica_set(input, previous),
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
                Ok(endpoint) => O::Endpoint(endpoint),
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
                    super::local_callback(
                        callbacks.replicator.change_role(epoch, role).await,
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
                    install(&callbacks, configuration).await,
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
                        install(&callbacks, configuration).await,
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
                            .public_operation_preview
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
                let P::Remove(build) = program else {
                    unreachable!()
                };
                callbacks
                    .primary
                    .remove_replica(build.replica.replica_id)
                    .await?;
                O::Done
            }
            ReplicatorClose | ApplicationClose => {
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
                        O::CloseFailure {
                            child,
                            error: error.to_string(),
                        }
                    }
                }
            }
            Abort => {
                callbacks.abort_once();
                O::Done
            }
            Cleanup => {
                if let Some(mut witness) = callbacks.containment.clone() {
                    witness
                        .wait_for(|contained| *contained)
                        .await
                        .map_err(|_| {
                            HostError::CommandRejected("terminal containment witness lost".into())
                        })?;
                }
                O::Done
            }
            Revoke => {
                callbacks.application.change_role(ReplicaRole::None).await?;
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
    Ok(())
}
