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
                configuration
                    .members
                    .iter()
                    .any(|member| member.identity == replica.identity)
            })
            .cloned()
            .map(Into::into)
            .collect(),
    }
}

pub(super) async fn install(
    callbacks: &PublicLifecycleCallbacks,
    input: &PublicConfiguration,
    authorization_context: Option<&mut crate::replicator::PublicAuthorizationRuntimeContext>,
) -> Result<()> {
    let authorization_snapshot = if let Some(context) = authorization_context.as_deref() {
        crate::replicator::admit_public_peer_configuration(context, input)?
    } else if input
        .replicas
        .iter()
        .any(|replica| replica.peer_authorization.is_some())
    {
        return Err(HostError::CommandRejected(
            "signed peer authorization requires public-v1 runtime context".into(),
        ));
    } else {
        None
    };
    let result = if let Some(previous) = &input.previous {
        callbacks
            .primary
            .update_catch_up_replica_set_configuration(
                replica_set(input, &input.current),
                replica_set(input, previous),
            )
            .await
    } else {
        callbacks
            .primary
            .update_current_replica_set_configuration(replica_set(input, &input.current))
            .await
    };
    if let Err(error) = result {
        if let Some(snapshot) = authorization_snapshot {
            crate::replicator::restore_public_peer_authorization(snapshot)?;
        }
        return Err(error.into());
    }
    if let Some(context) = authorization_context {
        context.epoch = input.current.epoch;
        context.previous_configuration_id = input
            .previous
            .as_ref()
            .map(|configuration| configuration.configuration_id.clone());
        context.current_configuration_id = Some(input.current.configuration_id.clone());
    }
    Ok(())
}

