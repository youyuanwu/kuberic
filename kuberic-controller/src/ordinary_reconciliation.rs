use std::collections::BTreeMap;
use std::time::Duration;

use kube::ResourceExt;
use kuberic_runtime::protocol::public_operations::*;
use kuberic_runtime::protocol::types::*;

use crate::cluster_api::{AgentApi, ClusterApi, KubeClusterApi};
use crate::crd::{
    KubericSetStatus, PublicV1AuthorityStatus, PublicV1TopologyState, RetainedReplicaGoal,
};
use crate::observation::{RawAgentObservation, RawObservation};
use crate::reconciler::{ReconcileAction, ReconcileKind};
use crate::{ControllerError, Result};

const PREPARATION_BATCH: usize = 8;

#[path = "ordinary_election.rs"]
mod election;
#[path = "ordinary_reconstruction.rs"]
mod reconstruction;

pub(crate) fn obsolete_goal_reconstructed(
    status: &KubericSetStatus,
    snapshots: &[ReplicaAgentSnapshot],
    old: &RetainedReplicaGoal,
) -> bool {
    let peers = snapshots
        .iter()
        .map(|peer| (peer.storage.replica.replica_id, peer))
        .collect();
    reconstruction::resolved(status, &peers, old)
}

fn invalid(message: impl Into<String>) -> ControllerError {
    ControllerError::InvalidAgentEvidence(message.into())
}

fn action(kind: ReconcileKind) -> ReconcileAction {
    ReconcileAction {
        requeue_after: Duration::from_millis(50),
        kind,
    }
}

fn snapshots(raw: &RawObservation) -> BTreeMap<ReplicaId, &ReplicaAgentSnapshot> {
    raw.agents
        .values()
        .filter_map(|observation| match observation {
            RawAgentObservation::ReplicaSnapshot(snapshot) => {
                Some((snapshot.storage.replica.replica_id, snapshot.as_ref()))
            }
            _ => None,
        })
        .collect()
}

fn exact_publications(
    remote: &RemoteReplicaSelection,
    peers: &BTreeMap<ReplicaId, &ReplicaAgentSnapshot>,
) -> bool {
    [&remote.source, &remote.target]
        .into_iter()
        .all(|publication| {
            peers
                .get(&publication.scope.storage.replica.replica_id)
                .is_some_and(|snapshot| snapshot.publication.as_ref() == Some(publication))
        })
}

fn goal(
    snapshot: &ReplicaAgentSnapshot,
    configuration: ReplicaConfigurationScope,
    desired: ReplicaGoalKind,
    retained: &[RetainedReplicaGoal],
) -> Result<RetainedReplicaGoal> {
    let revision = snapshot
        .goals
        .iter()
        .map(|observation| observation.goal.revision)
        .chain(std::iter::once(snapshot.accepted_revision_high_watermark))
        .chain(
            retained
                .iter()
                .filter(|entry| entry.goal.scope.storage.replica == snapshot.storage.replica)
                .map(|entry| entry.goal.revision),
        )
        .max()
        .unwrap_or(0)
        .checked_add(1)
        .ok_or_else(|| invalid("goal revision exhausted"))?;
    RetainedReplicaGoal::new(ReplicaGoal {
        format: ORDINARY_ACTION_FORMAT,
        identity: snapshot.identity.clone(),
        operation_id: OperationId::new(format!(
            "goal-{}-{}-{revision}",
            snapshot.storage.replica.replica_id, snapshot.process_session_id
        )),
        revision,
        scope: ReplicaScope {
            history: snapshot.history.clone(),
            storage: snapshot.storage.clone(),
            process_session_id: snapshot.process_session_id.clone(),
            creation_id: snapshot.creation_id.clone(),
            epoch: configuration.current.epoch,
            configuration,
        },
        desired,
    })
    .map_err(invalid)
}

