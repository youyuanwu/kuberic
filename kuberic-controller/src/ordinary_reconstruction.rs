use super::*;

fn role(goal: &ReplicaGoal) -> Option<ReplicaRole> {
    match &goal.desired {
        ReplicaGoalKind::Prepare { role, .. } | ReplicaGoalKind::Reconfigure { role, .. } => {
            Some(*role)
        }
        ReplicaGoalKind::RefreshPrimary { .. } => Some(ReplicaRole::Primary),
        ReplicaGoalKind::RestoreRetainedMember { .. } => Some(ReplicaRole::ActiveSecondary),
        ReplicaGoalKind::RecoverContainedCreation { recovery } => Some(recovery.role),
        _ => None,
    }
}

pub(super) fn achieved_scope(peer: &ReplicaAgentSnapshot, goal: &ReplicaGoal) -> bool {
    peer.format == ORDINARY_ACTION_FORMAT
        && peer.history == goal.scope.history
        && peer.storage == goal.scope.storage
        && peer.process_session_id == goal.scope.process_session_id
        && peer.creation_id == goal.scope.creation_id
        && !peer.withdrawn
        && peer
            .goals
            .iter()
            .any(|entry| entry.goal == *goal && entry.status == ReplicaGoalStatus::Achieved)
        && peer.publication.as_ref().is_some_and(|publication| {
            publication.stage == ReplicaPublicationStage::AchievedScope
                && publication.scope == goal.scope
                && Some(publication.role) == role(goal)
        })
}

/// Only an actually accepted, joined recovery of this exact old dependency
/// releases its veto. Historical errors/results are never rewritten.
pub(super) fn resolved(
    status: &KubericSetStatus,
    peers: &BTreeMap<ReplicaId, &ReplicaAgentSnapshot>,
    old: &RetainedReplicaGoal,
) -> bool {
    let Some(peer) = peers.get(&old.goal.scope.storage.replica.replica_id) else {
        return false;
    };
    if original_member_resolved(status, peers, old, peer) {
        return true;
    }
    peer.history == old.goal.scope.history
        && peer.storage == old.goal.scope.storage
        && peer.process_session_id != old.goal.scope.process_session_id
        && peer.creation_id != old.goal.scope.creation_id
        && status.replica_goals.iter().any(|entry| {
            let ReplicaGoalKind::RecoverContainedCreation { recovery } = &entry.goal.desired else {
                return false;
            };
            recovery.predecessor_process_session_id == old.goal.scope.process_session_id
                && recovery
                    .addition
                    .as_ref()
                    .is_none_or(|addition| addition.phase == MembershipAdditionPhase::Current)
                && recovery.predecessor_creation_id == old.goal.scope.creation_id
                && recovery
                    .predecessor_operations
                    .contains(&old.goal.operation_id)
                && entry.goal.scope.history == peer.history
                && entry.goal.scope.storage == peer.storage
                && entry.outcome.as_ref().is_some_and(|outcome| {
                    outcome.goal == entry.goal
                        && outcome.status == ReplicaGoalStatus::Achieved
                        && outcome.host_settled
                })
                && peer.goals.iter().any(|outcome| {
                    outcome.goal == entry.goal
                        && outcome.status == ReplicaGoalStatus::Achieved
                        && outcome.host_settled
                })
        })
}

fn final_owner(peer: &ReplicaAgentSnapshot, current: &ConfigurationDescriptor) -> bool {
    let Some(publication) = peer.publication.as_ref() else {
        return false;
    };
    let Some(facts) = peer.native_lifecycle.as_ref() else {
        return false;
    };
    !peer.withdrawn
        && publication.stage == ReplicaPublicationStage::AchievedScope
        && publication.scope.history == peer.history
        && publication.scope.storage == peer.storage
        && publication.scope.process_session_id == peer.process_session_id
        && publication.scope.creation_id == peer.creation_id
        && publication.scope.configuration.current == *current
        && publication.scope.configuration.previous.is_none()
        && facts.scope == publication.scope
        && facts.state == NativeReplicaState::Ready
        && facts.native_role == publication.role
        && facts.application_role == Some(publication.role)
        && facts.applied_primary_fence == Some(current.epoch)
        && facts
            .deactivation
            .as_ref()
            .is_some_and(|history| history.primary_fence == current.epoch)
        && peer.goals.iter().any(|entry| {
            entry.goal.operation_id == facts.operation_id
                && entry.goal.scope == facts.scope
                && entry.goal.revision == publication.generation
                && entry.status == ReplicaGoalStatus::Achieved
                && entry.host_settled
                && entry.selected_call.is_none()
                && matches!(
                    entry.goal.desired,
                    ReplicaGoalKind::Reconfigure {
                        role: ReplicaRole::Primary,
                        catch_up: Some(PublicCatchUpMode::WriteQuorum)
                    } | ReplicaGoalKind::Reconfigure {
                        role: ReplicaRole::ActiveSecondary,
                        catch_up: None
                    }
                )
        })
}

fn original_member_resolved(
    status: &KubericSetStatus,
    peers: &BTreeMap<ReplicaId, &ReplicaAgentSnapshot>,
    old: &RetainedReplicaGoal,
    peer: &ReplicaAgentSnapshot,
) -> bool {
    if peer.history != old.goal.scope.history
        || peer.storage != old.goal.scope.storage
        || peer.process_session_id == old.goal.scope.process_session_id
        || peer.creation_id == old.goal.scope.creation_id
    {
        return false;
    }
    status.replica_goals.iter().any(|build| {
        let ReplicaGoalKind::Build { remote } = &build.goal.desired else {
            return false;
        };
        let Some(prepare) = remote.original_member.as_deref() else {
            return false;
        };
        let ReplicaGoalKind::Prepare {
            rebuild: Some(rebuild),
            ..
        } = &prepare.desired
        else {
            return false;
        };
        let Some(original) = rebuild.inspection.scope.configuration.previous.as_ref() else {
            return false;
        };
        let current = &remote.source.scope.configuration.current;
        if remote.source.scope.configuration.previous.as_ref() != Some(original)
            || current.write_quorum != original.write_quorum
            || !current.members.iter().all(|member| {
                original
                    .members
                    .iter()
                    .any(|old| old.identity == member.identity)
            })
            || !(old.goal.scope.configuration.current == *original
                || old.goal.scope.configuration.previous.as_ref() == Some(original)
                    && old.goal.scope.configuration.current == *current)
            || !achieved(build)
            || !build
                .outcome
                .as_ref()
                .is_some_and(|outcome| outcome.host_settled)
            || !current
                .members
                .iter()
                .any(|member| member.identity == peer.storage.replica)
            || !final_owner(peer, current)
        {
            return false;
        }
        let Some(source) = peers.get(&current.primary_id) else {
            return false;
        };
        final_owner(source, current)
            && source.goals.iter().any(|entry| {
                entry.goal == build.goal
                    && entry.status == ReplicaGoalStatus::Achieved
                    && entry.host_settled
            })
            && current.members.iter().all(|member| {
                peers
                    .get(&member.identity.replica_id)
                    .is_some_and(|target| {
                        final_owner(target, current)
                            && target
                                .native_lifecycle
                                .as_ref()
                                .and_then(|facts| facts.deactivation.as_ref())
                                == source
                                    .native_lifecycle
                                    .as_ref()
                                    .and_then(|facts| facts.deactivation.as_ref())
                    })
            })
            && peers
                .get(&prepare.scope.storage.replica.replica_id)
                .is_some_and(|target| {
                    target.storage == prepare.scope.storage
                        && target.process_session_id == prepare.scope.process_session_id
                        && target.creation_id == prepare.scope.creation_id
                        && target.goals.iter().any(|entry| {
                            entry.goal == *prepare
                                && entry.status == ReplicaGoalStatus::Achieved
                                && entry.host_settled
                        })
                })
    })
}