pub(super) async fn install_current(
    callbacks: &PublicLifecycleCallbacks,
    input: &PublicConfiguration,
    authorization_context: Option<&mut crate::replicator::PublicAuthorizationRuntimeContext>,
) -> Result<()> {
    let authorization_snapshot = if let Some(context) = authorization_context.as_deref() {
        crate::replicator::admit_public_peer_configuration(context, input)?
    } else if input
        .replicas
        .iter()
        .any(|replica| replica.peer_authorization.is_some())
    {
        return Err(HostError::CommandRejected(
            "signed peer authorization requires public-v1 runtime context".into(),
        ));
    } else {
        None
    };
    let result = callbacks
        .primary
        .update_current_replica_set_configuration(replica_set(input, &input.current))
        .await;
    if let Err(error) = result {
        if let Some(snapshot) = authorization_snapshot {
            crate::replicator::restore_public_peer_authorization(snapshot)?;
        }
        return Err(error.into());
    }
    if let Some(context) = authorization_context {
        context.epoch = input.current.epoch;
        context.previous_configuration_id = input
            .previous
            .as_ref()
            .map(|configuration| configuration.configuration_id.clone());
        context.current_configuration_id = Some(input.current.configuration_id.clone());
    }
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

pub(crate) async fn abort(
    registry: &Arc<PartitionOperationRegistry>,
    store: Arc<dyn AgentStore>,
    intent: PublicOperationIntent,
    mut callbacks: PublicLifecycleCallbacks,
) -> Result<()> {
    intent
        .validate()
        .map_err(|message| HostError::CommandRejected(message.into()))?;
    callbacks.validate_intent_capabilities(&intent)?;
    if intent.program != Some(PublicOperationProgram::Abort) {
        return Err(HostError::CommandRejected(
            "synchronous abort requires exact abort input".into(),
        ));
    }
    let operation = registry.admit(intent.clone()).await?;
    let should_abort = !matches!(
        operation.snapshot().stage,
        PublicOperationStage::Completed | PublicOperationStage::ContainmentPending
    );
    registry.fence();
    callbacks.aborted = registry.abort_guard();
    if should_abort {
        callbacks.abort_once();
    }
    let owner = registry.clone();
    registry.own_control_task(async move {
        if let Err(error) = launch(&owner, store, intent, callbacks).await {
            tracing::warn!(%error, "aborted preview requires durable containment");
        }
    });
    Ok(())
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
    let mut authorization_context = if intent.preview.is_public_v1() {
        let state = store.load_state().await?;
        let preview = state
            .public_operation_preview
            .as_ref()
            .ok_or_else(|| HostError::Corrupt("missing public-v1 preview state".into()))?;
        let context = crate::replicator::PublicAuthorizationRuntimeContext {
            identity: state.identity.local_identity.clone(),
            history: preview
                .history
                .clone()
                .ok_or_else(|| HostError::Corrupt("missing public-v1 history".into()))?,
            storage_incarnation: preview.storage_incarnation.clone().ok_or_else(|| {
                HostError::Corrupt("missing public-v1 storage incarnation".into())
            })?,
            process_session_id: intent.process_session_id.clone(),
            role: state.role,
            epoch: state
                .current_configuration
                .as_ref()
                .map_or(state.highest_epoch, |configuration| configuration.epoch),
            previous_configuration_id: state
                .previous_configuration
                .as_ref()
                .map(|configuration| configuration.configuration_id.clone()),
            current_configuration_id: state
                .current_configuration
                .as_ref()
                .map(|configuration| configuration.configuration_id.clone()),
        };
        crate::replicator::bind_public_authorization_context(context.clone())?;
        Some(context)
    } else {
        None
    };
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
                let control = callbacks.application.clone().open(context).await?;
                if !Arc::ptr_eq(&control, &callbacks.replicator) {
                    return Err(HostError::IdentityMismatch(
                        "application returned a different Replicator".into(),
                    ));
                }
                O::Done
            }
            ReplicatorOpen => O::Endpoint(callbacks.replicator.open().await?),
            ReplicatorRole | ApplicationRole => {
                let (epoch, role) = match program {
                    P::Role { epoch, role } => (*epoch, *role),
                    P::Swap { epoch, handoff, .. } => (*epoch, *handoff),
                    _ => unreachable!(),
                };
                if instruction == ReplicatorRole {
                    callbacks.replicator.change_role(epoch, role).await?;
                    if let Some(context) = authorization_context.as_mut() {
                        context.epoch = epoch;
                        context.role = role;
                        crate::replicator::advance_public_authorization_context(context.clone())?;
                    }
                    O::Done
                } else {
                    O::ApplicationRole(
                        callbacks
                            .application
                            .change_role(role)
                            .await?
                            .service_address,
                    )
                }
            }
            ProgramEpoch => {
                let (P::Epoch { epoch } | P::Swap { epoch, .. }) = program else {
                    unreachable!()
                };
                callbacks.replicator.update_epoch(*epoch).await?;
                if let Some(context) = authorization_context.as_mut() {
                    context.epoch = *epoch;
                    crate::replicator::advance_public_authorization_context(context.clone())?;
                }
                O::Done
            }
            Configuration | StartingConfiguration | RefreshedConfiguration => {
                install(
                    &callbacks,
                    configuration(program, instruction),
                    authorization_context.as_mut(),
                )
                .await?;
                O::Done
            }
            FirstCatchUp | SecondCatchUp => {
                // At a recovery cut, the persisted install result is historical.
                // Reinstall the exact input before reevaluating the wait.
                if index == start && start > 0 {
                    install(
                        &callbacks,
                        configuration(program, instruction),
                        authorization_context.as_mut(),
                    )
                    .await?;
                }
                let (P::CatchUp { mode: selected, .. } | P::Swap { mode: selected, .. }) = program
                else {
                    unreachable!()
                };
                callbacks
                    .primary
                    .wait_for_catch_up_quorum(mode(*selected))
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
                replica.build_authorization = build.build_authorization.clone();
                replica.storage_incarnation = build
                    .build_authorization
                    .as_ref()
                    .map(|authorization| authorization.claims.target.storage_incarnation.clone());
                if let Some(context) = &authorization_context {
                    crate::replicator::authorize_public_build_descriptor(
                        context,
                        context.epoch,
                        &intent.operation_id,
                        &replica,
                    )?;
                } else if replica.build_authorization.is_some() {
                    return Err(HostError::CommandRejected(
                        "signed build authorization requires public-v1 runtime context".into(),
                    ));
                }
                match callbacks.primary.build_replica(replica).await {
                    Ok(()) => O::Done,
                    Err(crate::RuntimeError::ReplicaRebuildRequired(failure))
                        if matches!(
                            failure.as_ref(),
                            ReplicaRebuildRequired::RemoteBuildTarget(_)
                        ) =>
                    {
                        let state = store.load_state().await?;
                        let canonical = super::enrich_remote_rebuild(
                            &state.identity,
                            &intent,
                            build,
                            *failure,
                        )?;
                        rebuild_failure = Some("remote build target requires replacement");
                        O::RebuildRequired(Box::new(canonical))
                    }
                    Err(crate::RuntimeError::ReplicaRebuildRequired(_)) => {
                        return Err(HostError::CommandRejected(
                            "local-provider rebuild failure substituted for remote build failure"
                                .into(),
                        ));
                    }
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
                    Ok(()) => {
                        if instruction == ReplicatorClose
                            && let Some(context) = authorization_context.as_ref()
                        {
                            crate::replicator::clear_public_authorization_context(
                                &context.identity,
                            );
                        }
                        O::Done
                    }
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
                if let Some(context) = authorization_context.as_ref() {
                    crate::replicator::clear_public_authorization_context(&context.identity);
                }
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
