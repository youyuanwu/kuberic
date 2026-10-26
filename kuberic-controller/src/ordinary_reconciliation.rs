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
    for member in previous
        .into_iter()
        .chain(std::iter::once(current))
        .flat_map(|set| &set.members)
    {
        if member.identity.replica_id != local {
            members.insert(member.identity.replica_id, member);
        }
    }
    let replicas = members
        .into_iter()
        .map(|(id, member)| {
            let peer = peers
                .get(&id)
                .ok_or_else(|| invalid("configuration peer is unavailable"))?;
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

fn achieved(entry: &RetainedReplicaGoal) -> bool {
    entry
        .outcome
        .as_ref()
        .is_some_and(|outcome| outcome.status == ReplicaGoalStatus::Achieved)
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

impl<A: AgentApi + 'static> KubeClusterApi<A> {
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
        let peers = snapshots(raw);
        let desired = (1..=raw.set.spec.replicas)
            .map(|id| ReplicaId::new(i64::from(id)))
            .collect::<Vec<_>>();
        let current = status
            .public_v1_topology
            .as_ref()
            .map(PublicV1TopologyState::configuration)
            .transpose()
            .map_err(invalid)?;

        if crate::reconciler::bootstrap_resources_missing(raw, &desired, current.as_ref()) {
            self.ensure_replica_support(raw).await?;
            self.ensure_replica_scaffolding(raw, &desired).await?;
            return Ok(action(ReconcileKind::Executed));
        }
        let uid = ResourceUid::new(
            raw.set
                .uid()
                .ok_or_else(|| invalid("missing resource UID"))?,
        );
        if let Some(command) =
            crate::reconciler::next_initialization_command(raw, &uid, &desired, current.as_ref())?
        {
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
                if observation.status == ReplicaGoalStatus::Achieved {
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
                entry.outcome = Some(observation.clone());
                changed = true;
            } else {
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
                let mut configuration = previous.goal.scope.configuration.clone();
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
            })
        }) {
            return Err(invalid(
                "ordinary goal failed or is uncertain; creation settlement/replacement is required",
            ));
        }
        if raw.set.spec.switchover.is_some() {
            return Err(invalid(
                "ordinary M1 planned primary replacement is unavailable; creation withdrawal is required",
            ));
        }
        if peers.values().any(|peer| current_terminal(peer)) {
            return Ok(action(ReconcileKind::Waiting));
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
            return Ok(action(ReconcileKind::Waiting));
        }
        let primary = peers.get(&primary_id).ok_or_else(|| invalid("retained-history election requires fresh recovery/progress evidence; M1 cannot elect an unavailable primary"))?;
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
                    ReplicaGoalKind::Prepare { role },
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
                                                role: ReplicaRole::ActiveSecondary
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
                if !primary.remote_operations.iter().any(|remote| {
                    remote.selection.source == *source_publication
                        && remote.selection.target == *target_publication
                        && remote.state == RemoteReplicaState::Ready
                        && remote.accepted
                }) {
                    if status.replica_goals.iter().any(|entry| {
                        matches!(&entry.goal.desired, ReplicaGoalKind::Build { remote }
                            if remote.target.scope.storage.replica.replica_id == *id)
                    }) {
                        return Err(invalid(
                            "retained Build cannot be reused after target/source publication changed",
                        ));
                    }
                    let target_control_address = peer_address(raw, &target.storage.replica)?;
                    let remote = RemoteReplicaSelection {
                        source: source_publication.clone(),
                        target: target_publication.clone(),
                        target_control_address,
                        generation: 1,
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