pub(super) fn next_generation(
    status: &KubericSetStatus,
    source: &ReplicaAgentSnapshot,
    target: &ReplicaPublication,
) -> Result<u64> {
    let source_epoch = source
        .publication
        .as_ref()
        .ok_or_else(|| invalid("remote generation requires an actual source publication"))?
        .scope
        .epoch;
    let matches_target = |remote: &RemoteReplicaSelection| {
        remote.target.scope.storage == target.scope.storage
            && remote.target.scope.process_session_id == target.scope.process_session_id
            && remote.target.scope.creation_id == target.scope.creation_id
            && remote.source.scope.history == source.history
            && remote.source.scope.epoch == source_epoch
    };
    if status.replica_goals.iter().any(|entry| {
        matches!(&entry.goal.desired, ReplicaGoalKind::Remove { remote }
            if matches_target(remote))
    }) || source.remote_operations.iter().any(|entry| {
        matches_target(&entry.selection)
            && matches!(
                entry.state,
                RemoteReplicaState::Dropped | RemoteReplicaState::InDrop
            )
    }) {
        return Err(invalid(
            "retained target tombstone/watermark requires independent receiver settlement and a fresh publication before another Build",
        ));
    }
    status
        .replica_goals
        .iter()
        .filter_map(|entry| match &entry.goal.desired {
            ReplicaGoalKind::Build { remote } | ReplicaGoalKind::Remove { remote }
                if matches_target(remote) =>
            {
                Some(remote.generation)
            }
            _ => None,
        })
        .chain(
            source
                .remote_operations
                .iter()
                .filter(|entry| matches_target(&entry.selection))
                .map(|entry| entry.selection.generation),
        )
        .max()
        .unwrap_or(0)
        .checked_add(1)
        .ok_or_else(|| invalid("remote generation exhausted"))
}

fn predecessor<'a>(
    status: &KubericSetStatus,
    peers: &BTreeMap<ReplicaId, &ReplicaAgentSnapshot>,
    peer: &'a ReplicaAgentSnapshot,
) -> Option<&'a CreationRecoveryDependencies> {
    peer.reconstruction_dependencies
        .iter()
        .filter(|group| {
            group.process_session_id != peer.process_session_id
                && group.creation_id != peer.creation_id
        })
        .filter(|group| {
            peer.goals.iter().any(|observation| {
                observation.goal.scope.process_session_id == group.process_session_id
                    && observation.goal.scope.creation_id == group.creation_id
                    && (matches!(
                        observation.status,
                        ReplicaGoalStatus::Failed(_) | ReplicaGoalStatus::Uncertain(_)
                    ) || status.replica_goals.iter().any(|entry| {
                        entry.goal == observation.goal
                            && entry.outcome.as_ref().is_some_and(|outcome| {
                                matches!(outcome.status, ReplicaGoalStatus::Uncertain(_))
                                    && !outcome.host_settled
                            })
                    }))
                    && !status
                        .replica_goals
                        .iter()
                        .any(|old| old.goal == observation.goal && resolved(status, peers, old))
            })
        })
        .max_by_key(|group| {
            peer.goals
                .iter()
                .filter(|observation| {
                    observation.goal.scope.process_session_id == group.process_session_id
                })
                .map(|observation| observation.goal.revision)
                .max()
                .unwrap_or(0)
        })
}

pub(super) fn required(
    status: &KubericSetStatus,
    peers: &BTreeMap<ReplicaId, &ReplicaAgentSnapshot>,
    peer: &ReplicaAgentSnapshot,
) -> bool {
    predecessor(status, peers, peer).is_some()
}

fn selected_role<'a>(
    peer: &'a ReplicaAgentSnapshot,
    group: &CreationRecoveryDependencies,
) -> Option<&'a ReplicaGoalObservation> {
    let local = peer
        .goals
        .iter()
        .filter(|observation| {
            observation.goal.scope.process_session_id == group.process_session_id
                && observation.goal.scope.creation_id == group.creation_id
        })
        .collect::<Vec<_>>();
    if let Some(selected) = local
        .iter()
        .filter(|observation| role(&observation.goal).is_some())
        .max_by_key(|observation| observation.goal.revision)
    {
        return Some(selected);
    }
    let first = local
        .iter()
        .min_by_key(|observation| observation.goal.revision)?;
    if local.iter().any(|observation| {
        !matches!(observation.goal.desired, ReplicaGoalKind::Discover)
            || observation.goal.scope.configuration.previous.is_some()
            || observation.goal.scope.configuration.current
                != first.goal.scope.configuration.current
    }) {
        return None;
    }
    let selected = peer
        .goals
        .iter()
        .filter(|observation| {
            observation.goal.revision < first.goal.revision
                && observation.goal.scope.history == peer.history
                && observation.goal.scope.storage == peer.storage
                && role(&observation.goal).is_some()
        })
        .max_by_key(|observation| observation.goal.revision)?;
    (role(&selected.goal) == Some(ReplicaRole::ActiveSecondary)
        && selected.status == ReplicaGoalStatus::Achieved
        && selected.host_settled
        && selected.goal.scope.configuration.previous.is_none()
        && selected.goal.scope.configuration.current == first.goal.scope.configuration.current
        && selected.goal.scope.epoch == first.goal.scope.epoch)
        .then_some(selected)
}

