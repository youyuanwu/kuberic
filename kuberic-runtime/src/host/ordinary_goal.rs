use super::state::*;
use super::{HostError, Result};
use crate::protocol::public_operations::*;
use crate::protocol::types::*;

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

fn new_voters<'a>(
    state: &AgentState,
    goal: &'a ReplicaGoal,
) -> Result<Vec<&'a ReplicaPeerDescription>> {
    // Initial Prepare recovers the bootstrap topology without granting access.
    // Subsequent voting additions must cross the accepted idle-Ready boundary.
    if goal.role() != Some(ReplicaRole::Primary)
        || !matches!(goal.desired, ReplicaGoalKind::Reconfigure { .. })
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
                                    && known.process_session_id == peer.process_session_id
                                    && known.replication_address == peer.replication_address
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
                        && matches_source(&remote.selection, goal)
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
            if !matches_source(remote, goal) {
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
            if previous.class().is_terminal()
                || terminal_for_process(preview, &goal.scope.process_session_id)
            {
                return Err(reject(
                    "withdrawn creation requires contained process replacement",
                ));
            }
            if !previous.scope.compatible_with(&goal.scope) {
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
    if matches!(goal.desired, ReplicaGoalKind::Discover)
        && preview.operations.values().any(|record| {
            record.intent.goal().is_some_and(|previous| {
                previous.scope.process_session_id == goal.scope.process_session_id
                    && matches!(
                        previous.desired,
                        ReplicaGoalKind::Discover | ReplicaGoalKind::Prepare { .. }
                    )
            })
        })
    {
        return Err(reject(
            "creation already has a discovery or preparation goal",
        ));
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
            != Some(&remote.source)
        {
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
    let terminals = preview
        .operations
        .values()
        .filter(|record| record.intent.class.is_terminal())
        .collect::<Vec<_>>();
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
            record.intent.goal().is_none()
                || &record.intent.process_session_id == process
                || record.stage != PublicOperationStage::Completed
                || record.disposition != Some(PublicOperationDisposition::Succeeded)
                || record.lifecycle.in_flight.is_some()
        })
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
    let already_open =
        matches!(goal.desired, ReplicaGoalKind::Prepare { .. }) && opened_creation(state, goal);
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
                        && record
                            .ordinary
                            .as_ref()
                            .is_some_and(|ordinary| !ordinary.logically_retired)
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
            ReplicaGoalKind::Discover | ReplicaGoalKind::Prepare { .. }
        )
        .then_some(state.application_open_mode() == crate::application::OpenMode::Existing),
        already_open,
        accepted: false,
        logically_retired: false,
        host_settled: false,
        ready_admissions: admission_ready(state, goal)?,
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
                        previous.scope.compatible_with(&goal.scope)
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
                        .any(|outcome| matches!(outcome, PublicInstructionOutcome::Endpoint(_)))
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
    if withdrawn || terminal_for_process(preview, process) {
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
            .find_map(|outcome| match outcome {
                PublicInstructionOutcome::Endpoint(endpoint) => Some(endpoint.clone()),
                _ => None,
            })
    })?;
    let record = records.last()?;
    let goal = record.intent.goal()?;
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
    Some(ReplicaPublication {
        format: ORDINARY_ACTION_FORMAT,
        scope: goal.scope.clone(),
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
                goal,
                status,
                selected_call: record
                    .lifecycle
                    .in_flight
                    .map(|instruction| format!("{instruction:?}")),
                receipts: record.lifecycle.outcomes.len(),
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
        process_session_id: process,
        creation_id: creation,
        publication,
        goals,
        remote_operations,
        progress: ProgressQueryResult::NotQueried("ordinary progress has not been sampled".into()),
        capability: ProgressQueryResult::NotQueried(
            "ordinary capability has not been sampled".into(),
        ),
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
    if acknowledgement.logically_retired && !ordinary.logically_retired {
        return Err(reject(
            "controller acknowledgement cannot invent local idle retirement",
        ));
    }
    Ok(())
}