fn configuration_for(
    local: ReplicaId,
    current: &ConfigurationDescriptor,
    previous: Option<&ConfigurationDescriptor>,
    peers: &BTreeMap<ReplicaId, &ReplicaAgentSnapshot>,
) -> Result<ReplicaConfigurationScope> {
    let mut members = BTreeMap::new();
    let local_identity = &peers
        .get(&local)
        .ok_or_else(|| invalid("configuration local process is unavailable"))?
        .storage
        .replica;
    for member in previous
        .into_iter()
        .chain(std::iter::once(current))
        .flat_map(|set| &set.members)
    {
        if &member.identity != local_identity {
            members.insert(member.identity.clone(), member);
        }
    }
    let replicas = members
        .into_iter()
        .map(|(identity, member)| {
            let Some(peer) = peers
                .get(&identity.replica_id)
                .filter(|peer| peer.storage.replica == member.identity)
            else {
                let mut retained = peers
                    .get(&local)
                    .and_then(|local| {
                        local
                            .publication
                            .as_ref()
                            .and_then(|publication| {
                                publication
                                    .scope
                                    .configuration
                                    .replicas
                                    .iter()
                                    .find(|description| description.identity == member.identity)
                            })
                            .or_else(|| {
                                local
                                    .goals
                                    .iter()
                                    .filter(|entry| {
                                        entry.status == ReplicaGoalStatus::Achieved
                                            && entry.host_settled
                                            && entry.goal.scope.history == local.history
                                            && entry.goal.scope.storage == local.storage
                                    })
                                    .filter_map(|entry| {
                                        entry
                                            .goal
                                            .scope
                                            .configuration
                                            .replicas
                                            .iter()
                                            .find(|description| {
                                                description.identity == member.identity
                                            })
                                            .map(|description| (entry.goal.revision, description))
                                    })
                                    .max_by_key(|(revision, _)| *revision)
                                    .map(|(_, description)| description)
                            })
                    })
                    .cloned()
                    .ok_or_else(|| {
                        invalid("unavailable configuration peer lacks exact retained metadata")
                    })?;
                retained.role = member.role;
                retained.is_up = false;
                retained.is_ready = false;
                retained.must_catchup = false;
                return Ok(retained);
            };
            let publication = peer
                .publication
                .as_ref()
                .ok_or_else(|| invalid("configuration peer is unpublished"))?;
            if publication.scope.storage.replica != member.identity {
                return Err(invalid("configuration peer incarnation changed"));
            }
            Ok(ReplicaPeerDescription {
                identity: member.identity.clone(),
                replication_address: publication.replication_address.clone(),
                process_session_id: peer.process_session_id.clone(),
                storage_incarnation: peer.storage.clone(),
                role: member.role,
                current_progress: peer.progress.value().unwrap_or(INVALID_LSN),
                catch_up_capability: peer.capability.value().unwrap_or(INVALID_LSN),
                is_up: true,
                is_ready: true,
                must_catchup: false,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(ReplicaConfigurationScope {
        current: current.clone(),
        previous: previous.cloned(),
        replicas,
    })
}

#[cfg(test)]
fn select_cold_survivor_initialization(
    status: &KubericSetStatus,
    current: &ConfigurationDescriptor,
    peer: &ReplicaAgentSnapshot,
) -> Result<Option<RetainedReplicaGoal>> {
    let Some(topology) = status.public_v1_topology.as_ref() else {
        return Ok(None);
    };
    let sampled = |result: &ProgressQueryResult| {
        matches!(
            result,
            ProgressQueryResult::Value(_) | ProgressQueryResult::NotReady
        )
    };
    if topology.unavailable_primary_id != Some(current.primary_id)
        || peer.withdrawn
        || peer.format != ORDINARY_ACTION_FORMAT
        || status.public_v1_authority.as_ref().is_none_or(|authority| {
            authority.identity != peer.identity || authority.history != peer.history
        })
        || !sampled(&peer.progress)
        || !sampled(&peer.capability)
        || (peer.progress.value().is_some_and(|value| value >= 0)
            && peer.capability.value().is_some_and(|value| value >= 0))
    {
        return Ok(None);
    }
    let Some(publication) = peer.publication.as_ref().filter(|publication| {
        publication.format == ORDINARY_ACTION_FORMAT
            && publication.stage == ReplicaPublicationStage::AchievedScope
            && publication.role == ReplicaRole::ActiveSecondary
            && publication.scope.history == peer.history
            && publication.scope.storage == peer.storage
            && publication.scope.process_session_id == peer.process_session_id
            && publication.scope.creation_id == peer.creation_id
            && publication.scope.epoch == current.epoch
            && publication.scope.configuration.current == *current
            && publication.scope.configuration.previous.is_none()
            && topology
                .sessions
                .contains(&endpoint(peer, ReplicaRole::ActiveSecondary))
            && current.members.iter().any(|member| {
                member.identity == peer.storage.replica
                    && member.role == ReplicaRole::ActiveSecondary
            })
    }) else {
        return Ok(None);
    };
    if !peer.goals.iter().any(|entry| {
        entry.goal.scope == publication.scope
            && entry.status == ReplicaGoalStatus::Achieved
            && entry.host_settled
            && matches!(
                entry.goal.desired,
                ReplicaGoalKind::Reconfigure {
                    role: ReplicaRole::ActiveSecondary,
                    catch_up: None,
                }
            )
    }) || peer.goals.iter().any(|entry| {
        entry.goal.scope.process_session_id == peer.process_session_id
            && entry.goal.scope.creation_id == peer.creation_id
            && matches!(
                entry.status,
                ReplicaGoalStatus::Pending
                    | ReplicaGoalStatus::Failed(_)
                    | ReplicaGoalStatus::Uncertain(_)
            )
    }) {
        return Ok(None);
    }
    let mut configuration = publication.scope.configuration.clone();
    let Some(unavailable) = configuration
        .replicas
        .iter_mut()
        .find(|description| description.identity.replica_id == current.primary_id)
    else {
        return Ok(None);
    };
    unavailable.is_up = false;
    unavailable.is_ready = false;
    unavailable.must_catchup = false;
    unavailable.current_progress = -1;
    unavailable.catch_up_capability = -1;
    let desired = ReplicaGoalKind::Reconfigure {
        role: ReplicaRole::ActiveSecondary,
        catch_up: None,
    };
    if status.replica_goals.iter().any(|entry| {
        entry.goal.scope.history == peer.history
            && entry.goal.scope.storage == peer.storage
            && entry.goal.scope.process_session_id == peer.process_session_id
            && entry.goal.scope.creation_id == peer.creation_id
            && entry.goal.scope.configuration == configuration
            && entry.goal.desired == desired
    }) {
        return Ok(None);
    }
    goal(peer, configuration, desired, &status.replica_goals).map(Some)
}

fn achieved(entry: &RetainedReplicaGoal) -> bool {
    entry
        .outcome
        .as_ref()
        .is_some_and(|outcome| outcome.status == ReplicaGoalStatus::Achieved)
}

fn completed_source_handoff(
    entry: &RetainedReplicaGoal,
    source: &ReplicaAgentSnapshot,
    previous: &ConfigurationDescriptor,
    selected: &ConfigurationDescriptor,
) -> bool {
    let Some(outcome) = entry.outcome.as_ref() else {
        return false;
    };
    entry.validate().is_ok()
        && outcome.goal == entry.goal
        && outcome.status == ReplicaGoalStatus::Achieved
        && outcome.host_settled
        && outcome.selected_call.is_none()
        && (source.normal_handoff_result() == Some(outcome)
            || source.native_lifecycle.is_none()
                && outcome.receipts == 7
                && outcome.completed_handoff.as_deref() == Some(&SourceHandoffReceipt::ORDERED)
                && matches!(
                    entry.goal.desired,
                    ReplicaGoalKind::Reconfigure {
                        catch_up: Some(PublicCatchUpMode::All),
                        ..
                    }
                ))
        && entry.goal.scope.history == source.history
        && entry.goal.scope.storage == source.storage
        && entry.goal.scope.process_session_id == source.process_session_id
        && entry.goal.scope.creation_id == source.creation_id
        && entry.goal.scope.configuration.previous.as_ref() == Some(previous)
        && entry.goal.scope.configuration.current == *selected
        && previous.primary_id == source.storage.replica.replica_id
        && selected.primary_id != previous.primary_id
        && selected.epoch > previous.epoch
        && selected.epoch.data_loss_number == previous.epoch.data_loss_number
        && selected.write_quorum == previous.write_quorum
        && selected.members.len() == previous.members.len()
        && previous.members.iter().all(|member| {
            selected
                .members
                .iter()
                .any(|next| next.identity == member.identity)
        })
        && matches!(
            entry.goal.desired,
            ReplicaGoalKind::Reconfigure {
                role: ReplicaRole::ActiveSecondary,
                catch_up: Some(_),
            }
        )
        && !source.withdrawn
        && source.read_status != AccessStatus::Granted
        && source.write_status != AccessStatus::Granted
        && source.goals.iter().any(|observed| observed == outcome)
        && source.publication.as_ref().is_some_and(|publication| {
            publication.stage == ReplicaPublicationStage::AchievedScope
                && publication.role == ReplicaRole::ActiveSecondary
                && publication.scope == entry.goal.scope
                && publication.generation == entry.goal.revision
        })
}

fn switchover_configuration(
    current: &ConfigurationDescriptor,
    primary_id: ReplicaId,
    advance: i64,
) -> Result<ConfigurationDescriptor> {
    if !current
        .members
        .iter()
        .any(|member| member.identity.replica_id == primary_id)
    {
        return Err(invalid("switchover target is not an admitted member"));
    }
    Ok(ConfigurationDescriptor::new(
        Epoch::new(
            current.epoch.data_loss_number,
            current
                .epoch
                .configuration_number
                .checked_add(advance)
                .ok_or_else(|| invalid("configuration epoch exhausted"))?,
        ),
        primary_id,
        current
            .members
            .iter()
            .cloned()
            .map(|mut member| {
                member.role = if member.identity.replica_id == primary_id {
                    ReplicaRole::Primary
                } else {
                    ReplicaRole::ActiveSecondary
                };
                member
            })
            .collect(),
        current.write_quorum,
    ))
}

fn current_terminal(snapshot: &ReplicaAgentSnapshot) -> bool {
    snapshot.goals.iter().any(|observation| {
        observation.goal.scope.storage == snapshot.storage
            && observation.goal.scope.process_session_id == snapshot.process_session_id
            && observation.goal.scope.creation_id == snapshot.creation_id
            && matches!(
                observation.goal.desired,
                ReplicaGoalKind::Abort | ReplicaGoalKind::Close
            )
    })
}

fn retained_member_history<'a>(
    status: &'a KubericSetStatus,
    current: &ConfigurationDescriptor,
    peer: &ReplicaAgentSnapshot,
) -> Option<&'a RetainedReplicaGoal> {
    let qualification = peer.retained_member_qualification.as_ref()?;
    let topology = status.public_v1_topology.as_ref()?;
    if peer.format != ORDINARY_ACTION_FORMAT
        || peer.withdrawn
        || peer.open_mode != OpenMode::Existing
        || peer.retained_recovery_exit.is_some()
        || current_terminal(peer)
        || !matches!(
            peer.storage.storage,
            ReplicaStorageIdentity::Persistent { .. }
        )
        || qualification.configuration != *current
        || qualification.predecessor_process_session_id == peer.process_session_id
        || qualification.predecessor_creation_id == peer.creation_id
        || topology.pending_switchover.is_some()
        || !current.members.iter().any(|member| {
            member.identity == peer.storage.replica
                && member.role == ReplicaRole::ActiveSecondary
                && member.identity.replica_id != current.primary_id
        })
        || !topology.sessions.iter().any(|session| {
            session.identity == peer.storage.replica
                && session.storage_incarnation == peer.storage
                && session.process_session_id == qualification.predecessor_process_session_id
                && session.replicator_creation_id == qualification.predecessor_creation_id
                && session.role == ReplicaRole::ActiveSecondary
        })
        || status.replica_goals.iter().any(|entry| {
            entry.goal.scope.configuration.previous.as_ref() == Some(current)
                && entry.goal.scope.configuration.current != *current
                && entry.goal.scope.epoch > current.epoch
        })
    {
        return None;
    }
    status.replica_goals.iter().find(|entry| {
        achieved(entry)
            && entry.goal.operation_id == qualification.admitted_operation_id
            && entry.goal.revision == qualification.admitted_revision
            && entry.goal.identity == peer.identity
            && entry.goal.scope.history == peer.history
            && entry.goal.scope.storage == peer.storage
            && entry.goal.scope.process_session_id == qualification.predecessor_process_session_id
            && entry.goal.scope.creation_id == qualification.predecessor_creation_id
            && entry.goal.scope.epoch == current.epoch
            && entry.goal.scope.configuration.current == *current
            && entry.goal.scope.configuration.previous.is_none()
            && matches!(
                entry.goal.desired,
                ReplicaGoalKind::Prepare {
                    role: ReplicaRole::ActiveSecondary,
                    ..
                } | ReplicaGoalKind::Reconfigure {
                    role: ReplicaRole::ActiveSecondary,
                    ..
                } | ReplicaGoalKind::RestoreRetainedMember { .. }
            )
            && peer.goals.iter().any(|observation| {
                observation.goal == entry.goal && observation.status == ReplicaGoalStatus::Achieved
            })
    })
}

fn retained_storage_present(raw: &RawObservation, peer: &ReplicaAgentSnapshot) -> bool {
    let ReplicaStorageIdentity::Persistent { pod_uid, pvc_uid } = &peer.storage.storage else {
        return false;
    };
    let Some(pvc) = raw.pvcs.iter().find(|pvc| {
        pvc.uid().as_deref() == Some(pvc_uid.as_str()) && pvc.metadata.deletion_timestamp.is_none()
    }) else {
        return false;
    };
    raw.pods.iter().any(|pod| {
        pod.uid().as_deref() == Some(pod_uid.as_str())
            && pod_uid.as_str() == peer.storage.replica.instance_id.as_str()
            && pod.metadata.deletion_timestamp.is_none()
            && pod.spec.as_ref().is_some_and(|spec| {
                spec.volumes.as_ref().is_some_and(|volumes| {
                    volumes.iter().any(|volume| {
                        volume
                            .persistent_volume_claim
                            .as_ref()
                            .is_some_and(|claim| claim.claim_name == pvc.name_any())
                    })
                })
            })
    })
}

fn retained_restore_configuration(historical: &RetainedReplicaGoal) -> ReplicaConfigurationScope {
    let mut configuration = historical.goal.scope.configuration.clone();
    for description in &mut configuration.replicas {
        description.current_progress = INVALID_LSN;
        description.catch_up_capability = INVALID_LSN;
        description.is_up = false;
        description.is_ready = false;
        description.must_catchup = false;
    }
    configuration
}

#[cfg(test)]
fn select_retained_restore(
    status: &KubericSetStatus,
    current: &ConfigurationDescriptor,
    peer: &ReplicaAgentSnapshot,
) -> Result<Option<RetainedReplicaGoal>> {
    let Some(historical) = retained_member_history(status, current, peer) else {
        return Ok(None);
    };
    if peer.publication.is_some()
        || peer.provider_writer_held
        || peer
            .goals
            .iter()
            .any(|observation| observation.goal.scope.process_session_id == peer.process_session_id)
        || status.replica_goals.iter().any(|entry| {
            entry.goal.scope.storage == peer.storage
                && entry.goal.scope.process_session_id == peer.process_session_id
        })
    {
        return Ok(None);
    }
    goal(
        peer,
        retained_restore_configuration(historical),
        ReplicaGoalKind::RestoreRetainedMember {
            qualification: Box::new(peer.retained_member_qualification.clone().unwrap()),
        },
        &status.replica_goals,
    )
    .map(Some)
}

fn retained_restore_matches(
    status: &KubericSetStatus,
    current: &ConfigurationDescriptor,
    peer: &ReplicaAgentSnapshot,
    selected: &ReplicaGoal,
) -> bool {
    let ReplicaGoalKind::RestoreRetainedMember { qualification } = &selected.desired else {
        return false;
    };
    retained_member_history(status, current, peer).is_some_and(|historical| {
        peer.retained_member_qualification.as_ref() == Some(qualification.as_ref())
            && selected.identity == peer.identity
            && selected.scope.history == peer.history
            && selected.scope.storage == peer.storage
            && selected.scope.process_session_id == peer.process_session_id
            && selected.scope.creation_id == peer.creation_id
            && selected.scope.epoch == current.epoch
            && selected.scope.configuration == retained_restore_configuration(historical)
            && !peer.goals.iter().any(|observation| {
                observation.goal.scope.process_session_id == peer.process_session_id
                    && observation.goal != *selected
            })
            && !status.replica_goals.iter().any(|entry| {
                entry.goal.scope.storage == peer.storage
                    && entry.goal.scope.process_session_id == peer.process_session_id
                    && entry.goal != *selected
            })
    })
}

fn retained_restore_achieved(peer: &ReplicaAgentSnapshot, selected: &ReplicaGoal) -> bool {
    let ReplicaGoalKind::RestoreRetainedMember { qualification } = &selected.desired else {
        return false;
    };
    peer.format == ORDINARY_ACTION_FORMAT
        && !peer.withdrawn
        && !current_terminal(peer)
        && peer.retained_recovery_exit.is_none()
        && peer.identity == selected.identity
        && peer.history == selected.scope.history
        && peer.storage == selected.scope.storage
        && peer.process_session_id == selected.scope.process_session_id
        && peer.creation_id == selected.scope.creation_id
        && peer.retained_member_qualification.as_ref() == Some(qualification.as_ref())
        && peer.goals.iter().any(|observation| {
            observation.goal == *selected && observation.status == ReplicaGoalStatus::Achieved
        })
        && peer.publication.as_ref().is_some_and(|publication| {
            publication.validate().is_ok()
                && publication.stage == ReplicaPublicationStage::AchievedScope
                && publication.role == ReplicaRole::ActiveSecondary
                && publication.scope == selected.scope
        })
}

fn adopt_retained_member(
    status: &KubericSetStatus,
    current: &ConfigurationDescriptor,
    sampled: &ReplicaAgentSnapshot,
    selected: &RetainedReplicaGoal,
) -> Result<Option<KubericSetStatus>> {
    if !achieved(selected)
        || !retained_restore_matches(status, current, sampled, &selected.goal)
        || !retained_restore_achieved(sampled, &selected.goal)
        || sampled.progress.value().is_none_or(|value| value < 0)
        || sampled.capability.value().is_none_or(|value| value < 0)
    {
        return Ok(None);
    }
    let mut next = status.clone();
    let topology = next.public_v1_topology.as_mut().unwrap();
    let session = topology
        .sessions
        .iter_mut()
        .find(|session| session.identity == sampled.storage.replica)
        .unwrap();
    *session = endpoint(sampled, ReplicaRole::ActiveSecondary);
    if topology.configuration().map_err(invalid)? != *current {
        return Err(invalid(
            "retained restoration changed configuration membership",
        ));
    }
    Ok(Some(next))
}

fn replacement_predecessor(
    status: &KubericSetStatus,
    current: &ConfigurationDescriptor,
    id: ReplicaId,
) -> Option<ReplicaStorageIncarnationContext> {
    if let Some(member) = current
        .members
        .iter()
        .find(|member| member.identity.replica_id == id)
    {
        if id == current.primary_id
            || current.members.len().saturating_sub(1) >= current.write_quorum as usize
        {
            return None;
        }
        return status.replica_goals.iter().rev().find_map(|entry| {
            (achieved(entry)
                && entry.goal.scope.configuration.current == *current
                && matches!(
                    entry.goal.desired,
                    ReplicaGoalKind::Reconfigure {
                        role: ReplicaRole::Primary,
                        ..
                    } | ReplicaGoalKind::RefreshPrimary { .. }
                ))
            .then(|| {
                entry
                    .goal
                    .scope
                    .configuration
                    .replicas
                    .iter()
                    .find(|peer| peer.identity == member.identity)
                    .map(|peer| peer.storage_incarnation.clone())
            })
            .flatten()
        });
    }
    status.replica_goals.iter().rev().find_map(|entry| {
        if !achieved(entry)
            || entry.goal.scope.configuration.current != *current
            || !matches!(
                entry.goal.desired,
                ReplicaGoalKind::Reconfigure {
                    role: ReplicaRole::Primary,
                    ..
                }
            )
        {
            return None;
        }
        let predecessor = entry
            .goal
            .scope
            .configuration
            .previous
            .as_ref()?
            .members
            .iter()
            .find(|member| member.identity.replica_id == id)?;
        entry
            .goal
            .scope
            .configuration
            .replicas
            .iter()
            .find(|peer| peer.identity == predecessor.identity)
            .map(|peer| peer.storage_incarnation.clone())
    })
}

impl<A: AgentApi + 'static> KubeClusterApi<A> {
    async fn swap_mode(
        &self,
        raw: &RawObservation,
        source: &ReplicaAgentSnapshot,
    ) -> Result<PublicCatchUpMode> {
        let report = self
            .agents
            .get_status(
                &peer_address(raw, &source.storage.replica)?,
                &self.bearer_token,
                kuberic_runtime::control::proto::GetAgentStatusRequest {
                    protocol_version: kuberic_runtime::protocol::PROTOCOL_VERSION,
                    resource_uid: source.history.resource_uid.to_string(),
                    replica_id: source.storage.replica.replica_id.value(),
                    expected_instance_id: source.storage.replica.instance_id.to_string(),
                },
            )
            .await
            .map_err(crate::cluster_api::map_agent_effect_error)?
            .public_v1
            .ok_or_else(|| invalid("swap source has no registered capability report"))?;
        if report.process_session_id != source.process_session_id.as_str()
            || report.replicator_creation_id != source.creation_id.as_str()
            || report.resource_uid != source.history.resource_uid.as_str()
            || report.identity.as_ref() != Some(&source.storage.replica.clone().into())
        {
            return Err(ControllerError::ObservationStale);
        }
        match report.catch_up_specific_quorum {
            Some(true) => Ok(PublicCatchUpMode::WriteQuorum),
            Some(false) => Ok(PublicCatchUpMode::All),
            None => Err(invalid("swap source capability is unavailable")),
        }
    }

    async fn reconcile_inspection_round(
        &self,
        raw: &RawObservation,
        status: &KubericSetStatus,
        current: &ConfigurationDescriptor,
        peers: &BTreeMap<ReplicaId, &ReplicaAgentSnapshot>,
    ) -> Result<Option<ReconcileAction>> {
        if peers
            .get(&current.primary_id)
            .is_some_and(|peer| !peer.withdrawn && peer.publication.is_some())
            || status.replica_goals.iter().any(|entry| {
                entry.goal.scope.configuration.previous.as_ref() == Some(current)
                    && matches!(
                        entry.goal.desired,
                        ReplicaGoalKind::Reconfigure {
                            role: ReplicaRole::Primary,
                            ..
                        }
                    )
            })
        {
            return Ok(None);
        }
        let mut next_status = status.clone();
        let mut captured = false;
        for entry in &mut next_status.replica_goals {
            if entry.native_status.is_some()
                || !matches!(entry.goal.desired, ReplicaGoalKind::Discover)
                || entry.goal.scope.configuration.previous.as_ref() != Some(current)
                || !entry.outcome.as_ref().is_some_and(|outcome| {
                    outcome.status == ReplicaGoalStatus::Achieved && outcome.host_settled
                })
            {
                continue;
            }
            if let Some(peer) = peers.get(&entry.goal.scope.storage.replica.replica_id)
                && peer.storage == entry.goal.scope.storage
                && peer.process_session_id == entry.goal.scope.process_session_id
                && peer.creation_id == entry.goal.scope.creation_id
                && !peer.withdrawn
                && peer.goals.iter().any(|outcome| {
                    outcome.goal == entry.goal
                        && outcome.status == ReplicaGoalStatus::Achieved
                        && outcome.host_settled
                })
                && let Some(native) = peer.native_status.as_ref()
                && election::inspection_matches(&entry.goal, native)
            {
                entry.native_status = Some(
                    serde_json::to_string(native).map_err(|error| invalid(error.to_string()))?,
                );
                captured = true;
            }
        }
        if captured {
            return self.persist_goals(raw, next_status).await.map(Some);
        }
        let topology = next_status.public_v1_topology.as_mut().unwrap();
        if topology.unavailable_primary_id != Some(current.primary_id) {
            topology.unavailable_primary_id = Some(current.primary_id);
            topology.unavailable_since_unix_seconds = Some(raw.now_unix_seconds);
            return self.persist_goals(raw, next_status).await.map(Some);
        }
        let delay = i64::try_from(raw.set.spec.failover_delay_seconds).unwrap_or(i64::MAX);
        if raw.now_unix_seconds
            < topology
                .unavailable_since_unix_seconds
                .unwrap_or(raw.now_unix_seconds)
                .saturating_add(delay)
        {
            return Ok(None);
        }
        let original = current
            .members
            .iter()
            .filter(|member| member.identity.replica_id != current.primary_id)
            .collect::<Vec<_>>();
        if original.len() < current.write_quorum as usize {
            return Ok(None);
        }
        let round = status
            .replica_goals
            .iter()
            .rev()
            .find(|entry| {
                matches!(entry.goal.desired, ReplicaGoalKind::Discover)
                    && entry.goal.scope.configuration.previous.as_ref() == Some(current)
                    && entry.goal.scope.epoch > current.epoch
            })
            .map(|entry| entry.goal.scope.configuration.current.clone());
        let mut next = if let Some(round) = round {
            round
        } else {
            let available = |member: &ConfigurationMember| {
                peers
                    .get(&member.identity.replica_id)
                    .is_some_and(|peer| !peer.withdrawn && peer.storage.replica == member.identity)
            };
            let Some(nominee) = original
                .iter()
                .find(|member| {
                    available(member)
                        && peers[&member.identity.replica_id]
                            .native_lifecycle
                            .as_ref()
                            .is_some_and(|facts| {
                                facts.state == NativeReplicaState::Ready
                                    && facts.native_role == ReplicaRole::ActiveSecondary
                                    && facts.application_role == Some(ReplicaRole::ActiveSecondary)
                            })
                })
                .or_else(|| original.iter().find(|member| available(member)))
            else {
                return Ok(None);
            };
            let highest = status
                .replica_goals
                .iter()
                .map(|entry| entry.goal.scope.epoch)
                .chain(std::iter::once(current.epoch))
                .max()
                .unwrap();
            ConfigurationDescriptor::new(
                Epoch::new(
                    current.epoch.data_loss_number,
                    highest
                        .configuration_number
                        .checked_add(1)
                        .ok_or_else(|| invalid("inspection fence exhausted"))?,
                ),
                nominee.identity.replica_id,
                original
                    .iter()
                    .map(|member| ConfigurationMember {
                        identity: member.identity.clone(),
                        role: if member.identity == nominee.identity {
                            ReplicaRole::Primary
                        } else {
                            ReplicaRole::ActiveSecondary
                        },
                    })
                    .collect(),
                current.write_quorum,
            )
        };
        let mut added = false;
        for member in &original {
            let Some(peer) = peers
                .get(&member.identity.replica_id)
                .filter(|peer| !peer.withdrawn && peer.storage.replica == member.identity)
            else {
                continue;
            };
            if peer.publication.is_none()
                && (peer.open_mode != OpenMode::Existing || !retained_storage_present(raw, peer))
            {
                continue;
            }
            if !status.replica_goals.iter().any(|entry| {
                entry.goal.scope.storage == peer.storage
                    && entry.goal.scope.process_session_id == peer.process_session_id
                    && entry.goal.scope.creation_id == peer.creation_id
                    && entry.goal.scope.configuration.current == next
                    && entry.goal.scope.configuration.previous.as_ref() == Some(current)
                    && matches!(entry.goal.desired, ReplicaGoalKind::Discover)
            }) {
                next_status.replica_goals.push(goal(
                    peer,
                    ReplicaConfigurationScope {
                        current: next.clone(),
                        previous: Some(current.clone()),
                        replicas: Vec::new(),
                    },
                    ReplicaGoalKind::Discover,
                    &next_status.replica_goals,
                )?);
                added = true;
            }
        }
        if added {
            return self.persist_goals(raw, next_status).await.map(Some);
        }
        let records = status
            .replica_goals
            .iter()
            .filter(|entry| {
                matches!(entry.goal.desired, ReplicaGoalKind::Discover)
                    && entry.goal.scope.configuration.current == next
                    && entry.goal.scope.configuration.previous.as_ref() == Some(current)
            })
            .collect::<Vec<_>>();
        let observations = records
            .iter()
            .filter_map(|entry| {
                let observed = entry.native_status().ok().flatten()?;
                if !election::inspection_matches(&entry.goal, &observed) {
                    return None;
                }
                Some((*entry, observed))
            })
            .collect::<Vec<_>>();
        let replies = observations
            .iter()
            .map(|(entry, observed)| election::Reply {
                identity: &entry.goal.scope.storage.replica,
                status: observed,
                up: peers
                    .get(&entry.goal.scope.storage.replica.replica_id)
                    .is_some_and(|peer| {
                        !peer.withdrawn
                            && peer.history == entry.goal.scope.history
                            && peer.storage == entry.goal.scope.storage
                            && peer.process_session_id == entry.goal.scope.process_session_id
                            && peer.creation_id == entry.goal.scope.creation_id
                            && peer.native_status.as_ref() == Some(observed)
                    }),
            })
            .collect::<Vec<_>>();
        let decision = election::select(current, &next, next.primary_id, &replies);
        #[cfg(feature = "test-util")]
        if std::env::var_os("KUBERIC_M3_ELECTION_DIAGNOSTICS").is_some() {
            eprintln!(
                "native inspection round fence={:?}, cc={}, decision={decision:?}, replies={:?}",
                next.epoch,
                next.configuration_id,
                replies
                    .iter()
                    .map(|reply| (reply.identity.replica_id, reply.up, reply.status))
                    .collect::<Vec<_>>()
            );
        }
        let election::Decision::Primary(primary_id) = decision else {
            return Ok(None);
        };
        if next.primary_id != primary_id {
            next.primary_id = primary_id;
            for member in &mut next.members {
                member.role = if member.identity.replica_id == primary_id {
                    ReplicaRole::Primary
                } else {
                    ReplicaRole::ActiveSecondary
                };
            }
            next.configuration_id = next.expected_id();
            for member in &original {
                let Some(peer) = peers
                    .get(&member.identity.replica_id)
                    .filter(|peer| !peer.withdrawn && peer.storage.replica == member.identity)
                else {
                    continue;
                };
                next_status.replica_goals.push(goal(
                    peer,
                    ReplicaConfigurationScope {
                        current: next.clone(),
                        previous: Some(current.clone()),
                        replicas: Vec::new(),
                    },
                    ReplicaGoalKind::Discover,
                    &next_status.replica_goals,
                )?);
            }
            return self.persist_goals(raw, next_status).await.map(Some);
        }
        let Some(primary) = peers.get(&primary_id) else {
            return Ok(None);
        };
        let mut sampled = BTreeMap::new();
        for reply in &replies {
            if let Some(peer) = peers.get(&reply.identity.replica_id).filter(|_| reply.up) {
                let mut peer = (*peer).clone();
                peer.progress = reply.status.progress.clone();
                peer.capability = reply.status.capability.clone();
                sampled.insert(reply.identity.replica_id, peer);
            }
        }
        let sampled = sampled.iter().map(|(id, peer)| (*id, peer)).collect();
        let mut configuration = configuration_for(primary_id, &next, Some(current), &sampled)?;
        for description in &mut configuration.replicas {
            description.is_ready = replies.iter().any(|reply| {
                reply.identity == &description.identity
                    && reply.up
                    && reply.status.input.state == NativeReplicaState::Ready
                    && reply.status.input.native_role == ReplicaRole::ActiveSecondary
                    && reply.status.input.application_role == Some(ReplicaRole::ActiveSecondary)
                    && peers.get(&reply.identity.replica_id).is_some_and(|peer| {
                        status
                            .public_v1_topology
                            .as_ref()
                            .unwrap()
                            .sessions
                            .contains(&endpoint(peer, ReplicaRole::ActiveSecondary))
                            && peer.publication.as_ref().is_some_and(|publication| {
                                publication.stage == ReplicaPublicationStage::AchievedScope
                                    && publication.role == ReplicaRole::ActiveSecondary
                                    && publication.scope.configuration.current == *current
                            })
                    })
            });
            let source = replies
                .iter()
                .find(|reply| reply.identity.replica_id == primary_id);
            let remote = replies
                .iter()
                .find(|reply| reply.identity == &description.identity);
            if let (Some(source), Some(remote)) = (source, remote)
                && let (Some(last), Some(first), Some(remote_last)) = (
                    source.status.progress.value(),
                    source.status.capability.value(),
                    remote.status.progress.value(),
                )
                && remote_last < last
                && (first == 0 || first > remote_last.saturating_add(1))
            {
                description.is_ready = false;
            }
            description.is_up &= description.is_ready;
        }
        next_status.replica_goals.push(goal(
            primary,
            configuration,
            ReplicaGoalKind::Reconfigure {
                role: ReplicaRole::Primary,
                catch_up: if replies.iter().any(|reply| {
                    reply.up && reply.status.input.state == NativeReplicaState::StandBy
                }) {
                    None
                } else {
                    Some(PublicCatchUpMode::WriteQuorum)
                },
            },
            &next_status.replica_goals,
        )?);
        self.persist_goals(raw, next_status).await.map(Some)
    }

    async fn reconcile_retained_members(
        &self,
        raw: &RawObservation,
        status: &KubericSetStatus,
        current: &ConfigurationDescriptor,
        peers: &BTreeMap<ReplicaId, &ReplicaAgentSnapshot>,
    ) -> Result<Option<ReconcileAction>> {
        for member in current
            .members
            .iter()
            .filter(|member| member.role == ReplicaRole::ActiveSecondary)
        {
            let Some(peer) = peers.get(&member.identity.replica_id) else {
                continue;
            };
            if status
                .public_v1_topology
                .as_ref()
                .unwrap()
                .sessions
                .contains(&endpoint(peer, member.role))
            {
                continue;
            }
            let selected = status.replica_goals.iter().rev().find(|entry| {
                entry.goal.scope.storage == peer.storage
                    && entry.goal.scope.process_session_id == peer.process_session_id
                    && entry.goal.scope.creation_id == peer.creation_id
                    && matches!(
                        entry.goal.desired,
                        ReplicaGoalKind::RestoreRetainedMember { .. }
                    )
            });
            if let Some(selected) = selected {
                if selected.outcome.as_ref().is_some_and(|outcome| {
                    matches!(
                        outcome.status,
                        ReplicaGoalStatus::Failed(_) | ReplicaGoalStatus::Uncertain(_)
                    )
                }) || peer.goals.iter().any(|observation| {
                    observation.goal == selected.goal
                        && matches!(
                            observation.status,
                            ReplicaGoalStatus::Failed(_) | ReplicaGoalStatus::Uncertain(_)
                        )
                }) {
                    // Preserve the original error through normal completion and scoped blocking.
                    return Ok(None);
                }
                if !retained_storage_present(raw, peer)
                    || !retained_restore_matches(status, current, peer, &selected.goal)
                {
                    return Err(invalid(
                        "retained restoration lost exact historical eligibility",
                    ));
                }
                if !achieved(selected) {
                    return Ok(None);
                }
                if !retained_restore_achieved(peer, &selected.goal) {
                    return Err(ControllerError::ObservationStale);
                }
                // This query must run while the restoration scope is still achieved:
                // native reformation observes its fresh paired receipts here.
                let sampled = self.sample_recovery(raw, peer).await?;
                let Some(next) = adopt_retained_member(status, current, &sampled, selected)? else {
                    return Ok(Some(action(ReconcileKind::Waiting)));
                };
                return self.persist_goals(raw, next).await.map(Some);
            }
            // Preserve normal source-dependent retained transfer when the
            // existing Primary is still published. Local cold reconstruction
            // does not itself reconnect a same-epoch live source's old session.
            // Already selected reconstruction above still runs to settlement.
            if peers
                .get(&current.primary_id)
                .is_some_and(|primary| !primary.withdrawn && primary.publication.is_some())
            {
                continue;
            }
            // New owners use actual Existing Open/Discover rounds. Historical
            // Restore records above remain immutable, but are never selected
            // merely to make a native sample available.
        }
        Ok(None)
    }

    async fn reconcile_full_quorum_replacement(
        &self,
        raw: &RawObservation,
        status: &KubericSetStatus,
        current: &ConfigurationDescriptor,
        peers: &BTreeMap<ReplicaId, &ReplicaAgentSnapshot>,
    ) -> Result<Option<ReconcileAction>> {
        if current.members.len() != 2
            || current.write_quorum != 2
            || status
                .public_v1_topology
                .as_ref()
                .unwrap()
                .pending_switchover
                .is_some()
        {
            return Ok(None);
        }
        let member = current
            .members
            .iter()
            .find(|member| member.identity.replica_id != current.primary_id)
            .unwrap();
        if raw.pods.iter().any(|pod| {
            pod.uid().as_deref() == Some(member.identity.instance_id.as_str())
                && pod.metadata.deletion_timestamp.is_none()
        }) {
            return Ok(None);
        }
        let Some(source) = peers.get(&current.primary_id).filter(|peer| {
            !peer.withdrawn
                && status
                    .public_v1_topology
                    .as_ref()
                    .unwrap()
                    .sessions
                    .iter()
                    .any(|session| {
                        session.identity.replica_id == current.primary_id
                            && session.storage_incarnation == peer.storage
                    })
        }) else {
            return Ok(None);
        };
        let predecessor = status
            .replica_goals
            .iter()
            .rev()
            .filter(|entry| achieved(entry))
            .filter(|entry| entry.goal.scope.configuration.current == *current)
            .find_map(|entry| {
                entry
                    .goal
                    .scope
                    .configuration
                    .replicas
                    .iter()
                    .find(|peer| peer.identity == member.identity)
                    .cloned()
            })
            .ok_or_else(|| invalid("full-quorum replacement lacks exact predecessor metadata"))?;
        let Some(target) = peers
            .get(&member.identity.replica_id)
            .filter(|peer| peer.storage.replica != member.identity && !peer.withdrawn)
        else {
            return Ok(Some(action(ReconcileKind::Waiting)));
        };
        if target.storage == predecessor.storage_incarnation
            || target.process_session_id == predecessor.process_session_id
        {
            return Err(invalid(
                "full-quorum successor is not a distinct physical process",
            ));
        }
        let mut preparation = current.clone();
        preparation.epoch.configuration_number = current
            .epoch
            .configuration_number
            .checked_add(1)
            .ok_or_else(|| invalid("configuration epoch exhausted"))?;
        preparation.configuration_id = preparation.expected_id();
        let mut down = predecessor.clone();
        down.is_up = false;
        down.is_ready = false;
        down.must_catchup = false;
        down.current_progress = INVALID_LSN;
        down.catch_up_capability = INVALID_LSN;
        let prepared_scope = ReplicaConfigurationScope {
            current: preparation.clone(),
            previous: None,
            replicas: vec![down],
        };
        let mut status = status.clone();
        let source_prepared = status.replica_goals.iter().any(|entry| {
            achieved(entry)
                && entry.goal.scope.storage == source.storage
                && entry.goal.scope.process_session_id == source.process_session_id
                && entry.goal.scope.creation_id == source.creation_id
                && entry.goal.scope.configuration == prepared_scope
                && matches!(
                    entry.goal.desired,
                    ReplicaGoalKind::RefreshPrimary { catch_up: None }
                        | ReplicaGoalKind::Prepare {
                            role: ReplicaRole::Primary,
                            ..
                        }
                )
        });
        if !source_prepared {
            let Some(publication) = &source.publication else {
                return Ok(Some(action(ReconcileKind::Waiting)));
            };
            let desired = if publication.stage == ReplicaPublicationStage::DiscoveryPending
                && publication.role == ReplicaRole::None
            {
                ReplicaGoalKind::Prepare {
                    role: ReplicaRole::Primary,
                    rebuild: None,
                }
            } else if publication.stage == ReplicaPublicationStage::AchievedScope
                && publication.role == ReplicaRole::Primary
                && publication.scope.configuration.current == *current
                && publication.scope.configuration.previous.is_none()
            {
                ReplicaGoalKind::RefreshPrimary { catch_up: None }
            } else {
                return Err(invalid(
                    "replacement source is not the exact stable retained Primary",
                ));
            };
            status.replica_goals.push(goal(
                source,
                prepared_scope,
                desired,
                &status.replica_goals,
            )?);
            return self.persist_goals(raw, status).await.map(Some);
        }
        if target.publication.is_none() {
            status.replica_goals.push(goal(
                target,
                ReplicaConfigurationScope {
                    current: preparation.clone(),
                    previous: None,
                    replicas: Vec::new(),
                },
                ReplicaGoalKind::Discover,
                &status.replica_goals,
            )?);
            return self.persist_goals(raw, status).await.map(Some);
        }
        if target.publication.as_ref().unwrap().stage == ReplicaPublicationStage::DiscoveryPending {
            let source_publication = source
                .publication
                .as_ref()
                .ok_or_else(|| invalid("prepared source lost its publication"))?;
            let sampled = self.sample_recovery(raw, source).await?;
            let mut idle = prepared_scope;
            idle.replicas.push(ReplicaPeerDescription {
                identity: source.storage.replica.clone(),
                replication_address: source_publication.replication_address.clone(),
                process_session_id: source.process_session_id.clone(),
                storage_incarnation: source.storage.clone(),
                role: ReplicaRole::Primary,
                current_progress: sampled.progress.value().unwrap_or(INVALID_LSN),
                catch_up_capability: sampled.capability.value().unwrap_or(INVALID_LSN),
                is_up: true,
                is_ready: true,
                must_catchup: false,
            });
            status.replica_goals.push(goal(
                target,
                idle,
                ReplicaGoalKind::Prepare {
                    role: ReplicaRole::IdleSecondary,
                    rebuild: None,
                },
                &status.replica_goals,
            )?);
            return self.persist_goals(raw, status).await.map(Some);
        }
        let built = status.replica_goals.iter().find(|entry| {
            achieved(entry)
                && entry.goal.scope.storage == source.storage
                && entry.goal.scope.process_session_id == source.process_session_id
                && entry.goal.scope.creation_id == source.creation_id
                && matches!(&entry.goal.desired, ReplicaGoalKind::Build { remote }
                    if remote.target.scope.storage == target.storage
                        && remote.target.scope.process_session_id == target.process_session_id
                        && remote.target.scope.creation_id == target.creation_id)
        });
        if built.is_none() {
            let source_publication = source
                .publication
                .clone()
                .ok_or_else(|| invalid("prepared replacement source is unpublished"))?;
            let target_publication = target.publication.clone().unwrap();
            let generation = source
                .remote_operations
                .iter()
                .filter(|remote| {
                    remote.selection.target.scope.storage == target.storage
                        && remote.selection.target.scope.process_session_id
                            == target.process_session_id
                        && remote.selection.target.scope.creation_id == target.creation_id
                })
                .map(|remote| remote.selection.generation)
                .max()
                .unwrap_or(0)
                .checked_add(1)
                .ok_or_else(|| invalid("remote generation exhausted"))?;
            let endpoint = raw
                .pods
                .iter()
                .find(|pod| {
                    pod.uid().as_deref() == Some(target.storage.replica.instance_id.as_str())
                        && pod.metadata.deletion_timestamp.is_none()
                })
                .and_then(crate::cluster_api::agent_endpoint)
                .ok_or_else(|| invalid("replacement target has no exact control endpoint"))?;
            let remote = RemoteReplicaSelection {
                original_member: None,
                source: source_publication.clone(),
                target: target_publication,
                target_control_address: endpoint,
                generation,
            };
            remote.validate().map_err(invalid)?;
            status.replica_goals.push(goal(
                source,
                source_publication.scope.configuration.clone(),
                ReplicaGoalKind::Build {
                    remote: Box::new(remote),
                },
                &status.replica_goals,
            )?);
            return self.persist_goals(raw, status).await.map(Some);
        }
        let mut next = preparation.clone();
        next.members
            .iter_mut()
            .find(|candidate| candidate.identity.replica_id == target.storage.replica.replica_id)
            .unwrap()
            .identity = target.storage.replica.clone();
        next.configuration_id = next.expected_id();
        let selected = |entry: &RetainedReplicaGoal,
                        peer: &ReplicaAgentSnapshot,
                        previous: Option<&ConfigurationDescriptor>| {
            achieved(entry)
                && entry.goal.scope.storage == peer.storage
                && entry.goal.scope.process_session_id == peer.process_session_id
                && entry.goal.scope.creation_id == peer.creation_id
                && entry.goal.scope.configuration.current == next
                && entry.goal.scope.configuration.previous.as_ref() == previous
        };
        if !status.replica_goals.iter().any(|entry| {
            selected(entry, source, Some(current))
                && matches!(
                    entry.goal.desired,
                    ReplicaGoalKind::RefreshPrimary {
                        catch_up: Some(PublicCatchUpMode::WriteQuorum),
                    }
                )
        }) {
            status.replica_goals.push(goal(
                source,
                configuration_for(current.primary_id, &next, Some(current), peers)?,
                ReplicaGoalKind::RefreshPrimary {
                    catch_up: Some(PublicCatchUpMode::WriteQuorum),
                },
                &status.replica_goals,
            )?);
            return self.persist_goals(raw, status).await.map(Some);
        }
        for previous in [Some(current), None] {
            if !status.replica_goals.iter().any(|entry| {
                selected(entry, target, previous)
                    && matches!(
                        entry.goal.desired,
                        ReplicaGoalKind::Reconfigure {
                            role: ReplicaRole::ActiveSecondary,
                            ..
                        }
                    )
            }) {
                status.replica_goals.push(goal(
                    target,
                    configuration_for(target.storage.replica.replica_id, &next, previous, peers)?,
                    ReplicaGoalKind::Reconfigure {
                        role: ReplicaRole::ActiveSecondary,
                        catch_up: None,
                    },
                    &status.replica_goals,
                )?);
                return self.persist_goals(raw, status).await.map(Some);
            }
        }
        if !status.replica_goals.iter().any(|entry| {
            selected(entry, source, None)
                && matches!(
                    entry.goal.desired,
                    ReplicaGoalKind::RefreshPrimary {
                        catch_up: Some(PublicCatchUpMode::WriteQuorum),
                    }
                )
        }) {
            status.replica_goals.push(goal(
                source,
                configuration_for(current.primary_id, &next, None, peers)?,
                ReplicaGoalKind::RefreshPrimary {
                    catch_up: Some(PublicCatchUpMode::WriteQuorum),
                },
                &status.replica_goals,
            )?);
            return self.persist_goals(raw, status).await.map(Some);
        }
        let sessions = vec![
            endpoint(source, ReplicaRole::Primary),
            endpoint(target, ReplicaRole::ActiveSecondary),
        ];
        let mut topology =
            PublicV1TopologyState::from_configuration(&next, &sessions).map_err(invalid)?;
        topology.completed_switchover_request_id = status
            .public_v1_topology
            .as_ref()
            .unwrap()
            .completed_switchover_request_id
            .clone();
        status.public_v1_topology = Some(topology);
        self.persist_goals(raw, status).await.map(Some)
    }

    async fn reconcile_accepted_switchover(
        &self,
        raw: &RawObservation,
        status: &KubericSetStatus,
        current: &ConfigurationDescriptor,
        peers: &BTreeMap<ReplicaId, &ReplicaAgentSnapshot>,
    ) -> Result<Option<ReconcileAction>> {
        let topology = status
            .public_v1_topology
            .as_ref()
            .ok_or_else(|| invalid("switchover lacks retained topology"))?;
        let request = topology.pending_switchover.as_ref().or_else(|| {
            raw.set.spec.switchover.as_ref().filter(|request| {
                topology.completed_switchover_request_id.as_ref() != Some(&request.request_id)
            })
        });
        let Some(request) = request else {
            return Ok(None);
        };
        let target = ReplicaId::new(i64::from(request.target_replica_id));
        let mut status = status.clone();
        if topology.pending_switchover.is_none() && target == current.primary_id {
            status
                .public_v1_topology
                .as_mut()
                .unwrap()
                .completed_switchover_request_id = Some(request.request_id.clone());
            return self.persist_goals(raw, status).await.map(Some);
        }
        if topology.pending_switchover.is_none()
            && (peers.get(&current.primary_id).is_none_or(|peer| {
                peer.withdrawn
                    || peer.read_status != AccessStatus::Granted
                    || peer.write_status != AccessStatus::Granted
            }) || status.replica_goals.iter().any(|entry| {
                entry.goal.scope.configuration.previous.as_ref() == Some(current)
                    && entry.goal.scope.configuration.current.primary_id != current.primary_id
                    && matches!(
                        entry.goal.desired,
                        ReplicaGoalKind::Reconfigure {
                            role: ReplicaRole::Primary,
                            ..
                        }
                    )
            }))
        {
            // An unaccepted request cannot obstruct an already-selected election.
            return Ok(None);
        }
        if current.members.iter().any(|member| {
            peers.get(&member.identity.replica_id).is_none_or(|peer| {
                peer.withdrawn
                    || !topology.sessions.contains(&endpoint(peer, member.role))
                    || peer.publication.as_ref().is_none_or(|publication| {
                        publication.stage != ReplicaPublicationStage::AchievedScope
                    })
            })
        }) {
            return Ok(Some(action(ReconcileKind::Waiting)));
        }
        let intermediate = switchover_configuration(current, current.primary_id, 1)?;
        let handoff = switchover_configuration(current, target, 1)?;
        let next = handoff.clone();
        let source = peers[&current.primary_id];
        let mode = if let Some(selected) = status.replica_goals.iter().find(|entry| {
            entry.goal.scope.storage == source.storage
                && entry.goal.scope.process_session_id == source.process_session_id
                && entry.goal.scope.creation_id == source.creation_id
                && entry.goal.scope.configuration.current == *current
                && entry.goal.scope.configuration.previous.is_none()
                && entry
                    .goal
                    .scope
                    .configuration
                    .replicas
                    .iter()
                    .any(|peer| peer.identity.replica_id == target && peer.must_catchup)
                && matches!(
                    entry.goal.desired,
                    ReplicaGoalKind::Reconfigure {
                        role: ReplicaRole::Primary,
                        catch_up: Some(_),
                    }
                )
        }) {
            let ReplicaGoalKind::Reconfigure {
                catch_up: Some(mode),
                ..
            } = selected.goal.desired
            else {
                unreachable!()
            };
            mode
        } else {
            self.swap_mode(raw, source).await?
        };
        if topology.pending_switchover.is_none() {
            if current.members.iter().any(|member| {
                peers[&member.identity.replica_id]
                    .publication
                    .as_ref()
                    .is_none_or(|publication| {
                        publication.role != member.role
                            || publication.scope.configuration.current != *current
                            || publication.scope.configuration.previous.is_some()
                    })
            }) {
                return Ok(Some(action(ReconcileKind::Waiting)));
            }
            status
                .public_v1_topology
                .as_mut()
                .unwrap()
                .pending_switchover = Some(request.clone());
            let mut configuration = configuration_for(current.primary_id, current, None, peers)?;
            for peer in &mut configuration.replicas {
                peer.must_catchup = peer.identity.replica_id == target;
            }
            status.replica_goals.push(goal(
                source,
                configuration,
                ReplicaGoalKind::Reconfigure {
                    role: ReplicaRole::Primary,
                    catch_up: Some(mode),
                },
                &status.replica_goals,
            )?);
            return self.persist_goals(raw, status).await.map(Some);
        }
        let selected = |configuration: &ConfigurationDescriptor,
                        peer: &ReplicaAgentSnapshot,
                        desired: &ReplicaGoalKind| {
            status.replica_goals.iter().rev().find(|entry| {
                entry.goal.scope.storage == peer.storage
                    && entry.goal.scope.process_session_id == peer.process_session_id
                    && entry.goal.scope.creation_id == peer.creation_id
                    && entry.goal.scope.configuration.current == *configuration
                    && entry.goal.scope.configuration.previous.as_ref()
                        == if configuration == current {
                            None
                        } else {
                            Some(current)
                        }
                    && entry.goal.desired == *desired
            })
        };
        if !selected(
            current,
            source,
            &ReplicaGoalKind::Reconfigure {
                role: ReplicaRole::Primary,
                catch_up: Some(mode),
            },
        )
        .is_some_and(achieved)
        {
            return Ok(Some(action(ReconcileKind::Waiting)));
        }
        for member in intermediate
            .members
            .iter()
            .filter(|member| member.identity.replica_id != current.primary_id)
        {
            let peer = peers[&member.identity.replica_id];
            let desired = ReplicaGoalKind::Reconfigure {
                role: ReplicaRole::ActiveSecondary,
                catch_up: None,
            };
            if let Some(entry) = selected(&intermediate, peer, &desired) {
                if !achieved(entry) {
                    return Ok(Some(action(ReconcileKind::Waiting)));
                }
            } else {
                status.replica_goals.push(goal(
                    peer,
                    configuration_for(
                        member.identity.replica_id,
                        &intermediate,
                        Some(current),
                        peers,
                    )?,
                    desired,
                    &status.replica_goals,
                )?);
                return self.persist_goals(raw, status).await.map(Some);
            }
        }
        let desired = ReplicaGoalKind::Reconfigure {
            role: ReplicaRole::ActiveSecondary,
            catch_up: Some(mode),
        };
        if let Some(entry) = selected(&handoff, source, &desired) {
            if !achieved(entry) {
                return Ok(Some(action(ReconcileKind::Waiting)));
            }
        } else {
            let mut configuration =
                configuration_for(current.primary_id, &handoff, Some(current), peers)?;
            for peer in &mut configuration.replicas {
                peer.must_catchup = peer.identity.replica_id == target;
            }
            status
                .replica_goals
                .push(goal(source, configuration, desired, &status.replica_goals)?);
            return self.persist_goals(raw, status).await.map(Some);
        }
        let transfer_position = status
            .replica_goals
            .iter()
            .position(|entry| completed_source_handoff(entry, source, current, &handoff));
        let completed_transfer = transfer_position.is_some();
        if std::env::var_os("KUBERIC_M3_ELECTION_DIAGNOSTICS").is_some() {
            let transfer = selected(
                &handoff,
                source,
                &ReplicaGoalKind::Reconfigure {
                    role: ReplicaRole::ActiveSecondary,
                    catch_up: Some(mode),
                },
            );
            eprintln!(
                "M3 handoff witness: eligible={completed_transfer} outcome={:?} source_goal={:?} publication={:?} access={:?}/{:?}",
                transfer
                    .and_then(|entry| entry.outcome.as_ref())
                    .map(|outcome| (
                        &outcome.completed_handoff,
                        outcome.host_settled,
                        outcome.receipts,
                        &outcome.selected_call,
                        &outcome.status
                    )),
                transfer
                    .and_then(|entry| source
                        .goals
                        .iter()
                        .find(|observed| observed.goal == entry.goal))
                    .map(|outcome| (
                        &outcome.completed_handoff,
                        outcome.host_settled,
                        outcome.receipts
                    )),
                source.publication.as_ref().map(|publication| (
                    publication.generation,
                    publication.role,
                    publication.stage,
                    transfer.is_some_and(|entry| publication.scope == entry.goal.scope)
                )),
                source.read_status,
                source.write_status
            );
        }
        let desired = ReplicaGoalKind::Reconfigure {
            role: ReplicaRole::Primary,
            catch_up: Some(PublicCatchUpMode::WriteQuorum),
        };
        if selected(&next, peers[&target], &desired).is_some() {
            // Existing election continuation finishes only this frozen selection.
            return Ok(None);
        }
        if source.native_lifecycle.is_some() {
            if !completed_transfer {
                return Ok(Some(action(ReconcileKind::Waiting)));
            }
            for member in &current.members {
                let peer = peers[&member.identity.replica_id];
                let mut configuration =
                    configuration_for(member.identity.replica_id, &next, Some(current), peers)?;
                configuration.replicas.clear();
                let inspected = status.replica_goals.iter().rev().find(|entry| {
                    entry.goal.scope.storage == peer.storage
                        && entry.goal.scope.process_session_id == peer.process_session_id
                        && entry.goal.scope.creation_id == peer.creation_id
                        && entry.goal.scope.configuration.current == next
                        && entry.goal.scope.configuration.previous.as_ref() == Some(current)
                        && matches!(entry.goal.desired, ReplicaGoalKind::Discover)
                });
                if let Some(inspected) = inspected {
                    if !achieved(inspected)
                        || inspected
                            .native_status()
                            .map_err(invalid)?
                            .is_none_or(|status| {
                                !election::inspection_matches(&inspected.goal, &status)
                            })
                    {
                        return Ok(Some(action(ReconcileKind::Waiting)));
                    }
                } else {
                    status.replica_goals.push(goal(
                        peer,
                        configuration,
                        ReplicaGoalKind::Discover,
                        &status.replica_goals,
                    )?);
                    return self.persist_goals(raw, status).await.map(Some);
                }
            }
        }
        let mut sampled = BTreeMap::new();
        let mut quiescent_source = false;
        for member in &current.members {
            let observed = self
                .sample_recovery(raw, peers[&member.identity.replica_id])
                .await?;
            if std::env::var_os("KUBERIC_M3_ELECTION_DIAGNOSTICS").is_some() {
                eprintln!(
                    "M3 handoff sample: replica={} transfer={completed_transfer} progress={:?} capability={:?}",
                    member.identity.replica_id, observed.progress, observed.capability
                );
            }
            if observed.progress.value().is_none_or(|lsn| lsn < 0)
                || observed.capability.value().is_none_or(|lsn| lsn < 0)
            {
                let unavailable = |result: &ProgressQueryResult| {
                    matches!(
                        result,
                        ProgressQueryResult::NotReady | ProgressQueryResult::Value(INVALID_LSN)
                    )
                };
                if completed_transfer
                    && unavailable(&observed.progress)
                    && unavailable(&observed.capability)
                {
                    if member.identity.replica_id == current.primary_id {
                        // Transfer receipts permit the frozen promotion attempt,
                        // not a sample, live vote or a serving grant from this source.
                        quiescent_source = true;
                        sampled.insert(member.identity.replica_id, observed);
                        continue;
                    }
                    let peer = peers[&member.identity.replica_id];
                    let initialize = ReplicaGoalKind::Reconfigure {
                        role: ReplicaRole::ActiveSecondary,
                        catch_up: None,
                    };
                    let prepared = selected(&intermediate, peer, &initialize).filter(|entry| {
                        achieved(entry)
                            && entry
                                .outcome
                                .as_ref()
                                .is_some_and(|outcome| outcome.host_settled)
                    });
                    if prepared.is_none() {
                        return Ok(Some(action(ReconcileKind::Waiting)));
                    }
                    let attempted = transfer_position.is_some_and(|position| {
                        status.replica_goals[position + 1..].iter().any(|entry| {
                            entry.goal.scope.storage == peer.storage
                                && entry.goal.scope.process_session_id == peer.process_session_id
                                && entry.goal.scope.creation_id == peer.creation_id
                                && entry.goal.scope.configuration.current == intermediate
                                && entry.goal.scope.configuration.previous.as_ref() == Some(current)
                                && entry.goal.desired == initialize
                        })
                    });
                    if !attempted {
                        let initialization = goal(
                            peer,
                            configuration_for(
                                member.identity.replica_id,
                                &intermediate,
                                Some(current),
                                peers,
                            )?,
                            initialize,
                            &status.replica_goals,
                        )?;
                        status.replica_goals.push(initialization);
                        return self.persist_goals(raw, status).await.map(Some);
                    }
                }
                return Ok(Some(action(ReconcileKind::Waiting)));
            }
            sampled.insert(member.identity.replica_id, observed);
        }
        let sampled = sampled.iter().map(|(id, peer)| (*id, peer)).collect();
        let mut configuration = configuration_for(target, &next, Some(current), &sampled)?;
        if quiescent_source {
            let source = configuration
                .replicas
                .iter_mut()
                .find(|peer| peer.identity.replica_id == current.primary_id)
                .ok_or_else(|| invalid("completed handoff lost its original source"))?;
            source.current_progress = INVALID_LSN;
            source.catch_up_capability = INVALID_LSN;
            source.is_up = false;
            source.is_ready = false;
            source.must_catchup = false;
        }
        status.replica_goals.push(goal(
            peers[&target],
            configuration,
            desired,
            &status.replica_goals,
        )?);
        self.persist_goals(raw, status).await.map(Some)
    }

    async fn prepare_selected_primary_dependencies(
        &self,
        raw: &RawObservation,
        status: &KubericSetStatus,
        current: &ConfigurationDescriptor,
        peers: &BTreeMap<ReplicaId, &ReplicaAgentSnapshot>,
    ) -> Result<Option<ReconcileAction>> {
        let selected = status.replica_goals.iter().find(|entry| {
            entry.outcome.is_none()
                && entry.goal.scope.configuration.previous.as_ref() == Some(current)
                && entry.goal.scope.configuration.current != *current
                && entry.goal.scope.epoch > current.epoch
                && matches!(
                    entry.goal.desired,
                    ReplicaGoalKind::Reconfigure {
                        role: ReplicaRole::Primary,
                        ..
                    }
                )
        });
        let Some(selected) = selected else {
            return Ok(None);
        };
        let next = &selected.goal.scope.configuration.current;
        let mut status = status.clone();
        for member in next
            .members
            .iter()
            .filter(|member| member.identity.replica_id != next.primary_id)
        {
            let Some(peer) = peers.get(&member.identity.replica_id) else {
                return Ok(Some(action(ReconcileKind::Waiting)));
            };
            if matches!(
                selected.goal.desired,
                ReplicaGoalKind::Reconfigure { catch_up: None, .. }
            ) && selected
                .goal
                .scope
                .configuration
                .replicas
                .iter()
                .any(|description| description.identity == member.identity && !description.is_ready)
            {
                continue;
            }
            if status.replica_goals.iter().any(|entry| {
                matches!(entry.goal.desired, ReplicaGoalKind::Discover)
                    && entry.goal.scope.configuration.current == *next
                    && entry.goal.scope.storage == peer.storage
                    && entry.goal.scope.process_session_id == peer.process_session_id
                    && entry
                        .native_status()
                        .ok()
                        .flatten()
                        .is_some_and(|native| native.input.state == NativeReplicaState::StandBy)
            }) {
                // Opened inspection cannot initialize an Active receiver.
                // Original-member Idle/Build belongs to source establishment.
                continue;
            }
            if !current
                .members
                .iter()
                .any(|old| old.identity == member.identity)
                && peer
                    .publication
                    .as_ref()
                    .is_some_and(|publication| publication.role == ReplicaRole::IdleSecondary)
            {
                continue;
            }
            if !status.replica_goals.iter().any(|entry| {
                entry.goal.scope.storage == peer.storage
                    && entry.goal.scope.process_session_id == peer.process_session_id
                    && entry.goal.scope.creation_id == peer.creation_id
                    && entry.goal.scope.configuration.current == *next
                    && entry.goal.scope.configuration.previous.as_ref() == Some(current)
                    && matches!(entry.goal.desired, ReplicaGoalKind::Reconfigure { role, .. } if role == member.role)
            }) {
                status.replica_goals.push(goal(
                    peer, configuration_for(member.identity.replica_id, next, Some(current), peers)?,
                    ReplicaGoalKind::Reconfigure { role: member.role, catch_up: None },
                    &status.replica_goals,
                )?);
                return self.persist_goals(raw, status).await.map(Some);
            }
        }
        Ok(None)
    }

    async fn reconcile_returned_primary(
        &self,
        raw: &RawObservation,
        status: &KubericSetStatus,
        current: &ConfigurationDescriptor,
        peers: &BTreeMap<ReplicaId, &ReplicaAgentSnapshot>,
    ) -> Result<Option<ReconcileAction>> {
        for terminal in status.replica_goals.iter().filter(|entry| {
            achieved(entry)
                && matches!(entry.goal.desired, ReplicaGoalKind::Abort)
                && entry.goal.scope.epoch < current.epoch
                && replacement_predecessor(
                    status,
                    current,
                    entry.goal.scope.storage.replica.replica_id,
                )
                .as_ref()
                    == Some(&entry.goal.scope.storage)
        }) {
            if let Some(pod) = raw.pods.iter().find(|pod| {
                pod.uid().as_deref()
                    == Some(terminal.goal.scope.storage.replica.instance_id.as_str())
                    && pod.metadata.deletion_timestamp.is_none()
            }) {
                let ReplicaStorageIdentity::Persistent { pod_uid, .. } =
                    &terminal.goal.scope.storage.storage
                else {
                    return Err(invalid("former Primary settlement lacks persistent owner"));
                };
                self.delete_replica_scaffolding(
                    raw,
                    Some(&pod.name_any()),
                    Some(pod_uid),
                    None,
                    None,
                )
                .await?;
                return Ok(Some(action(ReconcileKind::Applied)));
            }
        }
        let Some(returned) = peers.values().find(|peer| {
            peer.storage.replica.replica_id.value() <= i64::from(raw.set.spec.replicas)
                && replacement_predecessor(status, current, peer.storage.replica.replica_id)
                    .as_ref()
                    == Some(&peer.storage)
                && peer.publication.as_ref().is_some_and(|publication| {
                    publication.stage == ReplicaPublicationStage::AchievedScope
                        && publication.role == ReplicaRole::Primary
                        && publication.scope.epoch < current.epoch
                })
        }) else {
            return Ok(None);
        };
        let Some(primary) = peers.get(&current.primary_id) else {
            return Ok(Some(action(ReconcileKind::Waiting)));
        };
        let old_progress = self.sample_recovery(raw, returned).await?;
        let primary_progress = self.sample_recovery(raw, primary).await?;
        if old_progress.progress.value().is_none_or(|lsn| lsn < 0)
            || primary_progress
                .progress
                .value()
                .is_none_or(|lsn| lsn < old_progress.progress.value().unwrap_or(i64::MAX))
        {
            return Ok(Some(action(ReconcileKind::Waiting)));
        }
        let mut next = status.clone();
        next.replica_goals.push(goal(
            returned,
            returned
                .publication
                .as_ref()
                .unwrap()
                .scope
                .configuration
                .clone(),
            ReplicaGoalKind::Abort,
            &status.replica_goals,
        )?);
        self.persist_goals(raw, next).await.map(Some)
    }

    async fn reconcile_quorum_healing(
        &self,
        raw: &RawObservation,
        status: &KubericSetStatus,
        current: &ConfigurationDescriptor,
        peers: &BTreeMap<ReplicaId, &ReplicaAgentSnapshot>,
    ) -> Result<Option<ReconcileAction>> {
        let topology = status.public_v1_topology.as_ref().unwrap();
        let Some(primary) = peers.get(&current.primary_id).filter(|peer| {
            !peer.withdrawn
                && topology
                    .sessions
                    .contains(&endpoint(peer, ReplicaRole::Primary))
        }) else {
            return Ok(None);
        };
        if topology.pending_switchover.is_some() {
            return Ok(None);
        }
        let next = status.replica_goals.iter().find_map(|entry| {
            (entry.goal.scope.configuration.previous.as_ref() == Some(current)
                && entry.goal.scope.epoch > current.epoch
                && entry.goal.scope.configuration.current.primary_id == current.primary_id
                && entry.goal.scope.configuration.current.members == current.members
                && entry.goal.scope.configuration.current.write_quorum == current.write_quorum
                && matches!(
                    entry.goal.desired,
                    ReplicaGoalKind::Reconfigure {
                        role: ReplicaRole::ActiveSecondary,
                        ..
                    }
                ))
            .then(|| entry.goal.scope.configuration.current.clone())
        });
        let Some(next) = next else {
            let native_quorum_lost = primary.write_status == AccessStatus::NoWriteQuorum
                && primary.publication.as_ref().is_some_and(|publication| {
                    publication.stage == ReplicaPublicationStage::AchievedScope
                        && publication.role == ReplicaRole::Primary
                        && publication.scope.configuration.current == *current
                        && publication.scope.configuration.previous.is_none()
                })
                && current.members.iter().all(|member| {
                    raw.pods.iter().any(|pod| {
                        pod.uid().as_deref() == Some(member.identity.instance_id.as_str())
                            && pod.metadata.deletion_timestamp.is_none()
                    })
                });
            let available = current
                .members
                .iter()
                .filter(|member| {
                    peers.get(&member.identity.replica_id).is_some_and(|peer| {
                        !peer.withdrawn
                            && topology.sessions.contains(&endpoint(peer, member.role))
                            && peer.publication.as_ref().is_some_and(|publication| {
                                publication.stage == ReplicaPublicationStage::AchievedScope
                                    && publication.role == member.role
                                    && publication.scope.configuration.current == *current
                                    && publication.scope.configuration.previous.is_none()
                            })
                    })
                })
                .count();
            if available >= current.write_quorum as usize && !native_quorum_lost {
                return Ok(None);
            }
            let missing = current.members.iter().any(|member| {
                member.identity.replica_id != current.primary_id
                    && !peers
                        .get(&member.identity.replica_id)
                        .is_some_and(|peer| peer.storage.replica == member.identity)
                    && raw.pods.iter().any(|pod| {
                        pod.uid().as_deref() == Some(member.identity.instance_id.as_str())
                            && pod.metadata.deletion_timestamp.is_none()
                    })
            });
            if !missing && !native_quorum_lost {
                return Ok(None);
            }
            let mut next = current.clone();
            next.epoch.configuration_number = next
                .epoch
                .configuration_number
                .checked_add(1)
                .ok_or_else(|| invalid("configuration epoch exhausted"))?;
            next.configuration_id = next.expected_id();
            let mut status = status.clone();
            for member in current
                .members
                .iter()
                .filter(|member| member.identity.replica_id != current.primary_id)
            {
                let session = topology
                    .sessions
                    .iter()
                    .find(|session| session.identity == member.identity)
                    .ok_or_else(|| invalid("recovery member lacks retained endpoint"))?;
                let old = status
                    .replica_goals
                    .iter()
                    .rev()
                    .find(|entry| {
                        achieved(entry)
                            && entry.goal.scope.storage == session.storage_incarnation
                            && entry.goal.scope.process_session_id == session.process_session_id
                            && entry.goal.scope.creation_id == session.replicator_creation_id
                            && matches!(
                                entry.goal.desired,
                                ReplicaGoalKind::Reconfigure {
                                    role: ReplicaRole::ActiveSecondary,
                                    ..
                                }
                            )
                    })
                    .ok_or_else(|| invalid("recovery member lacks achieved retained goal"))?;
                let mut selected = old.goal.clone();
                selected.revision = status
                    .replica_goals
                    .iter()
                    .filter(|entry| entry.goal.scope.storage.replica == member.identity)
                    .map(|entry| entry.goal.revision)
                    .max()
                    .unwrap_or(selected.revision)
                    .checked_add(1)
                    .ok_or_else(|| invalid("goal revision exhausted"))?;
                selected.operation_id = OperationId::new(format!(
                    "goal-{}-{}-{}",
                    member.identity.replica_id,
                    selected.scope.process_session_id,
                    selected.revision,
                ));
                selected.scope.epoch = next.epoch;
                selected.scope.configuration.current = next.clone();
                selected.scope.configuration.previous = Some(current.clone());
                for description in &mut selected.scope.configuration.replicas {
                    let observed = peers.get(&description.identity.replica_id).filter(|peer| {
                        peer.storage == description.storage_incarnation
                            && peer.process_session_id == description.process_session_id
                            && peer.creation_id
                                == topology
                                    .sessions
                                    .iter()
                                    .find(|session| session.identity == description.identity)
                                    .map(|session| session.replicator_creation_id.clone())
                                    .unwrap_or_default()
                    });
                    description.is_up = observed.is_some();
                    description.is_ready = observed.is_some();
                    description.current_progress = INVALID_LSN;
                    description.catch_up_capability = INVALID_LSN;
                    description.must_catchup = false;
                }
                selected.desired = ReplicaGoalKind::Reconfigure {
                    role: ReplicaRole::ActiveSecondary,
                    catch_up: None,
                };
                status
                    .replica_goals
                    .push(RetainedReplicaGoal::new(selected).map_err(invalid)?);
            }
            return self.persist_goals(raw, status).await.map(Some);
        };
        let mut sampled = BTreeMap::new();
        for member in &next.members {
            let Some(peer) = peers.get(&member.identity.replica_id) else {
                return Ok(Some(action(ReconcileKind::Waiting)));
            };
            let observed = self.sample_recovery(raw, peer).await?;
            if observed.progress.value().is_none_or(|lsn| lsn < 0)
                || observed.capability.value().is_none_or(|lsn| lsn < 0)
            {
                return Ok(Some(action(ReconcileKind::Waiting)));
            }
            sampled.insert(member.identity.replica_id, observed);
        }
        let sampled = sampled
            .iter()
            .map(|(id, peer)| (*id, peer))
            .collect::<BTreeMap<_, _>>();
        let mut status = status.clone();
        let selected = |entry: &RetainedReplicaGoal, id, previous| {
            achieved(entry)
                && entry.goal.scope.storage == sampled[&id].storage
                && entry.goal.scope.process_session_id == sampled[&id].process_session_id
                && entry.goal.scope.creation_id == sampled[&id].creation_id
                && entry.goal.scope.configuration.current == next
                && entry.goal.scope.configuration.previous.as_ref() == previous
                && matches!(entry.goal.desired, ReplicaGoalKind::Reconfigure { .. })
        };
        if !status
            .replica_goals
            .iter()
            .any(|entry| selected(entry, current.primary_id, Some(current)))
        {
            status.replica_goals.push(goal(
                primary,
                configuration_for(current.primary_id, &next, Some(current), &sampled)?,
                ReplicaGoalKind::Reconfigure {
                    role: ReplicaRole::Primary,
                    catch_up: Some(PublicCatchUpMode::All),
                },
                &status.replica_goals,
            )?);
            return self.persist_goals(raw, status).await.map(Some);
        }
        for member in &next.members {
            let id = member.identity.replica_id;
            if !status
                .replica_goals
                .iter()
                .any(|entry| selected(entry, id, None))
            {
                status.replica_goals.push(goal(
                    sampled[&id],
                    configuration_for(id, &next, None, &sampled)?,
                    ReplicaGoalKind::Reconfigure {
                        role: member.role,
                        catch_up: (member.role == ReplicaRole::Primary)
                            .then_some(PublicCatchUpMode::All),
                    },
                    &status.replica_goals,
                )?);
                return self.persist_goals(raw, status).await.map(Some);
            }
        }
        let sessions = next
            .members
            .iter()
            .map(|member| endpoint(sampled[&member.identity.replica_id], member.role))
            .collect::<Vec<_>>();
        let mut next_topology =
            PublicV1TopologyState::from_configuration(&next, &sessions).map_err(invalid)?;
        next_topology.completed_switchover_request_id =
            topology.completed_switchover_request_id.clone();
        status.public_v1_topology = Some(next_topology);
        self.persist_goals(raw, status).await.map(Some)
    }

    fn primary_dependencies_ready(
        goal: &ReplicaGoal,
        current: Option<&ConfigurationDescriptor>,
        peers: &BTreeMap<ReplicaId, &ReplicaAgentSnapshot>,
    ) -> bool {
        if !matches!(
            goal.desired,
            ReplicaGoalKind::Reconfigure {
                role: ReplicaRole::Primary,
                ..
            }
        ) || current.is_none_or(|current| {
            goal.scope.configuration.previous.as_ref() != Some(current)
                || goal.scope.configuration.current == *current
                || goal.scope.epoch <= current.epoch
        }) {
            return true;
        }
        if matches!(
            goal.desired,
            ReplicaGoalKind::Reconfigure { catch_up: None, .. }
        ) {
            return peers
                .get(&goal.scope.storage.replica.replica_id)
                .and_then(|peer| peer.native_status.as_ref())
                .is_some_and(|status| {
                    let input = &status.input.scope;
                    input.history == goal.scope.history
                        && input.storage == goal.scope.storage
                        && input.process_session_id == goal.scope.process_session_id
                        && input.creation_id == goal.scope.creation_id
                        && input.epoch == goal.scope.epoch
                        && input.configuration.current == goal.scope.configuration.current
                        && input.configuration.previous == goal.scope.configuration.previous
                });
        }
        goal.scope
            .configuration
            .current
            .members
            .iter()
            .filter(|member| {
                member.identity.replica_id != goal.scope.configuration.current.primary_id
            })
            .all(|member| {
                peers.get(&member.identity.replica_id).is_some_and(|peer| {
                    peer.storage.replica == member.identity
                        && goal.scope.configuration.replicas.iter().any(|selected| {
                            selected.identity == member.identity
                                && selected.storage_incarnation == peer.storage
                                && selected.process_session_id == peer.process_session_id
                        })
                        && peer.publication.as_ref().is_some_and(|publication| {
                            publication.stage == ReplicaPublicationStage::AchievedScope
                                && ((publication.role == member.role
                                    && publication.scope.configuration.current
                                        == goal.scope.configuration.current
                                    && publication.scope.configuration.previous
                                        == goal.scope.configuration.previous)
                                    || (publication.role == ReplicaRole::IdleSecondary
                                        && current.is_some_and(|current| {
                                            !current
                                                .members
                                                .iter()
                                                .any(|old| old.identity == member.identity)
                                        })))
                        })
                })
            })
    }

    async fn reconcile_survivor_election(
        &self,
        raw: &RawObservation,
        status: &KubericSetStatus,
        current: &ConfigurationDescriptor,
        peers: &BTreeMap<ReplicaId, &ReplicaAgentSnapshot>,
    ) -> Result<Option<ReconcileAction>> {
        let selected = status
            .replica_goals
            .iter()
            .rev()
            .find(|entry| {
                entry.goal.scope.configuration.previous.as_ref() == Some(current)
                    && entry.goal.scope.configuration.current.primary_id != current.primary_id
                    && matches!(
                        entry.goal.desired,
                        ReplicaGoalKind::Reconfigure {
                            role: ReplicaRole::Primary,
                            ..
                        }
                    )
            })
            .cloned();
        let mut status = status.clone();
        if selected.is_none() {
            #[cfg(feature = "test-util")]
            if std::env::var_os("KUBERIC_M3_ELECTION_DIAGNOSTICS").is_some() {
                eprintln!(
                    "ordinary election observation: primary={}, epoch={:?}, peers={:?}",
                    current.primary_id,
                    current.epoch,
                    peers
                        .iter()
                        .map(|(id, peer)| (
                            id,
                            peer.withdrawn,
                            peer.publication.as_ref().map(|publication| (
                                publication.role,
                                publication.stage,
                                publication.scope.epoch,
                            )),
                        ))
                        .collect::<Vec<_>>()
                );
            }
            if peers
                .get(&current.primary_id)
                .is_some_and(|peer| !peer.withdrawn && peer.publication.is_some())
            {
                if status
                    .public_v1_topology
                    .as_ref()
                    .is_some_and(|topology| topology.unavailable_primary_id.is_some())
                {
                    let topology = status.public_v1_topology.as_mut().unwrap();
                    topology.unavailable_primary_id = None;
                    topology.unavailable_since_unix_seconds = None;
                    return self.persist_goals(raw, status).await.map(Some);
                }
                return Ok(None);
            }
            let topology = status
                .public_v1_topology
                .as_mut()
                .ok_or_else(|| invalid("election lacks retained topology"))?;
            if topology.unavailable_primary_id != Some(current.primary_id) {
                topology.unavailable_primary_id = Some(current.primary_id);
                topology.unavailable_since_unix_seconds = Some(raw.now_unix_seconds);
                return self.persist_goals(raw, status).await.map(Some);
            }
            let delay = i64::try_from(raw.set.spec.failover_delay_seconds).unwrap_or(i64::MAX);
            if raw.now_unix_seconds
                < topology
                    .unavailable_since_unix_seconds
                    .unwrap_or(raw.now_unix_seconds)
                    .saturating_add(delay)
            {
                return Ok(Some(action(ReconcileKind::Waiting)));
            }
            return self
                .reconcile_inspection_round(raw, &status, current, peers)
                .await;
        }
        let selected = selected.unwrap();
        let next = &selected.goal.scope.configuration.current;
        if !achieved(&selected) {
            return Ok(Some(action(ReconcileKind::Waiting)));
        }
        for member in &next.members {
            let Some(peer) = peers.get(&member.identity.replica_id) else {
                return Ok(Some(action(ReconcileKind::Waiting)));
            };
            if !status.replica_goals.iter().any(|entry| {
                achieved(entry)
                    && entry.goal.scope.storage == peer.storage
                    && entry.goal.scope.process_session_id == peer.process_session_id
                    && entry.goal.scope.creation_id == peer.creation_id
                    && entry.goal.scope.configuration.current == *next
                    && entry.goal.scope.configuration.previous.is_none()
                    && matches!(entry.goal.desired, ReplicaGoalKind::Reconfigure { role, .. } if role == member.role)
            }) {
                status.replica_goals.push(goal(
                    peer, configuration_for(member.identity.replica_id, next, None, peers)?,
                    ReplicaGoalKind::Reconfigure { role: member.role,
                        catch_up: (member.role == ReplicaRole::Primary).then_some(PublicCatchUpMode::WriteQuorum) },
                    &status.replica_goals,
                )?);
                return self.persist_goals(raw, status).await.map(Some);
            }
        }
        let sessions = next
            .members
            .iter()
            .map(|member| endpoint(peers[&member.identity.replica_id], member.role))
            .collect::<Vec<_>>();
        let mut topology =
            PublicV1TopologyState::from_configuration(next, &sessions).map_err(invalid)?;
        let previous = status.public_v1_topology.as_ref().unwrap();
        topology.completed_switchover_request_id = previous.completed_switchover_request_id.clone();
        if let Some(request) = previous.pending_switchover.as_ref()
            && i64::from(request.target_replica_id) == next.primary_id.value()
        {
            topology.completed_switchover_request_id = Some(request.request_id.clone());
        } else if previous.pending_switchover.is_some() {
            topology.pending_switchover = previous.pending_switchover.clone();
        } else if let Some(request) = raw.set.spec.switchover.as_ref()
            && i64::from(request.target_replica_id) == next.primary_id.value()
        {
            topology.completed_switchover_request_id = Some(request.request_id.clone());
        }
        status.public_v1_topology = Some(topology);
        self.persist_goals(raw, status).await.map(Some)
    }

    async fn sample_recovery(
        &self,
        raw: &RawObservation,
        peer: &ReplicaAgentSnapshot,
    ) -> Result<ReplicaAgentSnapshot> {
        let response = self
            .agents
            .get_snapshot(
                &peer_address(raw, &peer.storage.replica)?,
                &self.bearer_token,
                kuberic_runtime::control::proto::ReplicaSnapshotRequest {
                    protocol_version: ORDINARY_ACTION_FORMAT,
                    resource_uid: peer.history.resource_uid.to_string(),
                    target: Some(peer.storage.replica.clone().into()),
                    expected_process_session_id: peer.process_session_id.to_string(),
                    sample_native_progress: true,
                },
            )
            .await
            .map_err(crate::cluster_api::map_agent_effect_error)?;
        if response.protocol_version != ORDINARY_ACTION_FORMAT {
            return Err(invalid("recovery sample changed format"));
        }
        let ReplicaAgentQueryResult::Initialized(sample) =
            serde_json::from_slice(&response.canonical_json)
                .map_err(|error| invalid(error.to_string()))?
        else {
            return Err(invalid("recovery sample lost initialized storage"));
        };
        if sample.identity != peer.identity
            || sample.format != ORDINARY_ACTION_FORMAT
            || sample.history != peer.history
            || sample.storage != peer.storage
            || sample.process_session_id != peer.process_session_id
            || sample.creation_id != peer.creation_id
            || sample.publication != peer.publication
        {
            return Err(ControllerError::ObservationStale);
        }
        Ok(*sample)
    }

    async fn reconcile_primary_reopen(
        &self,
        raw: &RawObservation,
        status: &KubericSetStatus,
        current: &ConfigurationDescriptor,
        peers: &BTreeMap<ReplicaId, &ReplicaAgentSnapshot>,
    ) -> Result<Option<ReconcileAction>> {
        let Some(primary) = peers.get(&current.primary_id) else {
            return Ok(None);
        };
        let Some(topology) = status.public_v1_topology.as_ref() else {
            return Ok(None);
        };
        let Some(prior) = topology
            .sessions
            .iter()
            .find(|session| session.role == ReplicaRole::Primary)
        else {
            return Err(invalid("retained topology lost its primary endpoint"));
        };
        if current.members.len() == 1 || *prior == endpoint(primary, ReplicaRole::Primary) {
            return Ok(None);
        }
        if prior.storage_incarnation != primary.storage || prior.identity != primary.storage.replica
        {
            return Ok(Some(action(ReconcileKind::Waiting)));
        }
        let provisional_epoch = Epoch::new(
            current.epoch.data_loss_number,
            current
                .epoch
                .configuration_number
                .checked_add(1)
                .ok_or_else(|| invalid("configuration epoch exhausted"))?,
        );
        let epoch = Epoch::new(
            current.epoch.data_loss_number,
            provisional_epoch
                .configuration_number
                .checked_add(1)
                .ok_or_else(|| invalid("configuration epoch exhausted"))?,
        );
        let provisional = ConfigurationDescriptor::new(
            provisional_epoch,
            current.primary_id,
            vec![ConfigurationMember {
                identity: primary.storage.replica.clone(),
                role: ReplicaRole::Primary,
            }],
            1,
        );
        let next = ConfigurationDescriptor::new(
            epoch,
            current.primary_id,
            current.members.clone(),
            current.write_quorum,
        );
        let mut status = status.clone();
        if !status.replica_goals.iter().any(|entry| {
            entry.goal.scope.storage == primary.storage
                && entry.goal.scope.process_session_id == primary.process_session_id
                && entry.goal.scope.creation_id == primary.creation_id
                && entry.goal.scope.configuration.current == provisional
                && matches!(
                    entry.goal.desired,
                    ReplicaGoalKind::Prepare {
                        role: ReplicaRole::Primary,
                        ..
                    }
                )
        }) {
            status.replica_goals.push(goal(
                primary,
                ReplicaConfigurationScope {
                    current: provisional,
                    previous: None,
                    replicas: Vec::new(),
                },
                ReplicaGoalKind::Prepare {
                    role: ReplicaRole::Primary,
                    rebuild: None,
                },
                &status.replica_goals,
            )?);
            return self.persist_goals(raw, status).await.map(Some);
        }
        if current.members.iter().any(|member| {
            !peers.get(&member.identity.replica_id).is_some_and(|peer| {
                peer.storage.replica == member.identity
                    && peer.publication.as_ref().is_some_and(|publication| {
                        publication.stage == ReplicaPublicationStage::AchievedScope
                    })
            })
        }) {
            return Ok(Some(action(ReconcileKind::Waiting)));
        }
        let mut sampled = BTreeMap::new();
        for member in &current.members {
            sampled.insert(
                member.identity.replica_id,
                self.sample_recovery(raw, peers[&member.identity.replica_id])
                    .await?,
            );
        }
        let Some(progress) = sampled[&current.primary_id]
            .progress
            .value()
            .filter(|value| *value >= 0)
        else {
            return Ok(Some(action(ReconcileKind::Waiting)));
        };
        if sampled.values().any(|peer| {
            peer.progress
                .value()
                .is_none_or(|value| value < 0 || value > progress)
        }) {
            return Ok(Some(action(ReconcileKind::Waiting)));
        }
        let sampled = sampled
            .iter()
            .map(|(id, peer)| (*id, peer))
            .collect::<BTreeMap<_, _>>();
        let selected = |entry: &RetainedReplicaGoal,
                        peer: &ReplicaAgentSnapshot,
                        previous: Option<&ConfigurationDescriptor>| {
            entry.goal.scope.storage == peer.storage
                && entry.goal.scope.process_session_id == peer.process_session_id
                && entry.goal.scope.creation_id == peer.creation_id
                && entry.goal.scope.configuration.current == next
                && entry.goal.scope.configuration.previous.as_ref() == previous
                && matches!(entry.goal.desired, ReplicaGoalKind::Reconfigure { .. })
        };
        for member in next
            .members
            .iter()
            .filter(|member| member.identity.replica_id != next.primary_id)
        {
            let peer = sampled[&member.identity.replica_id];
            if !status
                .replica_goals
                .iter()
                .any(|entry| selected(entry, peer, Some(current)))
            {
                status.replica_goals.push(goal(
                    peer,
                    configuration_for(member.identity.replica_id, &next, Some(current), &sampled)?,
                    ReplicaGoalKind::Reconfigure {
                        role: member.role,
                        catch_up: None,
                    },
                    &status.replica_goals,
                )?);
                return self.persist_goals(raw, status).await.map(Some);
            }
        }
        for previous in [Some(current), None] {
            let primary = sampled[&next.primary_id];
            if !status
                .replica_goals
                .iter()
                .any(|entry| selected(entry, primary, previous))
            {
                status.replica_goals.push(goal(
                    primary,
                    configuration_for(next.primary_id, &next, previous, &sampled)?,
                    ReplicaGoalKind::Reconfigure {
                        role: ReplicaRole::Primary,
                        catch_up: Some(PublicCatchUpMode::All),
                    },
                    &status.replica_goals,
                )?);
                return self.persist_goals(raw, status).await.map(Some);
            }
        }
        for member in next
            .members
            .iter()
            .filter(|member| member.identity.replica_id != next.primary_id)
        {
            let peer = sampled[&member.identity.replica_id];
            if !status
                .replica_goals
                .iter()
                .any(|entry| selected(entry, peer, None))
            {
                status.replica_goals.push(goal(
                    peer,
                    configuration_for(member.identity.replica_id, &next, None, &sampled)?,
                    ReplicaGoalKind::Reconfigure {
                        role: member.role,
                        catch_up: None,
                    },
                    &status.replica_goals,
                )?);
                return self.persist_goals(raw, status).await.map(Some);
            }
        }
        if sampled.values().any(|peer| {
            !peer.publication.as_ref().is_some_and(|publication| {
                publication.stage == ReplicaPublicationStage::AchievedScope
                    && publication.scope.configuration.current == next
                    && publication.scope.configuration.previous.is_none()
            })
        }) {
            return Ok(Some(action(ReconcileKind::Waiting)));
        }
        let sessions = next
            .members
            .iter()
            .map(|member| endpoint(sampled[&member.identity.replica_id], member.role))
            .collect::<Vec<_>>();
        status.public_v1_topology =
            Some(PublicV1TopologyState::from_configuration(&next, &sessions).map_err(invalid)?);
        self.persist_goals(raw, status).await.map(Some)
    }

    async fn cleanup_replaced_storage(
        &self,
        raw: &RawObservation,
        status: &KubericSetStatus,
        current: &ConfigurationDescriptor,
    ) -> Result<bool> {
        let mut candidates = BTreeMap::new();
        for entry in status.replica_goals.iter().filter(|entry| achieved(entry)) {
            let Some(previous) = entry.goal.scope.configuration.previous.as_ref() else {
                continue;
            };
            for member in &previous.members {
                if entry
                    .goal
                    .scope
                    .configuration
                    .current
                    .members
                    .iter()
                    .any(|next| next.identity == member.identity)
                    || !current.members.iter().any(|next| {
                        next.identity.replica_id == member.identity.replica_id
                            && next.identity != member.identity
                    })
                {
                    continue;
                }
                if let Some(peer) = entry
                    .goal
                    .scope
                    .configuration
                    .replicas
                    .iter()
                    .find(|peer| peer.identity == member.identity)
                {
                    candidates.insert(member.identity.clone(), peer.storage_incarnation.clone());
                }
            }
        }
        for storage in candidates.values() {
            let ReplicaStorageIdentity::Persistent { pod_uid, pvc_uid } = &storage.storage else {
                continue;
            };
            let pod = raw
                .pods
                .iter()
                .find(|pod| pod.uid().as_deref() == Some(pod_uid.as_str()));
            let pvc = raw
                .pvcs
                .iter()
                .find(|pvc| pvc.uid().as_deref() == Some(pvc_uid.as_str()));
            let service = raw.services.iter().any(|service| {
                service
                    .spec
                    .as_ref()
                    .and_then(|spec| spec.selector.as_ref())
                    .and_then(|selector| selector.get(crate::crd::INSTANCE_LABEL))
                    .is_some_and(|selected| selected == storage.replica.instance_id.as_str())
            });
            if pod.is_some() || pvc.is_some() || service {
                self.delete_replica_scaffolding(
                    raw,
                    pod.map(ResourceExt::name_any).as_deref(),
                    Some(pod_uid),
                    if pod.is_none() {
                        pvc.map(ResourceExt::name_any)
                    } else {
                        None
                    }
                    .as_deref(),
                    if pod.is_none() { Some(pvc_uid) } else { None },
                )
                .await?;
                return Ok(true);
            }
        }
        Ok(false)
    }

    async fn reconcile_missing_secondaries(
        &self,
        raw: &RawObservation,
        status: &KubericSetStatus,
        current: &ConfigurationDescriptor,
        peers: &BTreeMap<ReplicaId, &ReplicaAgentSnapshot>,
    ) -> Result<Option<ReconcileAction>> {
        let absent = current
            .members
            .iter()
            .filter(|member| {
                member.identity.replica_id != current.primary_id
                    && !raw.pods.iter().any(|pod| {
                        pod.uid().as_deref() == Some(member.identity.instance_id.as_str())
                            && pod.metadata.deletion_timestamp.is_none()
                    })
            })
            .map(|member| member.identity.replica_id)
            .collect::<Vec<_>>();
        if absent.is_empty() || !peers.contains_key(&current.primary_id) {
            return Ok(None);
        }
        let members = current
            .members
            .iter()
            .filter(|member| !absent.contains(&member.identity.replica_id))
            .cloned()
            .collect::<Vec<_>>();
        if members.len() < current.write_quorum as usize
            || members.iter().any(|member| {
                !peers
                    .get(&member.identity.replica_id)
                    .is_some_and(|peer| peer.storage.replica == member.identity)
            })
        {
            return Ok(Some(action(ReconcileKind::Waiting)));
        }
        let next = ConfigurationDescriptor::new(
            Epoch::new(
                current.epoch.data_loss_number,
                current
                    .epoch
                    .configuration_number
                    .checked_add(1)
                    .ok_or_else(|| invalid("configuration epoch exhausted"))?,
            ),
            current.primary_id,
            members,
            current.write_quorum,
        );
        let mut status = status.clone();
        let selected = |entry: &RetainedReplicaGoal,
                        peer: &ReplicaAgentSnapshot,
                        previous: Option<&ConfigurationDescriptor>| {
            entry.goal.scope.storage == peer.storage
                && entry.goal.scope.process_session_id == peer.process_session_id
                && entry.goal.scope.creation_id == peer.creation_id
                && entry.goal.scope.configuration.current == next
                && entry.goal.scope.configuration.previous.as_ref() == previous
                && matches!(entry.goal.desired, ReplicaGoalKind::Reconfigure { .. })
        };
        for member in next
            .members
            .iter()
            .filter(|member| member.identity.replica_id != next.primary_id)
        {
            let peer = peers[&member.identity.replica_id];
            if !status
                .replica_goals
                .iter()
                .any(|entry| selected(entry, peer, Some(current)))
            {
                status.replica_goals.push(goal(
                    peer,
                    configuration_for(member.identity.replica_id, &next, Some(current), peers)?,
                    ReplicaGoalKind::Reconfigure {
                        role: member.role,
                        catch_up: None,
                    },
                    &status.replica_goals,
                )?);
                return self.persist_goals(raw, status).await.map(Some);
            }
        }
        let primary = peers[&next.primary_id];
        if !status
            .replica_goals
            .iter()
            .any(|entry| selected(entry, primary, Some(current)))
        {
            status.replica_goals.push(goal(
                primary,
                configuration_for(next.primary_id, &next, Some(current), peers)?,
                ReplicaGoalKind::Reconfigure {
                    role: ReplicaRole::Primary,
                    catch_up: Some(PublicCatchUpMode::WriteQuorum),
                },
                &status.replica_goals,
            )?);
            return self.persist_goals(raw, status).await.map(Some);
        }
        for member in &next.members {
            let peer = peers[&member.identity.replica_id];
            if !status
                .replica_goals
                .iter()
                .any(|entry| selected(entry, peer, None))
            {
                status.replica_goals.push(goal(
                    peer,
                    configuration_for(member.identity.replica_id, &next, None, peers)?,
                    ReplicaGoalKind::Reconfigure {
                        role: member.role,
                        catch_up: (member.role == ReplicaRole::Primary)
                            .then_some(PublicCatchUpMode::WriteQuorum),
                    },
                    &status.replica_goals,
                )?);
                return self.persist_goals(raw, status).await.map(Some);
            }
            if !peer.publication.as_ref().is_some_and(|publication| {
                publication.stage == ReplicaPublicationStage::AchievedScope
                    && publication.role == member.role
                    && publication.scope.configuration.current == next
                    && publication.scope.configuration.previous.is_none()
            }) {
                return Ok(Some(action(ReconcileKind::Waiting)));
            }
        }
        let sessions = next
            .members
            .iter()
            .map(|member| endpoint(peers[&member.identity.replica_id], member.role))
            .collect::<Vec<_>>();
        status.public_v1_topology =
            Some(PublicV1TopologyState::from_configuration(&next, &sessions).map_err(invalid)?);
        self.persist_goals(raw, status).await.map(Some)
    }

    async fn persist_goals(
        &self,
        raw: &RawObservation,
        status: KubericSetStatus,
    ) -> Result<ReconcileAction> {
        let mut next = raw.set.clone();
        next.status = Some(status.clone());
        next.validate_ordinary_hosting().map_err(invalid)?;
        self.replace_public_v1_status(raw, status)
            .await
            .map_err(invalid)?;
        Ok(action(ReconcileKind::Applied))
    }

    fn goal_endpoint(raw: &RawObservation, goal: &ReplicaGoal) -> Result<String> {
        let pod = raw
            .pods
            .iter()
            .find(|pod| {
                pod.uid().as_deref() == Some(goal.scope.storage.replica.instance_id.as_str())
                    && pod.metadata.deletion_timestamp.is_none()
            })
            .ok_or(ControllerError::ObservationStale)?;
        crate::cluster_api::agent_endpoint(pod)
            .ok_or_else(|| invalid("goal target has no control endpoint"))
    }

    async fn send_goal(
        &self,
        raw: &RawObservation,
        goal: &ReplicaGoal,
        acknowledge: bool,
    ) -> Result<ReplicaAgentSnapshot> {
        let endpoint = Self::goal_endpoint(raw, goal)?;
        let request = kuberic_runtime::control::proto::ReplicaGoalRequest {
            protocol_version: ORDINARY_ACTION_FORMAT,
            canonical_json: if acknowledge {
                serde_json::to_vec(&ReplicaGoalAcknowledgement {
                    goal: goal.clone(),
                    logically_retired: false,
                })
            } else {
                serde_json::to_vec(goal)
            }
            .map_err(|error| invalid(error.to_string()))?,
        };
        let response = if acknowledge {
            self.agents
                .acknowledge_goal(&endpoint, &self.bearer_token, request)
                .await
        } else {
            self.agents
                .submit_goal(&endpoint, &self.bearer_token, request)
                .await
        }
        .inspect_err(|error| {
            #[cfg(feature = "test-util")]
            if std::env::var_os("KUBERIC_M3_ELECTION_DIAGNOSTICS").is_some() {
                static REPORTED: std::sync::atomic::AtomicBool =
                    std::sync::atomic::AtomicBool::new(false);
                if !REPORTED.swap(true, std::sync::atomic::Ordering::AcqRel) {
                    eprintln!("ordinary actual goal RPC failed: operation={}, desired={:?}, acknowledge={acknowledge}, error={error:?}", goal.operation_id, goal.desired);
                }
            }
            let _ = error;
        })
        .map_err(crate::cluster_api::map_agent_effect_error)?;
        if response.protocol_version != ORDINARY_ACTION_FORMAT {
            return Err(invalid("goal response format changed"));
        }
        let snapshot: ReplicaAgentSnapshot = serde_json::from_slice(&response.canonical_json)
            .map_err(|error| invalid(error.to_string()))?;
        if snapshot.format != ORDINARY_ACTION_FORMAT
            || snapshot.storage != goal.scope.storage
            || snapshot.history != goal.scope.history
            || snapshot.identity != goal.identity
            || snapshot.process_session_id != goal.scope.process_session_id
            || snapshot.creation_id != goal.scope.creation_id
        {
            return Err(invalid("goal response changed exact participant or scope"));
        }
        Ok(snapshot)
    }

    pub(crate) async fn reconcile_ordinary(&self, raw: &RawObservation) -> Result<ReconcileAction> {
        raw.set.validate_ordinary_hosting().map_err(invalid)?;
        if let Some(message) = raw
            .agents
            .values()
            .find_map(|observation| match observation {
                RawAgentObservation::Invalid { message } => Some(message),
                _ => None,
            })
        {
            return Err(invalid(message.clone()));
        }
        let mut status = raw.set.status.clone().unwrap_or_default();
        let mut observed = raw.clone();
        observed.agents.retain(|key, _| {
            raw.pods.iter().any(|pod| {
                pod.uid().as_deref() == Some(key.instance_id.as_str())
                    && pod.metadata.deletion_timestamp.is_none()
            })
        });
        let peers = snapshots(&observed);
        let desired = (1..=raw.set.spec.replicas)
            .map(|id| ReplicaId::new(i64::from(id)))
            .collect::<Vec<_>>();
        let current = status
            .public_v1_topology
            .as_ref()
            .map(PublicV1TopologyState::configuration)
            .transpose()
            .map_err(invalid)?;

        let historical_predecessors = current
            .as_ref()
            .map(|current| {
                desired
                    .iter()
                    .filter_map(|id| {
                        replacement_predecessor(&status, current, *id)
                            .map(|predecessor| (*id, predecessor))
                    })
                    .collect::<BTreeMap<_, _>>()
            })
            .unwrap_or_default();
        let bootstrap_ids = desired
            .iter()
            .filter(|id| !historical_predecessors.contains_key(id))
            .filter(|id| {
                current.as_ref().is_none_or(|configuration| {
                    !configuration
                        .members
                        .iter()
                        .any(|member| member.identity.replica_id == **id)
                })
            })
            .copied()
            .collect::<Vec<_>>();
        if crate::reconciler::bootstrap_resources_missing(raw, &bootstrap_ids, current.as_ref()) {
            self.ensure_replica_support(raw).await?;
            self.ensure_replica_scaffolding(raw, &bootstrap_ids).await?;
            return Ok(action(ReconcileKind::Executed));
        }
        let uid = ResourceUid::new(
            raw.set
                .uid()
                .ok_or_else(|| invalid("missing resource UID"))?,
        );
        let replacements = historical_predecessors
            .into_iter()
            .filter(|(_, predecessor)| {
                !raw.pods.iter().any(|pod| {
                    pod.uid().as_deref() == Some(predecessor.replica.instance_id.as_str())
                        && pod.metadata.deletion_timestamp.is_none()
                })
            })
            .collect::<BTreeMap<_, _>>();
        for (id, predecessor) in &replacements {
            let base = derive_replacement_resource_name(&uid, &predecessor.replica);
            let pod_name = format!("{}-{base}", raw.set.name_any());
            let ready = raw
                .pods
                .iter()
                .find(|pod| pod.name_any() == pod_name && pod.metadata.deletion_timestamp.is_none())
                .is_some_and(|pod| {
                    raw.pvcs
                        .iter()
                        .any(|pvc| pvc.name_any() == format!("{pod_name}-data"))
                        && pod.uid().is_some_and(|uid| {
                            raw.services.iter().any(|service| {
                                service
                                    .spec
                                    .as_ref()
                                    .and_then(|spec| spec.selector.as_ref())
                                    .and_then(|selector| selector.get(crate::crd::INSTANCE_LABEL))
                                    == Some(&uid)
                            })
                        })
                });
            if !ready {
                self.ensure_replacement_scaffolding(raw, *id, &predecessor.replica)
                    .await?;
                return Ok(action(ReconcileKind::Executed));
            }
        }
        if let Some(command) = crate::reconciler::next_initialization_command(
            &observed,
            &uid,
            &desired,
            current.as_ref(),
        )? {
            self.execute_command(
                raw,
                &kuberic_runtime::protocol::command::ProtocolCommand::InitializeAgentStore(
                    Box::new(command),
                ),
            )
            .await?;
            return Ok(action(ReconcileKind::Executed));
        }
        if peers.is_empty() {
            return Ok(action(ReconcileKind::Waiting));
        }

        if status.public_v1_authority.is_none() {
            let first = peers.values().next().unwrap();
            let accepted = crate::evaluator::public_v1_history::ensure_bootstrap_history(
                None,
                &uid,
                &first.identity,
            )
            .map_err(invalid)?;
            if peers.values().any(|peer| peer.history != accepted.history) {
                return Err(invalid("fresh peers disagree with bootstrap history"));
            }
            status.public_v1_authority =
                Some(PublicV1AuthorityStatus::from_authority(&accepted).map_err(invalid)?);
            return self.persist_goals(raw, status).await;
        }
        let authority = status
            .public_v1_authority
            .as_ref()
            .unwrap()
            .authority()
            .map_err(invalid)?;
        if peers
            .values()
            .any(|peer| peer.history != authority.history || peer.identity != authority.identity)
        {
            return Err(invalid("snapshot differs from accepted history"));
        }

        // Cancellation cannot wait behind the callback whose target was withdrawn.
        let cancelled = status
            .replica_goals
            .iter()
            .filter_map(|entry| {
                if let ReplicaGoalKind::Build { remote } = &entry.goal.desired
                    && remote.target.scope.storage.replica.replica_id.value()
                        > i64::from(raw.set.spec.replicas)
                    && entry.outcome.is_none()
                {
                    Some(remote.as_ref())
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();
        if !cancelled.is_empty() {
            let mut withdrawals = Vec::new();
            for remote in cancelled {
                for publication in [&remote.source, &remote.target] {
                    let id = publication.scope.storage.replica.replica_id;
                    let Some(peer) = peers.get(&id) else { continue };
                    if !status.replica_goals.iter().any(|entry| {
                        entry.goal.scope.process_session_id == peer.process_session_id
                            && matches!(
                                entry.goal.desired,
                                ReplicaGoalKind::Abort | ReplicaGoalKind::Close
                            )
                    }) {
                        withdrawals.push(goal(
                            peer,
                            publication.scope.configuration.clone(),
                            ReplicaGoalKind::Abort,
                            &status.replica_goals,
                        )?);
                    }
                }
            }
            if !withdrawals.is_empty() {
                status.replica_goals.extend(withdrawals);
                return self.persist_goals(raw, status).await;
            }
        }

        if let Some(current) = current.as_ref()
            && let Some(next) = self
                .reconcile_inspection_round(raw, &status, current, &peers)
                .await?
        {
            return Ok(next);
        }
        if let Some(current) = current.as_ref()
            && let Some(next) = self
                .reconcile_original_members(raw, &status, current, &peers)
                .await?
        {
            return Ok(next);
        }
        if let Some(next) = self
            .reconcile_ordered_addition(raw, &status, &peers)
            .await?
        {
            return Ok(next);
        }
        if let Some(next) = self
            .reconcile_contained_creation(raw, &status, &peers)
            .await?
        {
            return Ok(next);
        }
        if let Some(current) = current.as_ref()
            && let Some(next) = self
                .reconcile_retained_members(raw, &status, current, &peers)
                .await?
        {
            return Ok(next);
        }
        if let Some(current) = current.as_ref()
            && let Some(next) = self
                .prepare_selected_primary_dependencies(raw, &status, current, &peers)
                .await?
        {
            return Ok(next);
        }
        let mut changed = false;
        let mut pending = false;
        for entry in status
            .replica_goals
            .iter_mut()
            .filter(|entry| entry.outcome.is_none())
        {
            let Some(peer) = peers.get(&entry.goal.scope.storage.replica.replica_id) else {
                pending = true;
                continue;
            };
            let observation = peer
                .goals
                .iter()
                .find(|observation| observation.goal.operation_id == entry.goal.operation_id);
            if peer.storage != entry.goal.scope.storage
                || peer.process_session_id != entry.goal.scope.process_session_id
                || (peer.creation_id != entry.goal.scope.creation_id
                    && !observation.is_some_and(|observation| {
                        observation.goal == entry.goal
                            && matches!(observation.status, ReplicaGoalStatus::Failed(_))
                    }))
            {
                // Old callbacks are uncertainty, never permission to replay into a replacement.
                entry.outcome = Some(ReplicaGoalObservation {
                    goal: entry.goal.clone(),
                    status: ReplicaGoalStatus::Uncertain(
                        "participant process/creation/storage changed".into(),
                    ),
                    selected_call: None,
                    receipts: 0,
                    host_settled: false,
                    completed_handoff: None,
                });
                changed = true;
                continue;
            }
            if observation.is_some_and(|observation| observation.goal != entry.goal) {
                return Err(invalid("agent changed canonical retained goal"));
            }
            if let Some(observation) = observation
                && !matches!(observation.status, ReplicaGoalStatus::Pending)
            {
                let mut completion = observation.clone();
                if observation.status == ReplicaGoalStatus::Achieved {
                    if matches!(
                        entry.goal.desired,
                        ReplicaGoalKind::RecoverContainedCreation { .. }
                    ) && !reconstruction::achieved_scope(peer, &entry.goal)
                    {
                        return Err(invalid(
                            "reconstruction completion lost its exact actual scope",
                        ));
                    }
                    if matches!(
                        entry.goal.desired,
                        ReplicaGoalKind::RestoreRetainedMember { .. }
                    ) && !retained_restore_achieved(peer, &entry.goal)
                    {
                        return Err(invalid("restoration completion lost exact fresh receipts"));
                    }
                    if let ReplicaGoalKind::Build { remote } = &entry.goal.desired
                        && (!exact_publications(remote, &peers)
                            || !peer.remote_operations.iter().any(|operation| {
                                operation.operation_id == entry.goal.operation_id
                                    && operation.selection == **remote
                                    && operation.state == RemoteReplicaState::Ready
                            }))
                    {
                        entry.outcome = Some(ReplicaGoalObservation {
                            status: ReplicaGoalStatus::Uncertain(
                                "Build lost its exact two-ended Ready scope".into(),
                            ),
                            ..observation.clone()
                        });
                        changed = true;
                        continue;
                    }
                    let accepted = self.send_goal(raw, &entry.goal, true).await?;
                    completion = accepted
                        .goals
                        .iter()
                        .find(|accepted| {
                            accepted.goal == entry.goal
                                && accepted.status == ReplicaGoalStatus::Achieved
                                && accepted.host_settled
                        })
                        .cloned()
                        .ok_or_else(|| {
                            invalid("goal acknowledgement lost its actual joined root")
                        })?;
                    if matches!(entry.goal.desired, ReplicaGoalKind::Discover)
                        && let Some(native) = accepted.native_status.as_ref()
                        && election::inspection_matches(&entry.goal, native)
                    {
                        entry.native_status = Some(
                            serde_json::to_string(native)
                                .map_err(|error| invalid(error.to_string()))?,
                        );
                    }
                    if matches!(
                        entry.goal.desired,
                        ReplicaGoalKind::RecoverContainedCreation { .. }
                    ) && !reconstruction::achieved_scope(&accepted, &entry.goal)
                    {
                        return Err(invalid(
                            "reconstruction acceptance lost exact actual initialization",
                        ));
                    }
                    if matches!(
                        entry.goal.desired,
                        ReplicaGoalKind::RestoreRetainedMember { .. }
                    ) && !retained_restore_achieved(&accepted, &entry.goal)
                    {
                        return Err(invalid("restoration acceptance lost exact fresh receipts"));
                    }
                    if let ReplicaGoalKind::Build { remote } = &entry.goal.desired
                        && !accepted.remote_operations.iter().any(|operation| {
                            operation.operation_id == entry.goal.operation_id
                                && operation.selection == **remote
                                && operation.state == RemoteReplicaState::Ready
                                && operation.accepted
                        })
                    {
                        return Err(invalid("Build acceptance lost exact Ready"));
                    }
                }
                entry.outcome = Some(completion);
                changed = true;
            } else {
                if !Self::primary_dependencies_ready(&entry.goal, current.as_ref(), &peers) {
                    pending = true;
                    continue;
                }
                // An RPC observer can be lost at any point; the same canonical goal is retried.
                if matches!(entry.goal.desired, ReplicaGoalKind::Build { .. })
                    && peers.values().any(|snapshot| current_terminal(snapshot))
                {
                    pending = true;
                    continue;
                }
                self.send_goal(raw, &entry.goal, false).await?;
                pending = true;
            }
        }
        if changed {
            return self.persist_goals(raw, status).await;
        }
        if pending {
            // Submission may already have changed the host. Plan only from a fresh observation.
            return Ok(action(ReconcileKind::Waiting));
        }
        let mut reopen = Vec::new();
        for peer in peers
            .values()
            .filter(|peer| peer.publication.is_none() && !peer.withdrawn)
        {
            if reconstruction::required(&status, &peers, peer) {
                continue;
            }
            if peer.goals.iter().any(|observation| {
                observation.goal.scope.process_session_id == peer.process_session_id
            }) || status.replica_goals.iter().any(|entry| {
                entry.goal.scope.storage == peer.storage
                    && entry.goal.scope.process_session_id == peer.process_session_id
                    && entry.goal.scope.creation_id == peer.creation_id
            }) {
                continue;
            }
            if let Some(previous) = peer
                .goals
                .iter()
                .filter(|observation| {
                    matches!(
                        observation.goal.desired,
                        ReplicaGoalKind::Discover
                            | ReplicaGoalKind::Prepare { .. }
                            | ReplicaGoalKind::Reconfigure { .. }
                    )
                })
                .max_by_key(|observation| observation.goal.revision)
            {
                let mut configuration = if let Some(current) = current.as_ref()
                    && !current
                        .members
                        .iter()
                        .any(|member| member.identity == peer.storage.replica)
                {
                    ReplicaConfigurationScope {
                        current: current.clone(),
                        previous: None,
                        replicas: Vec::new(),
                    }
                } else {
                    previous.goal.scope.configuration.clone()
                };
                configuration.replicas.clear();
                reopen.push(goal(
                    peer,
                    configuration,
                    ReplicaGoalKind::Discover,
                    &status.replica_goals,
                )?);
            }
        }
        if !reopen.is_empty() {
            status
                .replica_goals
                .extend(reopen.into_iter().take(PREPARATION_BATCH));
            return self.persist_goals(raw, status).await;
        }
        if status.replica_goals.iter().any(|entry| {
            entry.outcome.as_ref().is_some_and(|outcome| {
                matches!(
                    outcome.status,
                    ReplicaGoalStatus::Failed(_) | ReplicaGoalStatus::Uncertain(_)
                )
            }) && !reconstruction::resolved(&status, &peers, entry)
        }) {
            return Err(invalid(
                "ordinary goal failed or is uncertain; creation settlement/replacement is required",
            ));
        }
        if let Some(current) = current.as_ref()
            && let Some(next) = self
                .reconcile_quorum_healing(raw, &status, current, &peers)
                .await?
        {
            return Ok(next);
        }
        if let Some(current) = current.as_ref()
            && let Some(next) = self
                .reconcile_accepted_switchover(raw, &status, current, &peers)
                .await?
        {
            return Ok(next);
        }
        if let Some(current) = current.as_ref()
            && let Some(next) = self
                .reconcile_survivor_election(raw, &status, current, &peers)
                .await?
        {
            return Ok(next);
        }
        if let Some(current) = current.as_ref()
            && let Some(next) = self
                .reconcile_returned_primary(raw, &status, current, &peers)
                .await?
        {
            return Ok(next);
        }
        if peers.values().any(|peer| current_terminal(peer)) {
            return Ok(action(ReconcileKind::Waiting));
        }
        if let Some(current) = current.as_ref()
            && let Some(next) = self
                .reconcile_full_quorum_replacement(raw, &status, current, &peers)
                .await?
        {
            return Ok(next);
        }
        let primary_id = current
            .as_ref()
            .map_or(desired[0], |configuration| configuration.primary_id);
        let primary_identity =
            crate::reconciler::observed_or_derived_identity(raw, &uid, primary_id)?;
        let bootstrap = current.clone().unwrap_or_else(|| {
            ConfigurationDescriptor::new(
                Epoch::new(authority.history.data_loss_number, 1),
                primary_id,
                vec![ConfigurationMember {
                    identity: primary_identity,
                    role: ReplicaRole::Primary,
                }],
                1,
            )
        });
        let initial = ReplicaConfigurationScope {
            current: bootstrap.clone(),
            previous: None,
            replicas: Vec::new(),
        };

        let mut preparations = Vec::new();
        for id in &desired {
            let Some(peer) = peers.get(id) else { continue };
            if peer.publication.is_none()
                && !status.replica_goals.iter().any(|entry| {
                    entry.goal.scope.storage == peer.storage
                        && entry.goal.scope.process_session_id == peer.process_session_id
                        && entry.outcome.is_none()
                })
            {
                if peer.open_mode == OpenMode::Existing && peer.goals.is_empty() {
                    return Err(invalid(
                        "retained application requires fresh recovery/progress evidence; automatic history election is unavailable",
                    ));
                }
                preparations.push(goal(
                    peer,
                    initial.clone(),
                    ReplicaGoalKind::Discover,
                    &status.replica_goals,
                )?);
            }
        }
        if !preparations.is_empty() {
            status
                .replica_goals
                .extend(preparations.into_iter().take(PREPARATION_BATCH));
            return self.persist_goals(raw, status).await;
        }
        if desired.iter().any(|id| !peers.contains_key(id)) {
            if let Some(current) = current.as_ref()
                && let Some(next) = self
                    .reconcile_missing_secondaries(raw, &status, current, &peers)
                    .await?
            {
                return Ok(next);
            }
            return Ok(action(ReconcileKind::Waiting));
        }
        let primary = peers.get(&primary_id).ok_or_else(|| invalid("retained-history election requires fresh recovery/progress evidence; M1 cannot elect an unavailable primary"))?;
        if let Some(current) = current.as_ref()
            && let Some(next) = self
                .reconcile_primary_reopen(raw, &status, current, &peers)
                .await?
        {
            return Ok(next);
        }
        for id in &desired {
            let peer = peers[id];
            if peer.publication.as_ref().is_some_and(|publication| {
                publication.stage == ReplicaPublicationStage::DiscoveryPending
            }) && !status.replica_goals.iter().any(|entry| {
                entry.goal.scope.storage == peer.storage
                    && entry.goal.scope.process_session_id == peer.process_session_id
                    && entry.outcome.is_none()
            }) {
                let role = bootstrap
                    .members
                    .iter()
                    .find(|member| member.identity == peer.storage.replica)
                    .map_or(ReplicaRole::IdleSecondary, |member| member.role);
                let configuration = configuration_for(
                    if role == ReplicaRole::IdleSecondary {
                        primary_id
                    } else {
                        *id
                    },
                    &bootstrap,
                    None,
                    &peers,
                )?;
                preparations.push(goal(
                    peer,
                    configuration,
                    ReplicaGoalKind::Prepare {
                        role,
                        rebuild: None,
                    },
                    &status.replica_goals,
                )?);
            }
        }
        if !preparations.is_empty() {
            status
                .replica_goals
                .extend(preparations.into_iter().take(PREPARATION_BATCH));
            return self.persist_goals(raw, status).await;
        }
        if current.is_none() {
            let sessions = vec![endpoint(primary, ReplicaRole::Primary)];
            status.public_v1_topology = Some(
                PublicV1TopologyState::from_configuration(&bootstrap, &sessions)
                    .map_err(invalid)?,
            );
            return self.persist_goals(raw, status).await;
        }
        let current = current.unwrap();
        if current
            .members
            .iter()
            .any(|member| !desired.contains(&member.identity.replica_id))
        {
            return Err(invalid(
                "admitted membership reduction requires ordinary reconfiguration/settlement; automatic M1 contraction is unavailable",
            ));
        }
        for build in status.replica_goals.iter().filter(|entry| achieved(entry)) {
            let ReplicaGoalKind::Build { remote } = &build.goal.desired else {
                continue;
            };
            if desired.contains(&remote.target.scope.storage.replica.replica_id) {
                continue;
            }
            let removal = status.replica_goals.iter().find(|entry| {
                matches!(&entry.goal.desired, ReplicaGoalKind::Remove { remote: selected } if selected == remote)
            });
            if removal.is_none() {
                if !exact_publications(remote, &peers) {
                    return Err(invalid("idle removal lost its exact source/target scope"));
                }
                let next = goal(
                    primary,
                    remote.source.scope.configuration.clone(),
                    ReplicaGoalKind::Remove {
                        remote: remote.clone(),
                    },
                    &status.replica_goals,
                )?;
                status.replica_goals.push(next);
                return self.persist_goals(raw, status).await;
            }
            if removal.is_some_and(achieved) {
                let target = peers
                    .get(&remote.target.scope.storage.replica.replica_id)
                    .ok_or_else(|| {
                        invalid("retired idle target is unavailable for separate settlement")
                    })?;
                let next = goal(
                    target,
                    remote.target.scope.configuration.clone(),
                    ReplicaGoalKind::Abort,
                    &status.replica_goals,
                )?;
                status.replica_goals.push(next);
                return self.persist_goals(raw, status).await;
            }
        }

        let addition = desired
            .iter()
            .filter(|id| {
                !current
                    .members
                    .iter()
                    .any(|member| member.identity.replica_id == **id)
            })
            .copied()
            .collect::<Vec<_>>();
        if addition.is_empty() {
            if self
                .cleanup_replaced_storage(raw, &status, &current)
                .await?
            {
                return Ok(action(ReconcileKind::Executed));
            }
            let sessions = current
                .members
                .iter()
                .map(|member| endpoint(peers[&member.identity.replica_id], member.role))
                .collect::<Vec<_>>();
            let topology = status.public_v1_topology.as_ref().unwrap();
            if current.members.len() > 1 && topology.sessions != sessions {
                let old_primary = topology
                    .sessions
                    .iter()
                    .find(|session| session.role == ReplicaRole::Primary)
                    .ok_or_else(|| invalid("current topology lacks its primary session"))?;
                if *old_primary != endpoint(primary, ReplicaRole::Primary) {
                    return Err(invalid(
                        "current-member recovery cannot replace the primary",
                    ));
                }
                for member in current
                    .members
                    .iter()
                    .filter(|m| m.role == ReplicaRole::ActiveSecondary)
                {
                    let peer = peers[&member.identity.replica_id];
                    let prior = topology
                        .sessions
                        .iter()
                        .find(|session| session.identity == member.identity)
                        .ok_or_else(|| invalid("recovery lost its admitted predecessor"))?;
                    if prior.storage_incarnation != peer.storage
                        || !peer.publication.as_ref().is_some_and(|publication| {
                            publication.stage == ReplicaPublicationStage::AchievedScope
                                && publication.role == ReplicaRole::ActiveSecondary
                                && publication.scope.configuration.current == current
                                && publication.scope.configuration.previous.is_none()
                                && status.replica_goals.iter().any(|entry| {
                                    entry.goal.scope == publication.scope
                                        && achieved(entry)
                                        && matches!(
                                            entry.goal.desired,
                                            ReplicaGoalKind::Prepare {
                                                role: ReplicaRole::ActiveSecondary,
                                                ..
                                            }
                                        )
                                })
                        }) && *prior != endpoint(peer, member.role)
                    {
                        return Ok(action(ReconcileKind::Waiting));
                    }
                }
                let configuration = configuration_for(primary_id, &current, None, &peers)?;
                let retained = status.replica_goals.iter().rev().find(|entry| {
                    entry.goal.scope.storage == primary.storage
                        && entry.goal.scope.process_session_id == primary.process_session_id
                        && entry.goal.scope.creation_id == primary.creation_id
                        && entry.goal.scope.configuration == configuration
                        && matches!(
                            entry.goal.desired,
                            ReplicaGoalKind::Reconfigure {
                                role: ReplicaRole::Primary,
                                catch_up: Some(PublicCatchUpMode::WriteQuorum)
                            }
                        )
                });
                if retained.is_none() {
                    status.replica_goals.push(goal(
                        primary,
                        configuration,
                        ReplicaGoalKind::Reconfigure {
                            role: ReplicaRole::Primary,
                            catch_up: Some(PublicCatchUpMode::WriteQuorum),
                        },
                        &status.replica_goals,
                    )?);
                    return self.persist_goals(raw, status).await;
                }
                if !retained.is_some_and(achieved)
                    || primary.write_status != AccessStatus::Granted
                    || primary.read_status != AccessStatus::Granted
                {
                    return Ok(action(ReconcileKind::Waiting));
                }
                for member in current
                    .members
                    .iter()
                    .filter(|m| m.role == ReplicaRole::ActiveSecondary)
                {
                    let peer = peers[&member.identity.replica_id];
                    if topology.sessions.contains(&endpoint(peer, member.role)) {
                        continue;
                    }
                    let configuration =
                        configuration_for(member.identity.replica_id, &current, None, &peers)?;
                    if !status.replica_goals.iter().any(|entry| {
                        entry.goal.scope.storage == peer.storage
                            && entry.goal.scope.process_session_id == peer.process_session_id
                            && entry.goal.scope.creation_id == peer.creation_id
                            && entry.goal.scope.configuration == configuration
                            && achieved(entry)
                            && matches!(
                                entry.goal.desired,
                                ReplicaGoalKind::Reconfigure {
                                    role: ReplicaRole::ActiveSecondary,
                                    catch_up: None
                                }
                            )
                    }) {
                        status.replica_goals.push(goal(
                            peer,
                            configuration,
                            ReplicaGoalKind::Reconfigure {
                                role: ReplicaRole::ActiveSecondary,
                                catch_up: None,
                            },
                            &status.replica_goals,
                        )?);
                        return self.persist_goals(raw, status).await;
                    }
                }
                status.public_v1_topology = Some(
                    PublicV1TopologyState::from_configuration(&current, &sessions)
                        .map_err(invalid)?,
                );
                return self.persist_goals(raw, status).await;
            }
            let serving = primary.write_status == AccessStatus::Granted
                && primary.read_status == AccessStatus::Granted;
            if !serving
                && !status.replica_goals.iter().any(|entry| {
                    entry.goal.scope.storage == primary.storage
                        && entry.goal.scope.process_session_id == primary.process_session_id
                        && entry.goal.scope.creation_id == primary.creation_id
                        && matches!(
                            entry.goal.desired,
                            ReplicaGoalKind::Reconfigure {
                                role: ReplicaRole::Primary,
                                catch_up: Some(_)
                            }
                        )
                        && entry.goal.scope.configuration.current == current
                        && entry.goal.scope.configuration.previous.is_none()
                })
            {
                let activation = goal(
                    primary,
                    configuration_for(primary_id, &current, None, &peers)?,
                    ReplicaGoalKind::Reconfigure {
                        role: ReplicaRole::Primary,
                        catch_up: Some(PublicCatchUpMode::WriteQuorum),
                    },
                    &status.replica_goals,
                )?;
                status.replica_goals.push(activation);
                return self.persist_goals(raw, status).await;
            }
            // A singleton has no remote session to replace or readmit.
            if current.members.len() == 1
                && serving
                && primary.publication.as_ref().is_some_and(|publication| {
                    publication.stage == ReplicaPublicationStage::AchievedScope
                        && publication.role == ReplicaRole::Primary
                        && publication.scope.storage == primary.storage
                        && publication.scope.process_session_id == primary.process_session_id
                        && publication.scope.creation_id == primary.creation_id
                        && publication.scope.configuration.current == current
                        && publication.scope.configuration.previous.is_none()
                        && status.replica_goals.iter().any(|entry| {
                            entry.goal.scope == publication.scope
                                && achieved(entry)
                                && matches!(
                                    entry.goal.desired,
                                    ReplicaGoalKind::Reconfigure {
                                        role: ReplicaRole::Primary,
                                        catch_up: Some(PublicCatchUpMode::WriteQuorum)
                                    }
                                )
                        })
                })
                && status.public_v1_topology.as_ref().unwrap().sessions
                    != [endpoint(primary, ReplicaRole::Primary)]
            {
                status.public_v1_topology = Some(
                    PublicV1TopologyState::from_configuration(
                        &current,
                        &[endpoint(primary, ReplicaRole::Primary)],
                    )
                    .map_err(invalid)?,
                );
                return self.persist_goals(raw, status).await;
            }
            self.converge_public_v1_write_service(raw, serving.then_some(&primary.storage.replica))
                .await?;
            return Ok(action(if serving {
                ReconcileKind::Stable
            } else {
                ReconcileKind::Waiting
            }));
        }
        let joint = status
            .replica_goals
            .iter()
            .rev()
            .find(|entry| {
                matches!(
                    entry.goal.desired,
                    ReplicaGoalKind::Reconfigure {
                        role: ReplicaRole::Primary,
                        ..
                    }
                ) && entry.goal.scope.configuration.previous.as_ref() == Some(&current)
            })
            .cloned();
        if joint.is_none() {
            for id in &addition {
                let target = peers[id];
                let source_publication = primary
                    .publication
                    .as_ref()
                    .ok_or_else(|| invalid("source unpublished"))?;
                let target_publication = target
                    .publication
                    .as_ref()
                    .ok_or_else(|| invalid("target unpublished"))?;
                if let Some(predecessor) = replacements.get(id)
                    && let Some(old) = status.replica_goals.iter().rev().find_map(|entry| {
                        match &entry.goal.desired {
                            ReplicaGoalKind::Build { remote }
                                if remote.target.scope.storage == *predecessor
                                    && remote.source.scope.storage == primary.storage
                                    && remote.source.scope.process_session_id == primary.process_session_id
                                    && remote.source.scope.creation_id == primary.creation_id => Some(remote.clone()),
                            _ => None,
                        }
                    })
                    && !status.replica_goals.iter().any(|entry| {
                        achieved(entry)
                            && matches!(&entry.goal.desired, ReplicaGoalKind::Remove { remote } if remote == &old)
                    })
                {
                    status.replica_goals.push(goal(
                        primary,
                        source_publication.scope.configuration.clone(),
                        ReplicaGoalKind::Remove { remote: old },
                        &status.replica_goals,
                    )?);
                    return self.persist_goals(raw, status).await;
                }
                if !primary.remote_operations.iter().any(|remote| {
                    remote.selection.source == *source_publication
                        && remote.selection.target == *target_publication
                        && remote.state == RemoteReplicaState::Ready
                        && remote.accepted
                }) {
                    if status.replica_goals.iter().any(|entry| {
                        matches!(&entry.goal.desired, ReplicaGoalKind::Build { remote }
                            if remote.target.scope.storage == target.storage)
                            && !reconstruction::resolved(&status, &peers, entry)
                    }) {
                        return Err(invalid(
                            "retained Build cannot be reused after target/source publication changed",
                        ));
                    }
                    let target_control_address = peer_address(raw, &target.storage.replica)?;
                    let remote = RemoteReplicaSelection {
                        original_member: None,
                        source: source_publication.clone(),
                        target: target_publication.clone(),
                        target_control_address,
                        generation: reconstruction::next_generation(
                            &status,
                            primary,
                            target_publication,
                        )?,
                    };
                    let build = goal(
                        primary,
                        source_publication.scope.configuration.clone(),
                        ReplicaGoalKind::Build {
                            remote: Box::new(remote),
                        },
                        &status.replica_goals,
                    )?;
                    status.replica_goals.push(build);
                    return self.persist_goals(raw, status).await;
                }
            }
            let members = desired
                .iter()
                .map(|id| ConfigurationMember {
                    identity: peers[id].storage.replica.clone(),
                    role: if *id == primary_id {
                        ReplicaRole::Primary
                    } else {
                        ReplicaRole::ActiveSecondary
                    },
                })
                .collect::<Vec<_>>();
            let policy =
                EffectivePolicy::fixed(raw.set.spec.replicas, raw.set.spec.failover_delay_seconds)
                    .ok_or_else(|| invalid("invalid desired policy"))?;
            let next = ConfigurationDescriptor::new(
                current.epoch,
                primary_id,
                members,
                policy.write_quorum,
            );
            let next_goal = goal(
                primary,
                configuration_for(primary_id, &next, Some(&current), &peers)?,
                ReplicaGoalKind::Reconfigure {
                    role: ReplicaRole::Primary,
                    catch_up: None,
                },
                &status.replica_goals,
            )?;
            status.replica_goals.push(next_goal);
            return self.persist_goals(raw, status).await;
        }
        let joint = joint.unwrap();
        if !achieved(&joint) {
            return Ok(action(ReconcileKind::Waiting));
        }
        let next = &joint.goal.scope.configuration.current;
        let mut updates = Vec::new();
        for member in next
            .members
            .iter()
            .filter(|member| member.identity.replica_id != primary_id)
        {
            let peer = peers[&member.identity.replica_id];
            if !peer.publication.as_ref().is_some_and(|publication| {
                publication.role == member.role
                    && publication.stage == ReplicaPublicationStage::AchievedScope
                    && publication.scope.configuration.current == *next
                    && publication.scope.configuration.previous.as_ref() == Some(&current)
            }) && !status.replica_goals.iter().any(|entry| {
                entry.goal.scope.storage == peer.storage
                    && achieved(entry)
                    && matches!(entry.goal.desired, ReplicaGoalKind::Reconfigure { .. })
                    && entry.goal.scope.configuration.current == *next
                    && entry.goal.scope.configuration.previous.is_none()
            }) {
                updates.push(goal(
                    peer,
                    configuration_for(member.identity.replica_id, next, Some(&current), &peers)?,
                    ReplicaGoalKind::Reconfigure {
                        role: member.role,
                        catch_up: None,
                    },
                    &status.replica_goals,
                )?);
            }
        }
        if !updates.is_empty() {
            status
                .replica_goals
                .extend(updates.into_iter().take(PREPARATION_BATCH));
            return self.persist_goals(raw, status).await;
        }
        for member in &next.members {
            let peer = peers[&member.identity.replica_id];
            if !peer.publication.as_ref().is_some_and(|publication| {
                publication.stage == ReplicaPublicationStage::AchievedScope
                    && publication.role == member.role
                    && publication.scope.configuration.current == *next
                    && publication.scope.configuration.previous.is_none()
            }) {
                let next_goal = goal(
                    peer,
                    configuration_for(member.identity.replica_id, next, None, &peers)?,
                    ReplicaGoalKind::Reconfigure {
                        role: member.role,
                        catch_up: (member.role == ReplicaRole::Primary)
                            .then_some(PublicCatchUpMode::WriteQuorum),
                    },
                    &status.replica_goals,
                )?;
                status.replica_goals.push(next_goal);
                return self.persist_goals(raw, status).await;
            }
        }
        let sessions = next
            .members
            .iter()
            .map(|member| endpoint(peers[&member.identity.replica_id], member.role))
            .collect::<Vec<_>>();
        status.public_v1_topology =
            Some(PublicV1TopologyState::from_configuration(next, &sessions).map_err(invalid)?);
        self.persist_goals(raw, status).await
    }
}

fn endpoint(snapshot: &ReplicaAgentSnapshot, role: ReplicaRole) -> ProcessSessionEndpoint {
    ProcessSessionEndpoint {
        identity: snapshot.storage.replica.clone(),
        storage_incarnation: snapshot.storage.clone(),
        process_session_id: snapshot.process_session_id.clone(),
        replicator_creation_id: snapshot.creation_id.clone(),
        role,
    }
}

fn peer_address(raw: &RawObservation, identity: &ReplicaIdentity) -> Result<String> {
    if let Some(address) = raw
        .pods
        .iter()
        .find(|pod| pod.uid().as_deref() == Some(identity.instance_id.as_str()))
        .and_then(|pod| {
            pod.annotations()
                .get(crate::crd::CONTROL_ADDRESS_ANNOTATION)
                .cloned()
        })
    {
        return Ok(address);
    }
    let namespace = raw
        .set
        .namespace()
        .ok_or_else(|| invalid("resource has no namespace"))?;
    Ok(format!(
        "http://{}.{namespace}.svc:50051",
        derive_replica_endpoint_name(&ResourceUid::new(raw.set.uid().unwrap()), identity)
    ))
}

#[cfg(test)]
mod switchover_tests {
    use super::*;

    fn current(epoch: i64) -> ConfigurationDescriptor {
        ConfigurationDescriptor::new(
            Epoch::new(7, epoch),
            ReplicaId::new(1),
            (1..=3)
                .map(|id| ConfigurationMember {
                    identity: ReplicaIdentity {
                        replica_id: ReplicaId::new(id),
                        instance_id: ReplicaInstanceId::new(format!("pod-{id}")),
                        agent_generation: AgentGeneration::new(format!("agent-{id}")),
                    },
                    role: if id == 1 {
                        ReplicaRole::Primary
                    } else {
                        ReplicaRole::ActiveSecondary
                    },
                })
                .collect(),
            2,
        )
    }

    #[test]
    fn ordinary_switchover_retains_members_and_quorum_across_preparation_and_promotion() {
        let starting = current(11);
        let intermediate = switchover_configuration(&starting, starting.primary_id, 1).unwrap();
        let handoff = switchover_configuration(&starting, ReplicaId::new(2), 1).unwrap();
        let promoted = switchover_configuration(&starting, ReplicaId::new(2), 2).unwrap();
        assert_eq!(intermediate.primary_id, starting.primary_id);
        assert_eq!(handoff.primary_id, ReplicaId::new(2));
        assert_eq!(promoted.primary_id, ReplicaId::new(2));
        assert_eq!(intermediate.epoch, Epoch::new(7, 12));
        assert_eq!(handoff.epoch, Epoch::new(7, 12));
        assert_eq!(promoted.epoch, Epoch::new(7, 13));
        for selected in [&intermediate, &handoff, &promoted] {
            assert_eq!(selected.write_quorum, starting.write_quorum);
            assert_eq!(
                selected
                    .members
                    .iter()
                    .map(|member| &member.identity)
                    .collect::<Vec<_>>(),
                starting
                    .members
                    .iter()
                    .map(|member| &member.identity)
                    .collect::<Vec<_>>()
            );
            assert_eq!(selected.configuration_id, selected.expected_id());
            assert_eq!(
                selected
                    .members
                    .iter()
                    .filter(|member| member.role == ReplicaRole::Primary)
                    .map(|member| member.identity.replica_id)
                    .collect::<Vec<_>>(),
                [selected.primary_id]
            );
        }
        assert_eq!(starting, current(11));
    }

    #[test]
    fn ordinary_switchover_rejects_unadmitted_target_and_epoch_exhaustion() {
        assert!(switchover_configuration(&current(11), ReplicaId::new(4), 1).is_err());
        assert!(switchover_configuration(&current(i64::MAX), ReplicaId::new(2), 1).is_err());
        assert!(switchover_configuration(&current(i64::MAX - 1), ReplicaId::new(2), 2).is_err());
    }
}

#[cfg(test)]
mod retained_member_tests {
    use super::*;
    use crate::cluster_api::AgentRpcError;
    use crate::crd::{KubericSet, KubericSetSpec};
    use k8s_openapi::api::core::v1::{
        PersistentVolumeClaim, PersistentVolumeClaimVolumeSource, Pod, PodSpec, Volume,
    };
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
    use kuberic_runtime::control::proto;
    use std::sync::{Arc, Mutex};

    fn storage(id: i64) -> ReplicaStorageIncarnationContext {
        ReplicaStorageIncarnationContext {
            replica: ReplicaIdentity {
                replica_id: ReplicaId::new(id),
                instance_id: ReplicaInstanceId::new(format!("pod-{id}")),
                agent_generation: AgentGeneration::new(format!("agent-{id}")),
            },
            storage: ReplicaStorageIdentity::Persistent {
                pod_uid: PodUid::new(format!("pod-{id}")),
                pvc_uid: PvcUid::new(format!("pvc-{id}")),
            },
            storage_generation: StorageGeneration::new(format!("storage-{id}")),
        }
    }

    pub(super) fn fixture() -> (
        ConfigurationDescriptor,
        KubericSetStatus,
        ReplicaAgentSnapshot,
    ) {
        let resource = ResourceUid::new("retained-controller");
        let generation = PublicProtocolGeneration::new(1);
        let initialization = derive_history_initialization_id(&resource, generation);
        let history = derive_bootstrap_history(&resource, generation, &initialization);
        let current = ConfigurationDescriptor::new(
            Epoch::new(history.data_loss_number, 11),
            ReplicaId::new(1),
            (1..=3)
                .map(|id| ConfigurationMember {
                    identity: storage(id).replica,
                    role: if id == 1 {
                        ReplicaRole::Primary
                    } else {
                        ReplicaRole::ActiveSecondary
                    },
                })
                .collect(),
            2,
        );
        let configuration = ReplicaConfigurationScope {
            current: current.clone(),
            previous: None,
            replicas: current
                .members
                .iter()
                .filter(|member| member.identity.replica_id != ReplicaId::new(2))
                .map(|member| ReplicaPeerDescription {
                    identity: member.identity.clone(),
                    replication_address: format!(
                        "http://127.0.0.1:{}",
                        50052 + member.identity.replica_id.value()
                    ),
                    process_session_id: ProcessSessionId::new(format!(
                        "old-{}",
                        member.identity.replica_id
                    )),
                    storage_incarnation: storage(member.identity.replica_id.value()),
                    role: member.role,
                    current_progress: 41,
                    catch_up_capability: 0,
                    is_up: true,
                    is_ready: true,
                    must_catchup: false,
                })
                .collect(),
        };
        let historical = ReplicaGoal {
            format: ORDINARY_ACTION_FORMAT,
            identity: PublicV1Identity::new(1),
            operation_id: OperationId::new("accepted-secondary"),
            revision: 4,
            scope: ReplicaScope {
                history: history.clone(),
                storage: storage(2),
                process_session_id: ProcessSessionId::new("old-2"),
                creation_id: ReplicatorCreationId::new("old-creation-2"),
                epoch: current.epoch,
                configuration,
            },
            desired: ReplicaGoalKind::Reconfigure {
                role: ReplicaRole::ActiveSecondary,
                catch_up: None,
            },
        };
        let observation = ReplicaGoalObservation {
            goal: historical.clone(),
            status: ReplicaGoalStatus::Achieved,
            selected_call: None,
            receipts: 1,
            host_settled: true,
            completed_handoff: None,
        };
        let mut retained = RetainedReplicaGoal::new(historical.clone()).unwrap();
        retained.outcome = Some(observation.clone());
        let sessions = current
            .members
            .iter()
            .map(|member| ProcessSessionEndpoint {
                identity: member.identity.clone(),
                storage_incarnation: storage(member.identity.replica_id.value()),
                process_session_id: ProcessSessionId::new(format!(
                    "old-{}",
                    member.identity.replica_id
                )),
                replicator_creation_id: ReplicatorCreationId::new(format!(
                    "old-creation-{}",
                    member.identity.replica_id
                )),
                role: member.role,
            })
            .collect::<Vec<_>>();
        let status = KubericSetStatus {
            replica_goals: vec![retained],
            public_v1_topology: Some(
                PublicV1TopologyState::from_configuration(&current, &sessions).unwrap(),
            ),
            ..Default::default()
        };
        let peer = ReplicaAgentSnapshot {
            format: ORDINARY_ACTION_FORMAT,
            identity: historical.identity.clone(),
            history,
            storage: storage(2),
            open_mode: OpenMode::Existing,
            read_status: AccessStatus::NotPrimary,
            write_status: AccessStatus::NotPrimary,
            withdrawn: false,
            provider_writer_held: false,
            process_session_id: ProcessSessionId::new("fresh-2"),
            creation_id: ReplicatorCreationId::new("fresh-creation-2"),
            publication: None,
            goals: vec![observation],
            remote_operations: Vec::new(),
            progress: ProgressQueryResult::NotQueried("fresh process".into()),
            capability: ProgressQueryResult::NotQueried("fresh process".into()),
            native_status: None,
            native_lifecycle: None,
            accepted_revision_high_watermark: 8,
            retained_recovery_exit: None,
            reconstruction_dependencies: Vec::new(),
            retained_member_qualification: Some(RetainedMemberQualification {
                predecessor_process_session_id: historical.scope.process_session_id,
                predecessor_creation_id: historical.scope.creation_id,
                admitted_operation_id: historical.operation_id,
                admitted_revision: historical.revision,
                configuration: current.clone(),
            }),
        };
        (current, status, peer)
    }

    #[test]
    fn completed_handoff_requires_exact_ordered_joined_receipts_and_current_physical_source() {
        let (initial, _, mut source) = fixture();
        let current =
            switchover_configuration(&initial, source.storage.replica.replica_id, 0).unwrap();
        let selected = switchover_configuration(&current, initial.primary_id, 1).unwrap();
        let mut handoff = source.goals[0].goal.clone();
        handoff.operation_id = OperationId::new("joined-source-transfer");
        handoff.revision = source.accepted_revision_high_watermark + 1;
        handoff.scope.process_session_id = source.process_session_id.clone();
        handoff.scope.creation_id = source.creation_id.clone();
        handoff.scope.epoch = selected.epoch;
        handoff.scope.configuration.current = selected.clone();
        handoff.scope.configuration.previous = Some(current.clone());
        handoff.desired = ReplicaGoalKind::Reconfigure {
            role: ReplicaRole::ActiveSecondary,
            catch_up: Some(PublicCatchUpMode::All),
        };
        let outcome = ReplicaGoalObservation {
            goal: handoff.clone(),
            status: ReplicaGoalStatus::Achieved,
            selected_call: None,
            receipts: SourceHandoffReceipt::ORDERED.len() + 1,
            host_settled: true,
            completed_handoff: Some(SourceHandoffReceipt::ORDERED.to_vec()),
        };
        let mut entry = RetainedReplicaGoal::new(handoff.clone()).unwrap();
        entry.outcome = Some(outcome.clone());
        source.goals = vec![outcome];
        source.progress = ProgressQueryResult::NotReady;
        source.capability = ProgressQueryResult::Value(INVALID_LSN);
        source.publication = Some(ReplicaPublication {
            format: ORDINARY_ACTION_FORMAT,
            scope: handoff.scope,
            replication_address: "http://127.0.0.1:50054".into(),
            generation: handoff.revision,
            stage: ReplicaPublicationStage::AchievedScope,
            role: ReplicaRole::ActiveSecondary,
        });
        assert!(completed_source_handoff(
            &entry, &source, &current, &selected
        ));
        assert!(source.progress.value().is_none());
        assert_eq!(source.capability.value(), Some(INVALID_LSN));
        let reject = |change: fn(&mut RetainedReplicaGoal, &mut ReplicaAgentSnapshot)| {
            let mut changed_entry = entry.clone();
            let mut changed_source = source.clone();
            change(&mut changed_entry, &mut changed_source);
            assert!(!completed_source_handoff(
                &changed_entry,
                &changed_source,
                &current,
                &selected
            ));
        };
        reject(|entry, source| {
            entry.outcome.as_mut().unwrap().completed_handoff = None;
            source.goals = vec![entry.outcome.clone().unwrap()];
        });
        reject(|entry, source| {
            entry
                .outcome
                .as_mut()
                .unwrap()
                .completed_handoff
                .as_mut()
                .unwrap()
                .swap(3, 4);
            source.goals = vec![entry.outcome.clone().unwrap()];
        });
        reject(|entry, source| {
            entry
                .outcome
                .as_mut()
                .unwrap()
                .completed_handoff
                .as_mut()
                .unwrap()
                .pop();
            source.goals = vec![entry.outcome.clone().unwrap()];
        });
        reject(|entry, source| {
            entry.outcome.as_mut().unwrap().host_settled = false;
            source.goals = vec![entry.outcome.clone().unwrap()];
        });
        reject(|entry, source| {
            entry.outcome.as_mut().unwrap().status =
                ReplicaGoalStatus::Failed("real child error".into());
            source.goals = vec![entry.outcome.clone().unwrap()];
        });
        reject(|_, source| source.process_session_id = ProcessSessionId::new("different-process"));
        reject(|_, source| source.creation_id = ReplicatorCreationId::new("different-creation"));
        reject(|_, source| {
            source.storage.storage_generation = StorageGeneration::new("different-storage")
        });
        reject(|_, source| source.history.history_id = HistoryId::new("different-history"));
        reject(|_, source| source.withdrawn = true);
        reject(|_, source| source.read_status = AccessStatus::Granted);
        reject(|_, source| source.write_status = AccessStatus::Granted);
        reject(|_, source| source.goals.clear());
        reject(|_, source| source.publication.as_mut().unwrap().generation += 1);
        reject(|_, source| {
            source.publication.as_mut().unwrap().stage = ReplicaPublicationStage::DiscoveryPending
        });
        reject(|_, source| {
            source
                .publication
                .as_mut()
                .unwrap()
                .scope
                .configuration
                .previous = None
        });
        reject(|entry, source| {
            entry.goal.desired = ReplicaGoalKind::Reconfigure {
                role: ReplicaRole::ActiveSecondary,
                catch_up: Some(PublicCatchUpMode::WriteQuorum),
            };
            entry.digest = entry.goal.intent().input_digest;
            entry.outcome.as_mut().unwrap().goal = entry.goal.clone();
            source.goals = vec![entry.outcome.clone().unwrap()];
        });
    }

    #[test]
    fn completed_native_handoff_fixture_retains_full_producer_mode_boundary_and_settlement() {
        // Captured from the actual ordinary-hosted native normal test, not the
        // legacy All receipt tags. This projection is not service qualification.
        let captured: serde_json::Value =
            serde_json::from_str(include_str!("normal_handoff_fixture.json")).unwrap();
        let handoff: ReplicaGoal = serde_json::from_value(captured["goal"].clone()).unwrap();
        handoff.validate().unwrap();
        assert_eq!(captured["mode"], "WriteQuorum");
        assert_eq!(
            captured["actions"],
            serde_json::json!([
                "Revoke",
                "ProgramEpoch",
                "RefreshedConfiguration",
                "SecondCatchUp",
                "ReplicatorRole",
                "ApplicationRole",
                "NativeComplete",
            ])
        );
        assert_eq!(
            captured["actions"].as_array().unwrap().len(),
            captured["outcomes"].as_array().unwrap().len()
        );
        assert_eq!(
            captured["outcomes"].as_array().unwrap().last().unwrap(),
            "NativeLifecycle"
        );
        assert_eq!(captured["accepted"], true);
        assert_eq!(captured["hostSettled"], true);
        let mut native = captured["native"].clone();
        native["scope"] = serde_json::to_value(&handoff.scope).unwrap();
        let facts: NativeLifecycleFacts = serde_json::from_value(native).unwrap();
        assert_eq!(facts.operation_id, handoff.operation_id);
        assert_eq!(facts.revision, handoff.revision);
        assert_eq!(facts.deactivation.as_ref().unwrap().catch_up_lsn, 1);
        let outcome = ReplicaGoalObservation {
            goal: handoff.clone(),
            status: ReplicaGoalStatus::Achieved,
            selected_call: None,
            receipts: captured["outcomes"].as_array().unwrap().len(),
            host_settled: captured["hostSettled"].as_bool().unwrap(),
            completed_handoff: None,
        };
        let mut entry = RetainedReplicaGoal::new(handoff.clone()).unwrap();
        entry.outcome = Some(outcome.clone());
        let mut source = fixture().2;
        source.identity = handoff.identity.clone();
        source.history = handoff.scope.history.clone();
        source.storage = handoff.scope.storage.clone();
        source.process_session_id = handoff.scope.process_session_id.clone();
        source.creation_id = handoff.scope.creation_id.clone();
        source.goals = vec![outcome];
        source.native_status = None;
        source.native_lifecycle = Some(facts);
        source.retained_member_qualification = None;
        source.publication = Some(ReplicaPublication {
            format: ORDINARY_ACTION_FORMAT,
            scope: handoff.scope.clone(),
            replication_address: "http://127.0.0.1:50054".into(),
            generation: handoff.revision,
            stage: ReplicaPublicationStage::AchievedScope,
            role: ReplicaRole::ActiveSecondary,
        });
        let previous = handoff.scope.configuration.previous.as_ref().unwrap();
        let current = &handoff.scope.configuration.current;
        assert!(completed_source_handoff(&entry, &source, previous, current));
        for kind in 0..7 {
            let mut changed_entry = entry.clone();
            let mut changed_source = source.clone();
            match kind {
                0 => changed_entry.outcome.as_mut().unwrap().receipts -= 1,
                1 => changed_entry.outcome.as_mut().unwrap().host_settled = false,
                2 => {
                    changed_source
                        .native_lifecycle
                        .as_mut()
                        .unwrap()
                        .deactivation = None
                }
                3 => {
                    changed_source
                        .native_lifecycle
                        .as_mut()
                        .unwrap()
                        .deactivation
                        .as_mut()
                        .unwrap()
                        .primary_fence = previous.epoch
                }
                4 => {
                    changed_source
                        .native_lifecycle
                        .as_mut()
                        .unwrap()
                        .application_role = Some(ReplicaRole::Primary)
                }
                5 => {
                    changed_source
                        .native_lifecycle
                        .as_mut()
                        .unwrap()
                        .scope
                        .creation_id = ReplicatorCreationId::new("recreated")
                }
                6 => {
                    changed_entry.goal.desired = ReplicaGoalKind::Reconfigure {
                        role: ReplicaRole::ActiveSecondary,
                        catch_up: Some(PublicCatchUpMode::All),
                    };
                    changed_entry.digest = changed_entry.goal.intent().input_digest;
                    changed_entry.outcome.as_mut().unwrap().goal = changed_entry.goal.clone();
                }
                _ => unreachable!(),
            }
            changed_source.goals = vec![changed_entry.outcome.clone().unwrap()];
            assert!(!completed_source_handoff(
                &changed_entry,
                &changed_source,
                previous,
                current
            ));
        }
    }

    fn achieved_restore(
        status: &mut KubericSetStatus,
        current: &ConfigurationDescriptor,
        peer: &mut ReplicaAgentSnapshot,
    ) -> RetainedReplicaGoal {
        let mut selected = select_retained_restore(status, current, peer)
            .unwrap()
            .unwrap();
        let observation = ReplicaGoalObservation {
            goal: selected.goal.clone(),
            status: ReplicaGoalStatus::Achieved,
            selected_call: None,
            receipts: 1,
            host_settled: true,
            completed_handoff: None,
        };
        selected.outcome = Some(observation.clone());
        peer.goals.push(observation);
        peer.publication = Some(ReplicaPublication {
            format: ORDINARY_ACTION_FORMAT,
            scope: selected.goal.scope.clone(),
            replication_address: "http://127.0.0.1:50054".into(),
            generation: 1,
            stage: ReplicaPublicationStage::AchievedScope,
            role: ReplicaRole::ActiveSecondary,
        });
        status.replica_goals.push(selected.clone());
        selected
    }

    fn raw(peer: &ReplicaAgentSnapshot) -> RawObservation {
        let mut set = KubericSet::new(
            "retained-controller",
            KubericSetSpec {
                hosting_format: ORDINARY_ACTION_FORMAT,
                replicas: 3,
                image: "test-only".into(),
                failover_delay_seconds: 1,
                switchover: None,
            },
        );
        set.metadata.uid = Some(peer.history.resource_uid.to_string());
        set.metadata.namespace = Some("default".into());
        RawObservation {
            set,
            pods: vec![Pod {
                metadata: ObjectMeta {
                    name: Some("member-2".into()),
                    uid: Some("pod-2".into()),
                    annotations: Some(BTreeMap::from([(
                        crate::crd::CONTROL_ADDRESS_ANNOTATION.into(),
                        "http://127.0.0.1:50051".into(),
                    )])),
                    ..Default::default()
                },
                spec: Some(PodSpec {
                    volumes: Some(vec![Volume {
                        name: "data".into(),
                        persistent_volume_claim: Some(PersistentVolumeClaimVolumeSource {
                            claim_name: "member-2-data".into(),
                            ..Default::default()
                        }),
                        ..Default::default()
                    }]),
                    ..Default::default()
                }),
                ..Default::default()
            }],
            pvcs: vec![PersistentVolumeClaim {
                metadata: ObjectMeta {
                    name: Some("member-2-data".into()),
                    uid: Some("pvc-2".into()),
                    ..Default::default()
                },
                ..Default::default()
            }],
            services: Vec::new(),
            secrets: Vec::new(),
            agents: BTreeMap::new(),
            failures: Vec::new(),
            now_unix_seconds: 100,
        }
    }

    #[test]
    fn retained_member_selects_direct_restore_without_progress_ready_or_quorum_change() {
        let (current, status, peer) = fixture();
        let selected = select_retained_restore(&status, &current, &peer)
            .unwrap()
            .unwrap();
        assert!(matches!(
            selected.goal.desired,
            ReplicaGoalKind::RestoreRetainedMember { .. }
        ));
        assert_eq!(selected.goal.revision, 9);
        assert_eq!(selected.goal.scope.storage, peer.storage);
        assert_eq!(
            selected.goal.scope.process_session_id,
            peer.process_session_id
        );
        assert_eq!(selected.goal.scope.creation_id, peer.creation_id);
        assert_eq!(selected.goal.scope.configuration.current, current);
        assert!(selected.goal.scope.configuration.previous.is_none());
        for description in &selected.goal.scope.configuration.replicas {
            assert_eq!(description.current_progress, INVALID_LSN);
            assert_eq!(description.catch_up_capability, INVALID_LSN);
            assert!(!description.is_up);
            assert!(!description.is_ready);
            assert!(!description.must_catchup);
        }
        assert!(peer.publication.is_none());
        assert!(peer.remote_operations.is_empty());
        assert!(peer.progress.value().is_none());
        assert_eq!(status.replica_goals[0].goal.revision, 4);
        selected.validate().unwrap();
    }

    #[test]
    fn retained_member_rejects_missing_crossed_and_unaccepted_historical_qualification() {
        let (current, status, peer) = fixture();
        let mut cases = Vec::new();
        let mut changed = peer.clone();
        changed.retained_member_qualification = None;
        cases.push(changed);
        let mut changed = peer.clone();
        changed
            .retained_member_qualification
            .as_mut()
            .unwrap()
            .admitted_revision += 1;
        cases.push(changed);
        let mut changed = peer.clone();
        changed
            .retained_member_qualification
            .as_mut()
            .unwrap()
            .admitted_operation_id = OperationId::new("unaccepted");
        cases.push(changed);
        let mut changed = peer.clone();
        changed.history.resource_uid = ResourceUid::new("other-history");
        cases.push(changed);
        let mut changed = peer.clone();
        changed.storage.storage_generation = StorageGeneration::new("other-generation");
        cases.push(changed);
        let mut changed = peer.clone();
        changed.storage.storage = ReplicaStorageIdentity::Persistent {
            pod_uid: PodUid::new("pod-2"),
            pvc_uid: PvcUid::new("other-pvc"),
        };
        cases.push(changed);
        let mut changed = peer.clone();
        changed.storage.replica.instance_id = ReplicaInstanceId::new("other-pod");
        cases.push(changed);
        let mut changed = peer.clone();
        changed.process_session_id = ProcessSessionId::new("old-2");
        cases.push(changed);
        let mut changed = peer.clone();
        changed.creation_id = ReplicatorCreationId::new("old-creation-2");
        cases.push(changed);
        let mut changed = peer.clone();
        changed.goals.clear();
        cases.push(changed);
        let mut changed = peer.clone();
        changed
            .retained_member_qualification
            .as_mut()
            .unwrap()
            .configuration
            .write_quorum = 1;
        cases.push(changed);
        for changed in cases {
            assert!(
                select_retained_restore(&status, &current, &changed)
                    .unwrap()
                    .is_none()
            );
        }
        for outcome in [
            None,
            Some(ReplicaGoalObservation {
                status: ReplicaGoalStatus::Failed("sticky historical error".into()),
                ..peer.goals[0].clone()
            }),
        ] {
            let mut changed = status.clone();
            changed.replica_goals[0].outcome = outcome;
            assert!(
                select_retained_restore(&changed, &current, &peer)
                    .unwrap()
                    .is_none()
            );
        }
        let mut changed = status.clone();
        changed.replica_goals[0].goal.scope.configuration.previous = Some(current.clone());
        assert!(
            select_retained_restore(&changed, &current, &peer)
                .unwrap()
                .is_none()
        );
        let mut changed = status.clone();
        changed.public_v1_topology.as_mut().unwrap().sessions[1].process_session_id =
            ProcessSessionId::new("crossed-predecessor");
        assert!(
            select_retained_restore(&changed, &current, &peer)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn retained_member_requires_unopened_secondary_and_no_selected_higher_epoch() {
        let (current, status, peer) = fixture();
        let selected = select_retained_restore(&status, &current, &peer)
            .unwrap()
            .unwrap();
        let mut changed = peer.clone();
        changed.publication = Some(ReplicaPublication {
            format: ORDINARY_ACTION_FORMAT,
            scope: selected.goal.scope.clone(),
            replication_address: "http://127.0.0.1:50054".into(),
            generation: 1,
            stage: ReplicaPublicationStage::DiscoveryPending,
            role: ReplicaRole::None,
        });
        assert!(
            select_retained_restore(&status, &current, &changed)
                .unwrap()
                .is_none()
        );
        let mut changed = peer.clone();
        changed.provider_writer_held = true;
        assert!(
            select_retained_restore(&status, &current, &changed)
                .unwrap()
                .is_none()
        );
        let mut changed = peer.clone();
        changed.withdrawn = true;
        assert!(
            select_retained_restore(&status, &current, &changed)
                .unwrap()
                .is_none()
        );
        let mut changed = peer.clone();
        let mut discover = selected.goal.clone();
        discover.desired = ReplicaGoalKind::Discover;
        discover.scope.configuration.replicas.clear();
        changed.goals.push(ReplicaGoalObservation {
            goal: discover.clone(),
            status: ReplicaGoalStatus::Pending,
            selected_call: None,
            receipts: 0,
            host_settled: false,
            completed_handoff: None,
        });
        assert!(
            select_retained_restore(&status, &current, &changed)
                .unwrap()
                .is_none()
        );
        let mut changed = status.clone();
        changed
            .replica_goals
            .push(RetainedReplicaGoal::new(discover).unwrap());
        assert!(
            select_retained_restore(&changed, &current, &peer)
                .unwrap()
                .is_none()
        );
        let mut changed = status.clone();
        let mut transition = selected;
        transition.goal.scope.configuration.current =
            switchover_configuration(&current, ReplicaId::new(3), 1).unwrap();
        transition.goal.scope.epoch = transition.goal.scope.configuration.current.epoch;
        transition.goal.scope.configuration.previous = Some(current.clone());
        changed.replica_goals.push(transition);
        assert!(
            select_retained_restore(&changed, &current, &peer)
                .unwrap()
                .is_none()
        );
        let mut changed = peer.clone();
        changed.storage = storage(1);
        changed.process_session_id = ProcessSessionId::new("fresh-1");
        changed.creation_id = ReplicatorCreationId::new("fresh-creation-1");
        changed
            .retained_member_qualification
            .as_mut()
            .unwrap()
            .predecessor_process_session_id = ProcessSessionId::new("old-1");
        changed
            .retained_member_qualification
            .as_mut()
            .unwrap()
            .predecessor_creation_id = ReplicatorCreationId::new("old-creation-1");
        assert!(
            select_retained_restore(&status, &current, &changed)
                .unwrap()
                .is_none()
        );
        let mut changed = peer;
        changed.accepted_revision_high_watermark = u64::MAX;
        assert!(select_retained_restore(&status, &current, &changed).is_err());
    }

    #[test]
    fn retained_member_revalidates_exact_selection_before_effects_and_completion() {
        let (current, mut status, mut peer) = fixture();
        let selected = achieved_restore(&mut status, &current, &mut peer);
        assert!(retained_restore_matches(
            &status,
            &current,
            &peer,
            &selected.goal
        ));
        assert!(retained_restore_achieved(&peer, &selected.goal));

        let mut crossed = selected.goal.clone();
        crossed.scope.configuration.replicas[0].current_progress = 41;
        assert!(!retained_restore_matches(
            &status, &current, &peer, &crossed
        ));
        let mut crossed = selected.goal.clone();
        crossed.scope.configuration.replicas[0].is_ready = true;
        assert!(!retained_restore_matches(
            &status, &current, &peer, &crossed
        ));
        let mut crossed = selected.goal.clone();
        crossed.scope.creation_id = ReplicatorCreationId::new("another-creation");
        assert!(!retained_restore_matches(
            &status, &current, &peer, &crossed
        ));

        let mut extra = selected.goal.clone();
        extra.operation_id = OperationId::new("other-fresh-work");
        extra.revision += 1;
        extra.desired = ReplicaGoalKind::Reconfigure {
            role: ReplicaRole::ActiveSecondary,
            catch_up: None,
        };
        let mut changed = peer.clone();
        changed.goals.push(ReplicaGoalObservation {
            goal: extra.clone(),
            status: ReplicaGoalStatus::Pending,
            selected_call: Some("ReplicatorRole".into()),
            receipts: 0,
            host_settled: false,
            completed_handoff: None,
        });
        assert!(!retained_restore_matches(
            &status,
            &current,
            &changed,
            &selected.goal
        ));
        let mut changed = status.clone();
        changed
            .replica_goals
            .push(RetainedReplicaGoal::new(extra.clone()).unwrap());
        assert!(!retained_restore_matches(
            &changed,
            &current,
            &peer,
            &selected.goal
        ));

        extra.desired = ReplicaGoalKind::Abort;
        peer.goals.push(ReplicaGoalObservation {
            goal: extra,
            status: ReplicaGoalStatus::Failed("actual terminal join error".into()),
            selected_call: None,
            receipts: 0,
            host_settled: false,
            completed_handoff: None,
        });
        assert!(!retained_restore_matches(
            &status,
            &current,
            &peer,
            &selected.goal
        ));
        assert!(!retained_restore_achieved(&peer, &selected.goal));
    }

    #[test]
    fn retained_member_requires_the_exact_still_live_pod_and_pvc_mount() {
        let (_, _, peer) = fixture();
        let observed = raw(&peer);
        assert!(retained_storage_present(&observed, &peer));
        let mut changed = observed.clone();
        changed.pvcs[0].metadata.uid = Some("other-pvc".into());
        assert!(!retained_storage_present(&changed, &peer));
        let mut changed = observed.clone();
        changed.pods[0].metadata.uid = Some("other-pod".into());
        assert!(!retained_storage_present(&changed, &peer));
        let mut changed = observed;
        changed.pods[0].spec.as_mut().unwrap().volumes = None;
        assert!(!retained_storage_present(&changed, &peer));
    }

    #[test]
    fn retained_member_adopts_only_achieved_exact_sample_and_preserves_old_errors() {
        let (current, mut status, mut peer) = fixture();
        let topology = status.public_v1_topology.as_mut().unwrap();
        topology.unavailable_primary_id = Some(current.primary_id);
        topology.unavailable_since_unix_seconds = Some(99);
        topology.completed_switchover_request_id = Some("older-request".into());
        let selected = achieved_restore(&mut status, &current, &mut peer);
        let mut old_goal = status.replica_goals[0].goal.clone();
        old_goal.operation_id = OperationId::new("unrelated-old-error");
        old_goal.scope.storage = storage(3);
        old_goal.scope.process_session_id = ProcessSessionId::new("old-3");
        old_goal.scope.creation_id = ReplicatorCreationId::new("old-creation-3");
        let description = old_goal
            .scope
            .configuration
            .replicas
            .iter_mut()
            .find(|description| description.identity.replica_id == ReplicaId::new(3))
            .unwrap();
        description.identity = peer.storage.replica.clone();
        description.storage_incarnation = peer.storage.clone();
        description.process_session_id = ProcessSessionId::new("old-2");
        description.replication_address = "http://127.0.0.1:50054".into();
        let mut old_error = RetainedReplicaGoal::new(old_goal).unwrap();
        old_error.outcome = Some(ReplicaGoalObservation {
            goal: old_error.goal.clone(),
            status: ReplicaGoalStatus::Uncertain("original unresolved result".into()),
            selected_call: None,
            receipts: 0,
            host_settled: false,
            completed_handoff: None,
        });
        status.replica_goals.push(old_error.clone());
        assert!(
            adopt_retained_member(&status, &current, &peer, &selected)
                .unwrap()
                .is_none()
        );
        for unavailable in [
            ProgressQueryResult::NotQueried("not sampled".into()),
            ProgressQueryResult::NotReady,
            ProgressQueryResult::Failed("real query error".into()),
            ProgressQueryResult::Value(INVALID_LSN),
        ] {
            peer.progress = ProgressQueryResult::Value(41);
            peer.capability = unavailable.clone();
            assert!(
                adopt_retained_member(&status, &current, &peer, &selected)
                    .unwrap()
                    .is_none()
            );
            peer.progress = unavailable;
            peer.capability = ProgressQueryResult::Value(0);
            assert!(
                adopt_retained_member(&status, &current, &peer, &selected)
                    .unwrap()
                    .is_none()
            );
        }
        peer.progress = ProgressQueryResult::Value(41);
        peer.capability = ProgressQueryResult::Value(0);
        let next = adopt_retained_member(&status, &current, &peer, &selected)
            .unwrap()
            .unwrap();
        let mut expected = status.public_v1_topology.clone().unwrap();
        expected.sessions[1] = endpoint(&peer, ReplicaRole::ActiveSecondary);
        assert_eq!(next.public_v1_topology.as_ref(), Some(&expected));
        assert_eq!(expected.configuration().unwrap(), current);
        assert_eq!(next.replica_goals, status.replica_goals);
        assert_eq!(next.replica_goals.last(), Some(&old_error));
        assert_eq!(
            expected.sessions[0],
            status.public_v1_topology.as_ref().unwrap().sessions[0]
        );
        assert!(peer.remote_operations.is_empty());

        let mut pending = selected.clone();
        pending.outcome = None;
        assert!(
            adopt_retained_member(&status, &current, &peer, &pending)
                .unwrap()
                .is_none()
        );
        let mut changed = peer.clone();
        changed.publication.as_mut().unwrap().stage = ReplicaPublicationStage::DiscoveryPending;
        assert!(
            adopt_retained_member(&status, &current, &changed, &selected)
                .unwrap()
                .is_none()
        );
        let mut changed = peer.clone();
        changed
            .publication
            .as_mut()
            .unwrap()
            .scope
            .epoch
            .configuration_number += 1;
        assert!(
            adopt_retained_member(&status, &current, &changed, &selected)
                .unwrap()
                .is_none()
        );
        let mut changed = peer.clone();
        changed.creation_id = ReplicatorCreationId::new("crossed-creation");
        assert!(
            adopt_retained_member(&status, &current, &changed, &selected)
                .unwrap()
                .is_none()
        );
        let mut changed = peer.clone();
        changed.publication.as_mut().unwrap().generation = 0;
        assert!(
            adopt_retained_member(&status, &current, &changed, &selected)
                .unwrap()
                .is_none()
        );
        let mut changed = peer.clone();
        changed.goals.last_mut().unwrap().status =
            ReplicaGoalStatus::Uncertain("initialization result is not success".into());
        assert!(
            adopt_retained_member(&status, &current, &changed, &selected)
                .unwrap()
                .is_none()
        );
        let mut changed = peer.clone();
        changed.retained_member_qualification = None;
        assert!(
            adopt_retained_member(&status, &current, &changed, &selected)
                .unwrap()
                .is_none()
        );
        let mut changed = peer;
        changed.withdrawn = true;
        assert!(
            adopt_retained_member(&status, &current, &changed, &selected)
                .unwrap()
                .is_none()
        );
    }

    struct RecoveryAgent {
        sample: ReplicaAgentSnapshot,
        request: Mutex<Option<proto::ReplicaSnapshotRequest>>,
    }

    fn cold_survivor_fixture() -> (
        ConfigurationDescriptor,
        KubericSetStatus,
        ReplicaAgentSnapshot,
    ) {
        let (current, mut status, mut peer) = fixture();
        let historical = &peer.goals[0].goal;
        peer.process_session_id = historical.scope.process_session_id.clone();
        peer.creation_id = historical.scope.creation_id.clone();
        peer.publication = Some(ReplicaPublication {
            format: ORDINARY_ACTION_FORMAT,
            scope: historical.scope.clone(),
            replication_address: "http://127.0.0.1:50054".into(),
            generation: 1,
            stage: ReplicaPublicationStage::AchievedScope,
            role: ReplicaRole::ActiveSecondary,
        });
        peer.progress = ProgressQueryResult::NotReady;
        peer.capability = ProgressQueryResult::Value(INVALID_LSN);
        status.public_v1_authority = Some(PublicV1AuthorityStatus {
            identity: peer.identity.clone(),
            history_initialization_id: HistoryInitializationId::new("unit-history-init"),
            history: peer.history.clone(),
            completed_operation_count: 0,
            canonical_authority_json: "{}".into(),
        });
        status
            .public_v1_topology
            .as_mut()
            .unwrap()
            .unavailable_primary_id = Some(current.primary_id);
        (current, status, peer)
    }

    #[test]
    fn cold_survivor_initialization_is_bounded_same_scope_and_never_a_sample() {
        let (current, mut status, peer) = cold_survivor_fixture();
        let original = peer.clone();
        let selected = select_cold_survivor_initialization(&status, &current, &peer)
            .unwrap()
            .unwrap();
        assert_eq!(selected.goal.scope.configuration.current, current);
        assert!(selected.goal.scope.configuration.previous.is_none());
        assert_eq!(selected.goal.scope.epoch, current.epoch);
        assert_eq!(selected.goal.scope.history, peer.history);
        assert_eq!(selected.goal.scope.storage, peer.storage);
        assert_eq!(
            selected.goal.scope.process_session_id,
            peer.process_session_id
        );
        assert_eq!(selected.goal.scope.creation_id, peer.creation_id);
        assert!(selected.outcome.is_none());
        let unavailable = selected
            .goal
            .scope
            .configuration
            .replicas
            .iter()
            .find(|description| description.identity.replica_id == current.primary_id)
            .unwrap();
        assert!(!unavailable.is_up && !unavailable.is_ready && !unavailable.must_catchup);
        assert_eq!(unavailable.current_progress, INVALID_LSN);
        assert_eq!(unavailable.catch_up_capability, INVALID_LSN);
        assert_eq!(peer, original);
        status.replica_goals.push(selected);
        assert!(
            select_cold_survivor_initialization(&status, &current, &peer)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn cold_survivor_initialization_rejects_nonqualifying_or_failed_owners() {
        let (current, status, peer) = cold_survivor_fixture();
        let mut cases = Vec::new();
        let mut changed = peer.clone();
        changed.progress = ProgressQueryResult::Value(41);
        changed.capability = ProgressQueryResult::Value(0);
        cases.push(changed);
        let mut changed = peer.clone();
        changed.progress = ProgressQueryResult::Failed("sticky native failure".into());
        cases.push(changed);
        let mut changed = peer.clone();
        changed.progress = ProgressQueryResult::NotQueried("not a native sample".into());
        cases.push(changed);
        let mut changed = peer.clone();
        changed.withdrawn = true;
        cases.push(changed);
        let mut changed = peer.clone();
        changed.process_session_id = ProcessSessionId::new("other-live-owner");
        cases.push(changed);
        let mut changed = peer.clone();
        changed.publication.as_mut().unwrap().stage = ReplicaPublicationStage::DiscoveryPending;
        cases.push(changed);
        let mut changed = peer.clone();
        changed
            .publication
            .as_mut()
            .unwrap()
            .scope
            .configuration
            .previous = Some(current.clone());
        cases.push(changed);
        let mut changed = peer.clone();
        changed.goals[0].host_settled = false;
        cases.push(changed);
        let mut changed = peer.clone();
        changed.goals[0].status = ReplicaGoalStatus::Uncertain("unsettled role".into());
        cases.push(changed);
        for changed in cases {
            assert!(
                select_cold_survivor_initialization(&status, &current, &changed)
                    .unwrap()
                    .is_none()
            );
        }
        let mut changed = status.clone();
        changed
            .public_v1_topology
            .as_mut()
            .unwrap()
            .unavailable_primary_id = None;
        assert!(
            select_cold_survivor_initialization(&changed, &current, &peer)
                .unwrap()
                .is_none()
        );
        changed = status.clone();
        changed
            .public_v1_authority
            .as_mut()
            .unwrap()
            .history
            .history_id = HistoryId::new("foreign-history");
        assert!(
            select_cold_survivor_initialization(&changed, &current, &peer)
                .unwrap()
                .is_none()
        );
    }

    #[async_trait::async_trait]
    impl AgentApi for RecoveryAgent {
        async fn get_snapshot(
            &self,
            endpoint: &str,
            token: &str,
            request: proto::ReplicaSnapshotRequest,
        ) -> std::result::Result<proto::ReplicaSnapshotResponse, AgentRpcError> {
            assert_eq!(endpoint, "http://127.0.0.1:50051");
            assert_eq!(token, "controller-test");
            *self.request.lock().unwrap() = Some(request);
            Ok(proto::ReplicaSnapshotResponse {
                protocol_version: ORDINARY_ACTION_FORMAT,
                canonical_json: serde_json::to_vec(&ReplicaAgentQueryResult::Initialized(
                    Box::new(self.sample.clone()),
                ))
                .unwrap(),
            })
        }

        async fn get_status(
            &self,
            _: &str,
            _: &str,
            _: proto::GetAgentStatusRequest,
        ) -> std::result::Result<proto::AgentStatusReport, AgentRpcError> {
            unreachable!("retained restoration uses canonical snapshots")
        }

        async fn execute(
            &self,
            _: &str,
            _: &str,
            _: proto::ExecuteCommandRequest,
        ) -> std::result::Result<proto::ExecuteCommandResponse, AgentRpcError> {
            unreachable!("retained restoration uses canonical goals")
        }
    }

    fn api(sample: ReplicaAgentSnapshot) -> KubeClusterApi<RecoveryAgent> {
        let client =
            kube::Client::try_from(kube::Config::new("http://127.0.0.1:1".parse().unwrap()))
                .unwrap();
        KubeClusterApi::new(
            client,
            Arc::new(RecoveryAgent {
                sample,
                request: Mutex::new(None),
            }),
            "controller-test",
            "default",
        )
        .unwrap()
    }

    #[tokio::test]
    async fn retained_member_queries_native_in_achieved_restore_scope_before_adoption() {
        let (current, mut status, mut peer) = fixture();
        let selected = achieved_restore(&mut status, &current, &mut peer);
        let mut actual = peer.clone();
        actual.progress = ProgressQueryResult::Value(41);
        actual.capability = ProgressQueryResult::Value(0);
        let api = api(actual);
        let sampled = api.sample_recovery(&raw(&peer), &peer).await.unwrap();
        let request = api.agents.request.lock().unwrap().clone().unwrap();
        assert!(request.sample_native_progress);
        assert_eq!(request.protocol_version, ORDINARY_ACTION_FORMAT);
        assert_eq!(
            request.expected_process_session_id,
            peer.process_session_id.as_str()
        );
        assert_eq!(request.resource_uid, peer.history.resource_uid.as_str());
        assert_eq!(request.target, Some(peer.storage.replica.clone().into()));
        assert_eq!(sampled.publication, peer.publication);
        assert_eq!(
            sampled.publication.as_ref().unwrap().scope.epoch,
            current.epoch
        );
        let adopted = adopt_retained_member(&status, &current, &sampled, &selected)
            .unwrap()
            .unwrap();
        assert_eq!(
            adopted.public_v1_topology.as_ref().unwrap().sessions[1],
            endpoint(&peer, ReplicaRole::ActiveSecondary)
        );
        assert_eq!(
            status
                .public_v1_topology
                .as_ref()
                .unwrap()
                .configuration()
                .unwrap(),
            current
        );
        assert_eq!(adopted.replica_goals, status.replica_goals);
    }

    #[tokio::test]
    async fn retained_member_rejects_crossed_sample_scope_before_adoption() {
        let (current, mut status, mut peer) = fixture();
        achieved_restore(&mut status, &current, &mut peer);
        let mut changed = peer.clone();
        changed
            .publication
            .as_mut()
            .unwrap()
            .scope
            .epoch
            .configuration_number += 1;
        let api = api(changed);
        assert!(matches!(
            api.sample_recovery(&raw(&peer), &peer).await,
            Err(ControllerError::ObservationStale)
        ));
    }
}