impl<A: AgentApi + 'static> KubeClusterApi<A> {
    pub(super) async fn reconcile_original_members(
        &self,
        raw: &RawObservation,
        status: &KubericSetStatus,
        current: &ConfigurationDescriptor,
        peers: &BTreeMap<ReplicaId, &ReplicaAgentSnapshot>,
    ) -> Result<Option<ReconcileAction>> {
        let Some(configured) = status.replica_goals.iter().rev().find(|entry| {
            entry.goal.scope.configuration.previous.as_ref() == Some(current)
                && entry.goal.scope.configuration.current.primary_id != current.primary_id
                && matches!(
                    entry.goal.desired,
                    ReplicaGoalKind::Reconfigure {
                        role: ReplicaRole::Primary,
                        catch_up: None
                    }
                )
        }) else {
            return Ok(None);
        };
        let next = &configured.goal.scope.configuration.current;
        if !achieved(configured) {
            return Ok(None);
        }
        let Some(source) = peers.get(&next.primary_id).copied() else {
            return Ok(Some(action(ReconcileKind::Waiting)));
        };
        let publication = source
            .publication
            .as_ref()
            .filter(|publication| {
                publication.scope == configured.goal.scope
                    && publication.stage == ReplicaPublicationStage::AchievedScope
                    && publication.role == ReplicaRole::Primary
            })
            .or_else(|| {
                status.replica_goals.iter().find_map(|entry| {
                    let ReplicaGoalKind::Prepare {
                        rebuild: Some(rebuild),
                        ..
                    } = &entry.goal.desired
                    else {
                        return None;
                    };
                    (rebuild.source.scope == configured.goal.scope).then_some(&rebuild.source)
                })
            });
        let Some(publication) = publication else {
            return Ok(None);
        };
        if !source.native_lifecycle.as_ref().is_some_and(|facts| {
            source
                .publication
                .as_ref()
                .is_some_and(|actual| facts.scope == actual.scope)
                && facts.state == NativeReplicaState::Ready
                && facts.native_role == ReplicaRole::Primary
                && facts.application_role == Some(ReplicaRole::Primary)
                && facts.applied_primary_fence == Some(next.epoch)
        }) {
            return Ok(Some(action(ReconcileKind::Waiting)));
        }
        for member in next
            .members
            .iter()
            .filter(|member| member.identity.replica_id != next.primary_id)
        {
            let Some(target) = peers.get(&member.identity.replica_id).copied() else {
                return Ok(Some(action(ReconcileKind::Waiting)));
            };
            if target.publication.is_none()
                && target.open_mode == OpenMode::Existing
                && !target.withdrawn
                && status.replica_goals.iter().any(|entry| {
                    matches!(entry.goal.desired, ReplicaGoalKind::Close)
                        && entry.goal.scope.storage == target.storage
                        && entry.goal.scope.process_session_id != target.process_session_id
                        && entry.goal.scope.configuration.current == *next
                        && entry.goal.scope.configuration.previous.as_ref() == Some(current)
                })
                && !status.replica_goals.iter().any(|entry| {
                    entry.goal.scope.storage == target.storage
                        && entry.goal.scope.process_session_id == target.process_session_id
                        && entry.goal.scope.creation_id == target.creation_id
                })
            {
                let mut selected = status.clone();
                selected.replica_goals.push(goal(
                    target,
                    ReplicaConfigurationScope {
                        current: next.clone(),
                        previous: Some(current.clone()),
                        replicas: Vec::new(),
                    },
                    ReplicaGoalKind::Discover,
                    &selected.replica_goals,
                )?);
                return self.persist_goals(raw, selected).await.map(Some);
            }
            let source_range = status
                .replica_goals
                .iter()
                .filter(|entry| {
                    entry.goal.scope.storage == source.storage
                        && entry.goal.scope.process_session_id == source.process_session_id
                        && entry.goal.scope.configuration.current == *next
                        && entry.goal.scope.configuration.previous.as_ref() == Some(current)
                        && matches!(entry.goal.desired, ReplicaGoalKind::Discover)
                        && achieved(entry)
                })
                .filter_map(|entry| entry.native_status().ok().flatten())
                .max_by_key(|native| native.input.revision);
            let target_range = status
                .replica_goals
                .iter()
                .filter(|entry| {
                    entry.goal.scope.storage == target.storage
                        && entry.goal.scope.process_session_id == target.process_session_id
                        && entry.goal.scope.configuration.current == *next
                        && entry.goal.scope.configuration.previous.as_ref() == Some(current)
                        && matches!(entry.goal.desired, ReplicaGoalKind::Discover)
                        && achieved(entry)
                })
                .filter_map(|entry| entry.native_status().ok().flatten())
                .max_by_key(|native| native.input.revision);
            if let (Some(source_range), Some(target_range)) = (&source_range, &target_range)
                && target_range.input.state == NativeReplicaState::Ready
                && let (Some(last), Some(first), Some(target_last)) = (
                    source_range.progress.value(),
                    source_range.capability.value(),
                    target_range.progress.value(),
                )
                && target_last < last
                && (first == 0 || first > target_last.saturating_add(1))
            {
                if source_range.input.native_role != ReplicaRole::Primary {
                    if !status.replica_goals.iter().any(|entry| {
                        entry.goal.scope.storage == publication.scope.storage
                            && entry.goal.scope.process_session_id
                                == publication.scope.process_session_id
                            && entry.goal.scope.creation_id == publication.scope.creation_id
                            && entry.goal.scope.configuration.current
                                == publication.scope.configuration.current
                            && entry.goal.scope.configuration.previous
                                == publication.scope.configuration.previous
                            && entry.goal.revision > configured.goal.revision
                            && matches!(entry.goal.desired, ReplicaGoalKind::Discover)
                    }) {
                        let mut selected = status.clone();
                        let mut inspection = publication.scope.configuration.clone();
                        inspection.replicas.clear();
                        selected.replica_goals.push(goal(
                            source,
                            inspection,
                            ReplicaGoalKind::Discover,
                            &selected.replica_goals,
                        )?);
                        return self.persist_goals(raw, selected).await.map(Some);
                    }
                    return Ok(None);
                }
                if !status.replica_goals.iter().any(|entry| {
                    entry.goal.scope.storage == target.storage
                        && entry.goal.scope.process_session_id == target.process_session_id
                        && entry.goal.scope.configuration.current == *next
                        && matches!(entry.goal.desired, ReplicaGoalKind::Close)
                }) {
                    let mut selected = status.clone();
                    selected.replica_goals.push(goal(
                        target,
                        configuration_for(member.identity.replica_id, next, Some(current), peers)?,
                        ReplicaGoalKind::Close,
                        &selected.replica_goals,
                    )?);
                    return self.persist_goals(raw, selected).await.map(Some);
                }
                return Ok(None);
            }
            let Some(inspection) = status.replica_goals.iter().rev().find(|entry| {
                entry.goal.scope.storage == target.storage
                    && entry.goal.scope.process_session_id == target.process_session_id
                    && entry.goal.scope.creation_id == target.creation_id
                    && entry.goal.scope.configuration.current == *next
                    && entry.goal.scope.configuration.previous.as_ref() == Some(current)
                    && entry.native_status().ok().flatten().is_some_and(|native| {
                        native.input.state == NativeReplicaState::StandBy
                            && native.input.deactivation.is_some()
                    })
                    && matches!(entry.goal.desired, ReplicaGoalKind::Discover)
                    && achieved(entry)
            }) else {
                if target_range.is_none() {
                    // A selected fresh-owner query must finish before this
                    // round can choose copy or genuine warm continuation.
                    return Ok(None);
                }
                continue;
            };
            let prepared = status.replica_goals.iter().find(|entry| {
                matches!(&entry.goal.desired, ReplicaGoalKind::Prepare {
                    rebuild: Some(rebuild), ..
                } if rebuild.inspection.as_ref() == &inspection.goal
                    && rebuild.source == *publication)
            });
            let Some(prepared) = prepared else {
                let mut selected = status.clone();
                selected.replica_goals.push(goal(
                    target,
                    configuration_for(member.identity.replica_id, next, Some(current), peers)?,
                    ReplicaGoalKind::Prepare {
                        role: ReplicaRole::IdleSecondary,
                        rebuild: Some(Box::new(OriginalMemberRebuild {
                            inspection: Box::new(inspection.goal.clone()),
                            source: publication.clone(),
                        })),
                    },
                    &selected.replica_goals,
                )?);
                return self.persist_goals(raw, selected).await.map(Some);
            };
            if !achieved(prepared) {
                return Ok(None);
            }
            let build = status.replica_goals.iter().find(|entry| {
                matches!(&entry.goal.desired, ReplicaGoalKind::Build { remote }
                    if remote.source == *publication
                        && remote.original_member.as_deref() == Some(&prepared.goal))
            });
            let idle = target.publication.as_ref().filter(|idle| {
                idle.scope == prepared.goal.scope
                    && idle.stage == ReplicaPublicationStage::AchievedScope
                    && idle.role == ReplicaRole::IdleSecondary
            });
            if build.is_none() {
                let Some(idle) = idle else {
                    return Ok(None);
                };
                let mut selected = status.clone();
                let remote = RemoteReplicaSelection {
                    source: publication.clone(),
                    target: idle.clone(),
                    target_control_address: peer_address(raw, &target.storage.replica)?,
                    generation: next_generation(status, source, idle)?,
                    original_member: Some(Box::new(prepared.goal.clone())),
                };
                selected.replica_goals.push(goal(
                    source,
                    publication.scope.configuration.clone(),
                    ReplicaGoalKind::Build {
                        remote: Box::new(remote),
                    },
                    &selected.replica_goals,
                )?);
                return self.persist_goals(raw, selected).await.map(Some);
            }
            if !achieved(build.unwrap()) {
                return Ok(None);
            }
            let active = status.replica_goals.iter().find(|entry| {
                entry.goal.scope.storage == target.storage
                    && entry.goal.scope.process_session_id == target.process_session_id
                    && entry.goal.scope.creation_id == target.creation_id
                    && entry.goal.scope.configuration.current == *next
                    && entry.goal.scope.configuration.previous.as_ref() == Some(current)
                    && matches!(
                        entry.goal.desired,
                        ReplicaGoalKind::Reconfigure {
                            role: ReplicaRole::ActiveSecondary,
                            catch_up: None
                        }
                    )
            });
            let Some(active) = active else {
                let mut selected = status.clone();
                selected.replica_goals.push(goal(
                    target,
                    configuration_for(member.identity.replica_id, next, Some(current), peers)?,
                    ReplicaGoalKind::Reconfigure {
                        role: ReplicaRole::ActiveSecondary,
                        catch_up: None,
                    },
                    &selected.replica_goals,
                )?);
                return self.persist_goals(raw, selected).await.map(Some);
            };
            if !achieved(active) {
                return Ok(None);
            }
        }
        for previous in [Some(current)] {
            let selected = status.replica_goals.iter().find(|entry| {
                entry.goal.scope.storage == source.storage
                    && entry.goal.scope.process_session_id == source.process_session_id
                    && entry.goal.scope.creation_id == source.creation_id
                    && entry.goal.scope.configuration.current == *next
                    && entry.goal.scope.configuration.previous.as_ref() == previous
                    && matches!(
                        entry.goal.desired,
                        ReplicaGoalKind::Reconfigure {
                            role: ReplicaRole::Primary,
                            catch_up: Some(PublicCatchUpMode::WriteQuorum)
                        }
                    )
            });
            let Some(selected) = selected else {
                let mut selection = status.clone();
                selection.replica_goals.push(goal(
                    source,
                    configuration_for(next.primary_id, next, previous, peers)?,
                    ReplicaGoalKind::Reconfigure {
                        role: ReplicaRole::Primary,
                        catch_up: Some(PublicCatchUpMode::WriteQuorum),
                    },
                    &selection.replica_goals,
                )?);
                return self.persist_goals(raw, selection).await.map(Some);
            };
            if !achieved(selected) {
                return Ok(None);
            }
        }
        for member in next
            .members
            .iter()
            .filter(|member| member.identity.replica_id != next.primary_id)
        {
            let target = peers[&member.identity.replica_id];
            let selected = status.replica_goals.iter().find(|entry| {
                entry.goal.scope.storage == target.storage
                    && entry.goal.scope.process_session_id == target.process_session_id
                    && entry.goal.scope.creation_id == target.creation_id
                    && entry.goal.scope.configuration.current == *next
                    && entry.goal.scope.configuration.previous.is_none()
                    && matches!(
                        entry.goal.desired,
                        ReplicaGoalKind::Reconfigure {
                            role: ReplicaRole::ActiveSecondary,
                            catch_up: None
                        }
                    )
            });
            let Some(selected) = selected else {
                let mut selection = status.clone();
                selection.replica_goals.push(goal(
                    target,
                    configuration_for(member.identity.replica_id, next, None, peers)?,
                    ReplicaGoalKind::Reconfigure {
                        role: ReplicaRole::ActiveSecondary,
                        catch_up: None,
                    },
                    &selection.replica_goals,
                )?);
                return self.persist_goals(raw, selection).await.map(Some);
            };
            if !achieved(selected) {
                return Ok(None);
            }
        }
        let final_source = status.replica_goals.iter().find(|entry| {
            entry.goal.scope.storage == source.storage
                && entry.goal.scope.process_session_id == source.process_session_id
                && entry.goal.scope.creation_id == source.creation_id
                && entry.goal.scope.configuration.current == *next
                && entry.goal.scope.configuration.previous.is_none()
                && matches!(
                    entry.goal.desired,
                    ReplicaGoalKind::Reconfigure {
                        role: ReplicaRole::Primary,
                        catch_up: Some(PublicCatchUpMode::WriteQuorum)
                    }
                )
        });
        let Some(final_source) = final_source else {
            let mut selection = status.clone();
            selection.replica_goals.push(goal(
                source,
                configuration_for(next.primary_id, next, None, peers)?,
                ReplicaGoalKind::Reconfigure {
                    role: ReplicaRole::Primary,
                    catch_up: Some(PublicCatchUpMode::WriteQuorum),
                },
                &selection.replica_goals,
            )?);
            return self.persist_goals(raw, selection).await.map(Some);
        };
        if !achieved(final_source) {
            return Ok(None);
        }
        if next.members.iter().any(|member| {
            !peers.get(&member.identity.replica_id).is_some_and(|peer| {
                peer.publication.as_ref().is_some_and(|publication| {
                    publication.stage == ReplicaPublicationStage::AchievedScope
                        && publication.role == member.role
                        && publication.scope.configuration.current == *next
                        && publication.scope.configuration.previous.is_none()
                })
            })
        }) {
            return Ok(None);
        }
        for member in next
            .members
            .iter()
            .filter(|member| member.identity.replica_id != next.primary_id)
        {
            let peer = peers[&member.identity.replica_id];
            let final_goal = status
                .replica_goals
                .iter()
                .find(|entry| {
                    entry.goal.scope.storage == peer.storage
                        && entry.goal.scope.process_session_id == peer.process_session_id
                        && entry.goal.scope.configuration.current == *next
                        && entry.goal.scope.configuration.previous.is_none()
                        && matches!(
                            entry.goal.desired,
                            ReplicaGoalKind::Reconfigure {
                                role: ReplicaRole::ActiveSecondary,
                                catch_up: None
                            }
                        )
                })
                .ok_or_else(|| invalid("original-member final target disappeared"))?;
            self.send_goal(raw, &final_goal.goal, true).await?;
        }
        let sessions = next
            .members
            .iter()
            .map(|member| endpoint(peers[&member.identity.replica_id], member.role))
            .collect::<Vec<_>>();
        let mut topology =
            PublicV1TopologyState::from_configuration(next, &sessions).map_err(invalid)?;
        topology.completed_switchover_request_id = status
            .public_v1_topology
            .as_ref()
            .unwrap()
            .completed_switchover_request_id
            .clone();
        topology.pending_switchover = status
            .public_v1_topology
            .as_ref()
            .unwrap()
            .pending_switchover
            .clone();
        let mut selection = status.clone();
        selection.public_v1_topology = Some(topology);
        return self.persist_goals(raw, selection).await.map(Some);
    }

    async fn reform_reconstructed_members(
        &self,
        raw: &RawObservation,
        status: &KubericSetStatus,
        current: &ConfigurationDescriptor,
        peers: &BTreeMap<ReplicaId, &ReplicaAgentSnapshot>,
    ) -> Result<Option<ReconcileAction>> {
        let Some(topology) = status.public_v1_topology.as_ref() else {
            return Ok(None);
        };
        let Some(primary) = peers.get(&current.primary_id).copied() else {
            return Ok(None);
        };
        if !topology
            .sessions
            .contains(&endpoint(primary, ReplicaRole::Primary))
        {
            return Ok(None);
        }
        let next = ConfigurationDescriptor::new(
            Epoch::new(
                current.epoch.data_loss_number,
                current
                    .epoch
                    .configuration_number
                    .checked_add(1)
                    .ok_or_else(|| invalid("retained member reformation epoch exhausted"))?,
            ),
            current.primary_id,
            current.members.clone(),
            current.write_quorum,
        );
        let selected = status.replica_goals.iter().rev().find(|entry| {
            entry.goal.scope.storage == primary.storage
                && entry.goal.scope.process_session_id == primary.process_session_id
                && entry.goal.scope.creation_id == primary.creation_id
                && entry.goal.scope.configuration.current == next
                && entry.goal.scope.configuration.previous.as_ref() == Some(current)
                && matches!(
                    entry.goal.desired,
                    ReplicaGoalKind::Reconfigure {
                        role: ReplicaRole::Primary,
                        catch_up: Some(PublicCatchUpMode::All),
                    }
                )
        });
        if selected.is_none() {
            let mut sampled = BTreeMap::new();
            for member in &current.members {
                let Some(peer) = peers.get(&member.identity.replica_id).copied() else {
                    return Ok(None);
                };
                if peer.withdrawn
                    || peer.storage.replica != member.identity
                    || !peer.publication.as_ref().is_some_and(|publication| {
                        publication.stage == ReplicaPublicationStage::AchievedScope
                            && publication.role == member.role
                            && publication.scope.configuration.current == *current
                            && publication.scope.configuration.previous.is_none()
                    })
                {
                    return Ok(None);
                }
                let actual = self.sample_recovery(raw, peer).await?;
                if actual.progress.value().is_none_or(|value| value < 0)
                    || actual.capability.value().is_none_or(|value| value < 0)
                {
                    #[cfg(feature = "test-util")]
                    if std::env::var_os("KUBERIC_M3_RECONSTRUCTION_DIAGNOSTICS").is_some() {
                        static REPORTED: std::sync::atomic::AtomicBool =
                            std::sync::atomic::AtomicBool::new(false);
                        if !REPORTED.swap(true, std::sync::atomic::Ordering::AcqRel) {
                            eprintln!(
                                "retained member fresh native sample unavailable: member={}, progress={:?}, capability={:?}, publication={:?}",
                                member.identity.replica_id,
                                actual.progress,
                                actual.capability,
                                actual.publication.as_ref().map(|publication| (
                                    publication.role,
                                    publication.stage,
                                    publication.scope.epoch
                                ))
                            );
                        }
                    }
                    return Ok(None);
                }
                sampled.insert(member.identity.replica_id, actual);
            }
            let sampled = sampled
                .iter()
                .map(|(id, peer)| (*id, peer))
                .collect::<BTreeMap<_, _>>();
            let mut next_status = status.clone();
            next_status.replica_goals.push(goal(
                primary,
                configuration_for(current.primary_id, &next, Some(current), &sampled)?,
                ReplicaGoalKind::Reconfigure {
                    role: ReplicaRole::Primary,
                    catch_up: Some(PublicCatchUpMode::All),
                },
                &next_status.replica_goals,
            )?);
            return self.persist_goals(raw, next_status).await.map(Some);
        }
        let selected = selected.unwrap();
        if !achieved(selected)
            || selected
                .outcome
                .as_ref()
                .is_none_or(|outcome| !outcome.host_settled)
        {
            return Ok(None);
        }
        let current_only = status.replica_goals.iter().rev().find(|entry| {
            entry.goal.scope.storage == primary.storage
                && entry.goal.scope.process_session_id == primary.process_session_id
                && entry.goal.scope.creation_id == primary.creation_id
                && entry.goal.scope.configuration.current == next
                && entry.goal.scope.configuration.previous.is_none()
                && matches!(
                    entry.goal.desired,
                    ReplicaGoalKind::Reconfigure {
                        role: ReplicaRole::Primary,
                        catch_up: Some(PublicCatchUpMode::All),
                    }
                )
        });
        if current_only.is_none() {
            let mut next_status = status.clone();
            next_status.replica_goals.push(goal(
                primary,
                configuration_for(current.primary_id, &next, None, peers)?,
                ReplicaGoalKind::Reconfigure {
                    role: ReplicaRole::Primary,
                    catch_up: Some(PublicCatchUpMode::All),
                },
                &next_status.replica_goals,
            )?);
            return self.persist_goals(raw, next_status).await.map(Some);
        }
        let current_only = current_only.unwrap();
        if !achieved(current_only)
            || current_only
                .outcome
                .as_ref()
                .is_none_or(|outcome| !outcome.host_settled)
        {
            return Ok(None);
        }
        for member in current
            .members
            .iter()
            .filter(|member| member.identity.replica_id != current.primary_id)
        {
            let Some(peer) = peers
                .get(&member.identity.replica_id)
                .copied()
                .filter(|peer| peer.storage.replica == member.identity)
            else {
                return Ok(None);
            };
            if !status.replica_goals.iter().any(|entry| {
                entry.goal.scope.storage == peer.storage
                    && entry.goal.scope.process_session_id == peer.process_session_id
                    && entry.goal.scope.creation_id == peer.creation_id
                    && entry.goal.scope.configuration.current == next
                    && entry.goal.scope.configuration.previous.is_none()
                    && matches!(
                        entry.goal.desired,
                        ReplicaGoalKind::Reconfigure {
                            role: ReplicaRole::ActiveSecondary,
                            catch_up: None,
                        }
                    )
            }) {
                let mut next_status = status.clone();
                next_status.replica_goals.push(goal(
                    peer,
                    configuration_for(member.identity.replica_id, &next, None, peers)?,
                    ReplicaGoalKind::Reconfigure {
                        role: ReplicaRole::ActiveSecondary,
                        catch_up: None,
                    },
                    &next_status.replica_goals,
                )?);
                return self.persist_goals(raw, next_status).await.map(Some);
            }
        }
        if next.members.iter().any(|member| {
            !peers.get(&member.identity.replica_id).is_some_and(|peer| {
                peer.publication.as_ref().is_some_and(|publication| {
                    publication.stage == ReplicaPublicationStage::AchievedScope
                        && publication.role == member.role
                        && publication.scope.configuration.current == next
                        && publication.scope.configuration.previous.is_none()
                })
            })
        }) {
            return Ok(None);
        }
        let sessions = next
            .members
            .iter()
            .map(|member| endpoint(peers[&member.identity.replica_id], member.role))
            .collect::<Vec<_>>();
        let mut next_topology =
            PublicV1TopologyState::from_configuration(&next, &sessions).map_err(invalid)?;
        next_topology.completed_switchover_request_id =
            topology.completed_switchover_request_id.clone();
        next_topology.pending_switchover = topology.pending_switchover.clone();
        let mut next_status = status.clone();
        next_status.public_v1_topology = Some(next_topology);
        self.persist_goals(raw, next_status).await.map(Some)
    }

    pub(super) async fn reconcile_contained_creation(
        &self,
        raw: &RawObservation,
        status: &KubericSetStatus,
        peers: &BTreeMap<ReplicaId, &ReplicaAgentSnapshot>,
    ) -> Result<Option<ReconcileAction>> {
        for peer in peers.values().copied() {
            if status.public_v1_topology.as_ref().is_some_and(|topology| {
                topology.unavailable_primary_id == Some(topology.primary_id)
                    && topology.configuration().is_ok_and(|current| {
                        current
                            .members
                            .iter()
                            .any(|member| member.identity == peer.storage.replica)
                    })
            }) {
                continue;
            }
            if let Some(selected) = status.replica_goals.iter().rev().find(|entry| {
                entry.goal.scope.storage == peer.storage
                    && entry.goal.scope.process_session_id == peer.process_session_id
                    && entry.goal.scope.creation_id == peer.creation_id
                    && matches!(
                        entry.goal.desired,
                        ReplicaGoalKind::RecoverContainedCreation { .. }
                    )
            }) {
                if matches!(&selected.goal.desired, ReplicaGoalKind::RecoverContainedCreation { recovery }
                    if recovery.addition.is_some())
                {
                    continue;
                }
                if achieved(selected)
                    && selected
                        .outcome
                        .as_ref()
                        .is_some_and(|outcome| outcome.host_settled)
                {
                    let mut next = status.clone();
                    if let Some(topology) = next.public_v1_topology.as_mut()
                        && topology.configuration().map_err(invalid)?
                            == selected.goal.scope.configuration.current
                        && selected.goal.scope.configuration.previous.is_none()
                        && let Some(member) = selected
                            .goal
                            .scope
                            .configuration
                            .current
                            .members
                            .iter()
                            .find(|member| member.identity == peer.storage.replica)
                    {
                        let fresh = endpoint(peer, member.role);
                        if !topology.sessions.contains(&fresh) {
                            if member.role == ReplicaRole::ActiveSecondary {
                                let current = topology.configuration().map_err(invalid)?;
                                if let Some(action) = self
                                    .reform_reconstructed_members(raw, status, &current, peers)
                                    .await?
                                {
                                    return Ok(Some(action));
                                }
                                continue;
                            }
                            let Some(old) = topology.sessions.iter_mut().find(|old| {
                                old.identity == fresh.identity
                                    && old.storage_incarnation == fresh.storage_incarnation
                            }) else {
                                return Err(invalid(
                                    "reconstruction lost exact retained topology storage",
                                ));
                            };
                            *old = fresh;
                            return self.persist_goals(raw, next).await.map(Some);
                        }
                    }
                }
                continue;
            }
            let Some(group) = predecessor(status, peers, peer) else {
                continue;
            };
            if peer.open_mode != OpenMode::Existing
                || peer.publication.is_some()
                || peer.provider_writer_held
                || peer.withdrawn
                || !retained_storage_present(raw, peer)
                || peer.goals.iter().any(|observation| {
                    observation.goal.scope.process_session_id == peer.process_session_id
                })
                || status.replica_goals.iter().any(|entry| {
                    entry.goal.scope.storage == peer.storage
                        && entry.goal.scope.process_session_id == peer.process_session_id
                })
            {
                continue;
            }
            let old = peer
                .goals
                .iter()
                .filter(|observation| {
                    observation.goal.scope.process_session_id == group.process_session_id
                        && observation.goal.scope.creation_id == group.creation_id
                })
                .max_by_key(|observation| observation.goal.revision);
            let selected = selected_role(peer, group);
            let (Some(old), Some(selected)) = (old, selected) else {
                continue;
            };
            if old.goal.scope.epoch != selected.goal.scope.epoch {
                return Err(invalid(
                    "reconstruction requires ordered frozen epoch/configuration recovery",
                ));
            }
            let mut receiver_settlements = Vec::new();
            let mut waiting = false;
            for build in peer.goals.iter().filter(|observation| {
                observation.goal.scope.process_session_id == group.process_session_id
                    && matches!(observation.goal.desired, ReplicaGoalKind::Build { .. })
            }) {
                let ReplicaGoalKind::Build { remote } = &build.goal.desired else {
                    unreachable!()
                };
                let Some(receiver) = peers
                    .get(&remote.target.scope.storage.replica.replica_id)
                    .filter(|receiver| {
                        receiver.history == remote.target.scope.history
                            && receiver.storage == remote.target.scope.storage
                    })
                else {
                    waiting = true;
                    continue;
                };
                let terminal = status.replica_goals.iter().rev().find(|entry| {
                    entry.goal.scope.history == receiver.history
                        && entry.goal.scope.storage == receiver.storage
                        && entry.goal.scope.process_session_id
                            == remote.target.scope.process_session_id
                        && entry.goal.scope.creation_id == remote.target.scope.creation_id
                        && matches!(
                            entry.goal.desired,
                            ReplicaGoalKind::Close | ReplicaGoalKind::Abort
                        )
                });
                let Some(terminal) = terminal else {
                    if receiver.process_session_id != remote.target.scope.process_session_id
                        || receiver.creation_id != remote.target.scope.creation_id
                    {
                        waiting = true;
                        continue;
                    }
                    let configuration = receiver
                        .publication
                        .as_ref()
                        .map(|publication| publication.scope.configuration.clone())
                        .or_else(|| {
                            receiver
                                .goals
                                .iter()
                                .max_by_key(|entry| entry.goal.revision)
                                .map(|entry| entry.goal.scope.configuration.clone())
                        })
                        .ok_or_else(|| {
                            invalid("receiver has no exact scope for independent closure")
                        })?;
                    let mut next = status.clone();
                    next.replica_goals.push(goal(
                        receiver,
                        configuration,
                        ReplicaGoalKind::Close,
                        &status.replica_goals,
                    )?);
                    return self.persist_goals(raw, next).await.map(Some);
                };
                let same_receiver = receiver.process_session_id
                    == remote.target.scope.process_session_id
                    && receiver.creation_id == remote.target.scope.creation_id;
                if !achieved(terminal)
                    || !terminal
                        .outcome
                        .as_ref()
                        .is_some_and(|outcome| outcome.host_settled)
                    || !receiver.goals.iter().any(|observation| {
                        observation.goal == terminal.goal
                            && observation.status == ReplicaGoalStatus::Achieved
                            && observation.host_settled
                    })
                    || if same_receiver {
                        !receiver.withdrawn
                    } else {
                        receiver.process_session_id == remote.target.scope.process_session_id
                            || receiver.creation_id == remote.target.scope.creation_id
                            || receiver.goals.iter().any(|entry| {
                                entry.goal.scope.process_session_id == receiver.process_session_id
                            })
                    }
                    || receiver.publication.is_some()
                    || receiver.provider_writer_held
                    || receiver.read_status == AccessStatus::Granted
                    || receiver.write_status == AccessStatus::Granted
                {
                    waiting = true;
                    continue;
                }
                receiver_settlements.push(ContainedReceiverSettlement {
                    source_operation_id: build.goal.operation_id.clone(),
                    terminal_goal: Box::new(terminal.goal.clone()),
                });
            }
            if waiting {
                continue;
            }
            if let Some(previous) = selected.goal.scope.configuration.previous.as_ref()
                && previous.members.len() == 1
                && previous.primary_id == peer.storage.replica.replica_id
                && selected.goal.scope.configuration.current.primary_id == previous.primary_id
                && selected.goal.scope.configuration.current.members.len() == 2
                && status.public_v1_topology.as_ref().is_some_and(|topology| {
                    topology.configuration().ok().as_ref() == Some(previous)
                        && topology.pending_switchover.is_none()
                })
                && let Some(admitted) = peer
                    .goals
                    .iter()
                    .filter(|entry| {
                        entry.goal.scope.process_session_id == group.process_session_id
                            && entry.goal.scope.creation_id == group.creation_id
                            && entry.goal.scope.configuration.current == *previous
                            && entry.goal.scope.configuration.previous.is_none()
                            && role(&entry.goal) == Some(ReplicaRole::Primary)
                            && entry.status == ReplicaGoalStatus::Achieved
                            && entry.host_settled
                    })
                    .max_by_key(|entry| entry.goal.revision)
                && let Some(member) = selected
                    .goal
                    .scope
                    .configuration
                    .current
                    .members
                    .iter()
                    .find(|member| member.identity != peer.storage.replica)
                && let Some(receiver) = peers
                    .get(&member.identity.replica_id)
                    .copied()
                    .filter(|receiver| receiver.storage.replica == member.identity)
            {
                let maximum = peer
                    .goals
                    .iter()
                    .map(|entry| entry.goal.scope.epoch)
                    .chain(
                        receiver_settlements
                            .iter()
                            .map(|settled| settled.terminal_goal.scope.epoch),
                    )
                    .chain(
                        status
                            .public_v1_topology
                            .iter()
                            .map(|topology| topology.epoch),
                    )
                    .max()
                    .unwrap();
                let addition = MembershipAdditionRecovery {
                    phase: MembershipAdditionPhase::Bootstrap,
                    admitted: Box::new(admitted.goal.clone()),
                    interrupted: Box::new(selected.goal.clone()),
                    source: endpoint(peer, ReplicaRole::Primary),
                    receiver: endpoint(receiver, ReplicaRole::IdleSecondary),
                    bootstrap_epoch: Epoch::new(
                        maximum.data_loss_number,
                        maximum
                            .configuration_number
                            .checked_add(1)
                            .ok_or_else(|| invalid("addition bootstrap fence exhausted"))?,
                    ),
                };
                let mut next = status.clone();
                next.replica_goals.push(goal(
                    peer,
                    addition
                        .configuration(MembershipAdditionPhase::Bootstrap)
                        .map_err(invalid)?,
                    ReplicaGoalKind::RecoverContainedCreation {
                        recovery: Box::new(ContainedCreationRecovery {
                            predecessor_process_session_id: group.process_session_id.clone(),
                            predecessor_creation_id: group.creation_id.clone(),
                            predecessor_operations: group.operations.clone(),
                            receiver_settlements,
                            role: ReplicaRole::Primary,
                            catch_up: Some(PublicCatchUpMode::All),
                            addition: Some(Box::new(addition)),
                        }),
                    },
                    &status.replica_goals,
                )?);
                return self.persist_goals(raw, next).await.map(Some);
            }
            let mut configuration = selected.goal.scope.configuration.clone();
            for description in &mut configuration.replicas {
                description.current_progress = INVALID_LSN;
                description.catch_up_capability = INVALID_LSN;
                description.is_up = false;
                description.is_ready = false;
                description.must_catchup = false;
            }
            let restored_role = role(&selected.goal).unwrap();
            let catch_up = if restored_role == ReplicaRole::Primary {
                Some(match &selected.goal.desired {
                    ReplicaGoalKind::Reconfigure {
                        catch_up: Some(mode),
                        ..
                    }
                    | ReplicaGoalKind::RefreshPrimary {
                        catch_up: Some(mode),
                    } => *mode,
                    ReplicaGoalKind::RecoverContainedCreation { recovery } => {
                        recovery.catch_up.unwrap()
                    }
                    _ => PublicCatchUpMode::WriteQuorum,
                })
            } else {
                None
            };
            let mut next = status.clone();
            next.replica_goals.push(goal(
                peer,
                configuration,
                ReplicaGoalKind::RecoverContainedCreation {
                    recovery: Box::new(ContainedCreationRecovery {
                        addition: None,
                        predecessor_process_session_id: group.process_session_id.clone(),
                        predecessor_creation_id: group.creation_id.clone(),
                        predecessor_operations: group.operations.clone(),
                        receiver_settlements,
                        role: restored_role,
                        catch_up,
                    }),
                },
                &status.replica_goals,
            )?);
            return self.persist_goals(raw, next).await.map(Some);
        }
        Ok(None)
    }

    pub(super) async fn reconcile_ordered_addition(
        &self,
        raw: &RawObservation,
        status: &KubericSetStatus,
        peers: &BTreeMap<ReplicaId, &ReplicaAgentSnapshot>,
    ) -> Result<Option<ReconcileAction>> {
        use MembershipAdditionPhase as Phase;
        let Some((bootstrap, recovery, selected)) =
            status.replica_goals.iter().rev().find_map(|entry| {
                let ReplicaGoalKind::RecoverContainedCreation { recovery } = &entry.goal.desired
                else {
                    return None;
                };
                let selected = recovery.addition.as_deref()?;
                (selected.phase == Phase::Bootstrap).then_some((entry, recovery, selected))
            })
        else {
            return Ok(None);
        };
        if status.public_v1_topology.as_ref().is_some_and(|topology| {
            selected
                .configuration(Phase::Current)
                .is_ok_and(|projection| topology.epoch >= projection.current.epoch)
        }) {
            return Ok(None);
        }
        let Some(source) = peers
            .get(&selected.source.identity.replica_id)
            .copied()
            .filter(|peer| endpoint(peer, ReplicaRole::Primary) == selected.source)
        else {
            return Ok(None);
        };
        let Some(receiver) = peers
            .get(&selected.receiver.identity.replica_id)
            .copied()
            .filter(|peer| endpoint(peer, ReplicaRole::IdleSecondary) == selected.receiver)
        else {
            return Ok(None);
        };
        let settled = |entry: &RetainedReplicaGoal| {
            achieved(entry)
                && entry
                    .outcome
                    .as_ref()
                    .is_some_and(|outcome| outcome.host_settled)
        };
        if !settled(bootstrap) {
            return Ok(None);
        }
        let find_phase = |phase| {
            status.replica_goals.iter().find(|entry| {
                matches!(&entry.goal.desired, ReplicaGoalKind::RecoverContainedCreation { recovery }
                if recovery.addition.as_ref().is_some_and(|other| {
                    let mut expected = selected.clone();
                    expected.phase = phase;
                    other.as_ref() == &expected
                }))
            })
        };
        let mut next = status.clone();
        let mut select_phase = |peer: &ReplicaAgentSnapshot,
                                phase: Phase,
                                configuration: ReplicaConfigurationScope|
         -> Result<()> {
            let mut recovery = recovery.as_ref().clone();
            recovery.addition.as_mut().unwrap().phase = phase;
            recovery.role = if phase == Phase::Receiver {
                ReplicaRole::IdleSecondary
            } else {
                ReplicaRole::Primary
            };
            recovery.catch_up = (phase != Phase::Receiver).then_some(PublicCatchUpMode::All);
            next.replica_goals.push(goal(
                peer,
                configuration,
                ReplicaGoalKind::RecoverContainedCreation {
                    recovery: Box::new(recovery),
                },
                &status.replica_goals,
            )?);
            Ok(())
        };
        let Some(idle) = find_phase(Phase::Receiver) else {
            select_phase(
                receiver,
                Phase::Receiver,
                selected.configuration(Phase::Receiver).map_err(invalid)?,
            )?;
            return self.persist_goals(raw, next).await.map(Some);
        };
        if !settled(idle) {
            return Ok(None);
        }
        let build = status.replica_goals.iter().find(|entry| {
            matches!(&entry.goal.desired, ReplicaGoalKind::Build { remote }
                if remote.source.scope == bootstrap.goal.scope
                    && remote.target.scope == idle.goal.scope)
        });
        let Some(build) = build else {
            let source_publication = source
                .publication
                .as_ref()
                .filter(|publication| {
                    publication.stage == ReplicaPublicationStage::AchievedScope
                        && publication.scope == bootstrap.goal.scope
                })
                .ok_or_else(|| invalid("addition lost actual achieved P0 before fresh Build"))?;
            let target_publication = receiver
                .publication
                .as_ref()
                .filter(|publication| {
                    publication.stage == ReplicaPublicationStage::AchievedScope
                        && publication.role == ReplicaRole::IdleSecondary
                        && publication.scope == idle.goal.scope
                })
                .ok_or_else(|| invalid("addition lost actual achieved idle preparation"))?;
            let remote = RemoteReplicaSelection {
                original_member: None,
                source: source_publication.clone(),
                target: target_publication.clone(),
                target_control_address: peer_address(raw, &receiver.storage.replica)?,
                generation: next_generation(status, source, target_publication)?,
            };
            remote.validate().map_err(invalid)?;
            next.replica_goals.push(goal(
                source,
                bootstrap.goal.scope.configuration.clone(),
                ReplicaGoalKind::Build {
                    remote: Box::new(remote),
                },
                &status.replica_goals,
            )?);
            return self.persist_goals(raw, next).await.map(Some);
        };
        if !settled(build) {
            return Ok(None);
        }
        let Some(joint) = find_phase(Phase::Joint) else {
            let projection = selected.configuration(Phase::Joint).map_err(invalid)?;
            let configuration = configuration_for(
                source.storage.replica.replica_id,
                &projection.current,
                projection.previous.as_ref(),
                peers,
            )?;
            select_phase(source, Phase::Joint, configuration)?;
            return self.persist_goals(raw, next).await.map(Some);
        };
        if !source.goals.iter().any(|entry| entry.goal == joint.goal) {
            return Ok(None);
        }
        let receiver_joint = status.replica_goals.iter().find(|entry| {
            entry.goal.scope.storage == receiver.storage
                && entry.goal.scope.process_session_id == receiver.process_session_id
                && entry.goal.scope.configuration.current == joint.goal.scope.configuration.current
                && entry.goal.scope.configuration.previous
                    == joint.goal.scope.configuration.previous
                && matches!(
                    entry.goal.desired,
                    ReplicaGoalKind::Reconfigure {
                        role: ReplicaRole::ActiveSecondary,
                        catch_up: None
                    }
                )
        });
        let Some(receiver_joint) = receiver_joint else {
            next.replica_goals.push(goal(
                receiver,
                configuration_for(
                    receiver.storage.replica.replica_id,
                    &joint.goal.scope.configuration.current,
                    joint.goal.scope.configuration.previous.as_ref(),
                    peers,
                )?,
                ReplicaGoalKind::Reconfigure {
                    role: ReplicaRole::ActiveSecondary,
                    catch_up: None,
                },
                &status.replica_goals,
            )?);
            return self.persist_goals(raw, next).await.map(Some);
        };
        if !settled(receiver_joint) {
            return Ok(None);
        }
        if !settled(joint) {
            return Ok(None);
        }
        let Some(current) = find_phase(Phase::Current) else {
            let projection = selected.configuration(Phase::Current).map_err(invalid)?;
            select_phase(
                source,
                Phase::Current,
                configuration_for(
                    source.storage.replica.replica_id,
                    &projection.current,
                    None,
                    peers,
                )?,
            )?;
            return self.persist_goals(raw, next).await.map(Some);
        };
        if !source.goals.iter().any(|entry| entry.goal == current.goal) {
            return Ok(None);
        }
        let receiver_current = status.replica_goals.iter().find(|entry| {
            entry.goal.scope.storage == receiver.storage
                && entry.goal.scope.process_session_id == receiver.process_session_id
                && entry.goal.scope.configuration.current
                    == current.goal.scope.configuration.current
                && entry.goal.scope.configuration.previous.is_none()
                && matches!(
                    entry.goal.desired,
                    ReplicaGoalKind::Reconfigure {
                        role: ReplicaRole::ActiveSecondary,
                        catch_up: None
                    }
                )
        });
        let Some(receiver_current) = receiver_current else {
            next.replica_goals.push(goal(
                receiver,
                configuration_for(
                    receiver.storage.replica.replica_id,
                    &current.goal.scope.configuration.current,
                    None,
                    peers,
                )?,
                ReplicaGoalKind::Reconfigure {
                    role: ReplicaRole::ActiveSecondary,
                    catch_up: None,
                },
                &status.replica_goals,
            )?);
            return self.persist_goals(raw, next).await.map(Some);
        };
        if !settled(receiver_current) {
            return Ok(None);
        }
        if !settled(current) {
            return Ok(None);
        }
        let descriptor = &current.goal.scope.configuration.current;
        let sessions = [
            endpoint(source, ReplicaRole::Primary),
            endpoint(receiver, ReplicaRole::ActiveSecondary),
        ];
        if status.public_v1_topology.as_ref().is_some_and(|topology| {
            topology.configuration().ok().as_ref() == Some(descriptor)
                && topology.sessions == sessions
        }) {
            return Ok(None);
        }
        let mut topology =
            PublicV1TopologyState::from_configuration(descriptor, &sessions).map_err(invalid)?;
        topology.completed_switchover_request_id = status
            .public_v1_topology
            .as_ref()
            .and_then(|topology| topology.completed_switchover_request_id.clone());
        next.public_v1_topology = Some(topology);
        self.persist_goals(raw, next).await.map(Some)
    }
}
