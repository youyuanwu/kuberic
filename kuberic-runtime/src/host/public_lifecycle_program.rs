use super::*;
use crate::host::state::{PublicCloseChild, PublicInstructionOutcome};
use crate::protocol::public_operations::{
    PublicCatchUpMode, PublicConfiguration, PublicOperationProgram,
};
use crate::replicator::ReplicaInformation;

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
    record.lifecycle.in_flight.is_none_or(repeatable)
}

fn mode(selection: PublicCatchUpMode) -> ReplicaSetQuorumMode {
    match selection {
        PublicCatchUpMode::WriteQuorum => ReplicaSetQuorumMode::WriteQuorum,
        PublicCatchUpMode::All => ReplicaSetQuorumMode::All,
    }
}

async fn install(callbacks: &PublicLifecycleCallbacks, input: &PublicConfiguration) -> Result<()> {
    if let Some(previous) = &input.previous {
        callbacks
            .primary
            .update_catch_up_replica_set_configuration(
                input.current.clone().into(),
                previous.clone().into(),
            )
            .await?;
    } else {
        callbacks
            .primary
            .update_current_replica_set_configuration(input.current.clone().into())
            .await?;
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
    let start = record.lifecycle.outcomes.len();
    // An interrupted close that already recorded a child failure still needs
    // process-local abort containment; the durable diagnostic is not a proof.
    if record
        .lifecycle
        .outcomes
        .iter()
        .any(|outcome| matches!(outcome, O::CloseFailure { .. }))
    {
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
                O::Done
            }
            Configuration | StartingConfiguration | RefreshedConfiguration => {
                install(&callbacks, configuration(program, instruction)).await?;
                O::Done
            }
            FirstCatchUp | SecondCatchUp => {
                // At a recovery cut, the persisted install result is historical.
                // Reinstall the exact input before reevaluating the wait.
                if index == start && start > 0 {
                    install(&callbacks, configuration(program, instruction)).await?;
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
                callbacks
                    .primary
                    .build_replica(ReplicaInformation {
                        build_id: build.attempt.clone(),
                        identity: build.replica.clone(),
                        replication_address: build.replication_address.clone(),
                        process_session_id: build.process_session_id.clone(),
                        role: ReplicaRole::IdleSecondary,
                        current_progress: 0,
                        catch_up_capability: 0,
                    })
                    .await?;
                O::Done
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
            Revoke => O::Done,
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
    }
    Ok(())
}
