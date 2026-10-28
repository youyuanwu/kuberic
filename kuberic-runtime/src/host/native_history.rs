use std::sync::{Arc, Mutex};

use crate::host::state::{
    AgentState, NativeActionInput, PublicInstructionOutcome as O, PublicOperationDisposition,
    PublicOperationRecord, PublicOperationStage,
};
use crate::host::{HostError, Result};
use crate::protocol::public_operations::*;
use crate::protocol::types::{Epoch, ReplicaIdentity, ReplicaRole};
use crate::replicator::v1::LifecycleObservation;

use super::public_lifecycle::PublicLifecycleCallbacks;
use super::store::AgentStore;

#[derive(Default)]
struct Live {
    observation: Option<LifecycleObservation>,
    application_role: Option<ReplicaRole>,
    state: Option<NativeReplicaState>,
    applied: Option<Epoch>,
}

/// One actual creation owns this state. It is never hydrated from a journal.
#[derive(Default)]
pub(crate) struct NativeLifecycleOwner(Mutex<Live>);

pub(crate) type PeerAddress = Arc<dyn Fn(&ReplicaIdentity) -> crate::Result<String> + Send + Sync>;

fn missing(message: &str) -> HostError {
    HostError::CommandRejected(format!("missing native lifecycle contract: {message}"))
}

impl NativeLifecycleOwner {
    pub(crate) fn observe(&self, observation: LifecycleObservation) {
        let mut live = self.0.lock().unwrap();
        match observation {
            LifecycleObservation::EpochApplied(epoch) => live.applied = Some(epoch),
            LifecycleObservation::TransportQuorum { .. } => {}
            LifecycleObservation::Opened => {
                live.observation = Some(observation);
                live.state = Some(NativeReplicaState::StandBy);
            }
            LifecycleObservation::Ready(_) => {
                if live.observation != Some(observation) {
                    live.state = Some(NativeReplicaState::InBuild);
                }
                live.observation = Some(observation);
            }
            LifecycleObservation::Closed | LifecycleObservation::Faulted => {
                live.observation = Some(observation);
                live.state = None;
            }
        }
    }

    pub(crate) fn application_role(&self, role: ReplicaRole) {
        self.0.lock().unwrap().application_role = Some(role);
    }

    pub(crate) fn is_native(&self) -> bool {
        self.0.lock().unwrap().observation.is_some()
    }

    pub(crate) fn applied(&self, epoch: Epoch) -> bool {
        self.0.lock().unwrap().applied == Some(epoch)
    }

    pub(crate) fn matches(&self, facts: &NativeLifecycleFacts) -> bool {
        let live = self.0.lock().unwrap();
        live.observation == Some(LifecycleObservation::Ready(facts.native_role))
            && live.state == Some(facts.state)
            && live.application_role == facts.application_role
            && live.applied == facts.applied_primary_fence
    }

    pub(crate) fn status_is_live(&self, status: &NativeStatusObservation) -> bool {
        let live = self.0.lock().unwrap();
        if status.input.state == NativeReplicaState::StandBy {
            live.observation == Some(LifecycleObservation::Opened)
        } else {
            live.observation == Some(LifecycleObservation::Ready(status.input.native_role))
                && live.applied == Some(status.input.scope.epoch)
        }
    }

    fn facts(
        &self,
        goal: &ReplicaGoal,
        deactivation: Option<NativeDeactivationInfo>,
    ) -> Result<NativeLifecycleFacts> {
        let live = self.0.lock().unwrap();
        let native_role = match live.observation {
            Some(LifecycleObservation::Opened) => ReplicaRole::None,
            Some(LifecycleObservation::Ready(role)) => role,
            _ => return Err(missing("actual native owner is not opened/available")),
        };
        Ok(NativeLifecycleFacts {
            operation_id: goal.operation_id.clone(),
            revision: goal.revision,
            scope: goal.scope.clone(),
            state: live.state.ok_or_else(|| missing("proxy lifecycle state"))?,
            native_role,
            application_role: live.application_role,
            applied_primary_fence: live.applied,
            deactivation,
        })
    }

    fn ready_primary(&self, goal: &ReplicaGoal) -> Result<()> {
        let mut live = self.0.lock().unwrap();
        if live.observation != Some(LifecycleObservation::Ready(ReplicaRole::Primary))
            || live.application_role != Some(ReplicaRole::Primary)
            || live.applied != Some(goal.scope.epoch)
        {
            return Err(missing(
                "ReportAnyDataLoss requires actual paired Primary and epoch",
            ));
        }
        let data_loss = goal.scope.configuration.current.primary_id
            == goal.scope.storage.replica.replica_id
            && goal
                .scope
                .configuration
                .previous
                .as_ref()
                .is_some_and(|pc| {
                    pc.epoch.data_loss_number
                        != goal.scope.configuration.current.epoch.data_loss_number
                });
        if data_loss {
            return Err(missing(
                "selected PC/CC requires explicit OnDataLoss handling; no reset was invoked",
            ));
        }
        live.state = Some(NativeReplicaState::Ready);
        Ok(())
    }
}

fn successful(record: &PublicOperationRecord) -> bool {
    record.stage == PublicOperationStage::Completed
        && record.disposition == Some(PublicOperationDisposition::Succeeded)
        && record.lifecycle.in_flight.is_none()
}

fn same_owner(a: &ReplicaScope, b: &ReplicaScope) -> bool {
    a.history == b.history
        && a.storage == b.storage
        && a.process_session_id == b.process_session_id
        && a.creation_id == b.creation_id
}

fn same_configuration(a: &ReplicaScope, b: &ReplicaScope) -> bool {
    a.configuration.current == b.configuration.current
        && a.configuration.previous == b.configuration.previous
}

pub(crate) fn status_for(
    state: &AgentState,
    scope: &ReplicaScope,
) -> Option<NativeStatusObservation> {
    state
        .public_v1_operation_state
        .as_ref()?
        .operations
        .values()
        .filter(|record| successful(record))
        .flat_map(|record| &record.lifecycle.outcomes)
        .filter_map(|outcome| match outcome {
            O::NativeStatus(status)
                if same_owner(&status.input.scope, scope)
                    && status.input.scope.epoch == scope.epoch
                    && same_configuration(&status.input.scope, scope) =>
            {
                Some(status.as_ref())
            }
            _ => None,
        })
        .max_by_key(|status| status.input.revision)
        .cloned()
}

pub(crate) fn lifecycle_for(
    state: &AgentState,
    scope: &ReplicaScope,
) -> Option<NativeLifecycleFacts> {
    state
        .public_v1_operation_state
        .as_ref()?
        .operations
        .values()
        .filter(|record| {
            successful(record)
                && record.intent.goal().is_none_or(|goal| {
                    super::public_lifecycle::program::ordinary_handoff(goal).is_none()
                        || full_normal_record(record)
                })
        })
        .flat_map(|record| &record.lifecycle.outcomes)
        .filter_map(|outcome| match outcome {
            O::NativeLifecycle(facts) if same_owner(&facts.scope, scope) => Some(facts.as_ref()),
            _ => None,
        })
        .max_by_key(|facts| facts.revision)
        .cloned()
}

fn full_normal_record(record: &PublicOperationRecord) -> bool {
    use super::state::PublicInstruction as I;
    record.ordinary.as_ref().is_some_and(|ordinary| {
        ordinary.action_format == ORDINARY_ACTION_FORMAT
            && ordinary.accepted
            && ordinary.host_settled
            && ordinary.native_input.is_some()
            && ordinary.actions
                == [
                    I::Revoke,
                    I::ProgramEpoch,
                    I::RefreshedConfiguration,
                    I::SecondCatchUp,
                    I::ReplicatorRole,
                    I::ApplicationRole,
                    I::NativeComplete,
                ]
            && matches!(
                record.lifecycle.outcomes.as_slice(),
                [
                    O::Done,
                    O::Done,
                    O::Done,
                    O::Done,
                    O::Done,
                    O::ApplicationRole(_),
                    O::NativeLifecycle(_),
                ]
            )
    })
}

pub(crate) fn configuration_prefix(
    state: &AgentState,
    scope: &ReplicaScope,
    owner: &NativeLifecycleOwner,
) -> Option<NativeLifecycleFacts> {
    use super::state::PublicInstruction as I;
    let record = state
        .public_v1_operation_state
        .as_ref()?
        .operations
        .values()
        .filter(|record| {
            record
                .intent
                .goal()
                .is_some_and(|goal| same_owner(&goal.scope, scope))
        })
        .max_by_key(|record| record.intent.revision)?;
    let goal = record.intent.goal()?;
    let ordinary = record.ordinary.as_ref()?;
    let input = ordinary.native_input.as_ref()?;
    let instruction = record.lifecycle.in_flight?;
    let index = ordinary
        .actions
        .iter()
        .position(|action| *action == instruction)?;
    let mut expected = super::public_lifecycle::program::operation_instructions(&goal.intent());
    if (super::original_member::round(state, goal)
        || super::ordinary_goal::normal_current_owner(state, goal).is_some())
        && matches!(
            goal.desired,
            ReplicaGoalKind::Reconfigure {
                role: ReplicaRole::Primary,
                catch_up: Some(PublicCatchUpMode::WriteQuorum),
            }
        )
        && goal.scope.configuration.previous.is_none()
    {
        expected = super::original_member::final_actions(goal);
    }
    if ordinary.already_open {
        if !matches!(
            goal.desired,
            ReplicaGoalKind::Prepare { .. } | ReplicaGoalKind::Discover
        ) || !super::ordinary_goal::opened_creation(state, goal)
        {
            return None;
        }
        expected.retain(|action| !matches!(action, I::ApplicationOpen | I::ReplicatorOpen));
    }
    if goal.scope != *scope
        || !matches!(
            goal.desired,
            ReplicaGoalKind::Reconfigure { .. }
                | ReplicaGoalKind::RefreshPrimary { .. }
                | ReplicaGoalKind::RecoverContainedCreation { .. }
        )
        || record.disposition.is_some()
        || record.superseded_by.is_some()
        || !matches!(
            instruction,
            I::Configuration
                | I::CurrentConfiguration
                | I::StartingConfiguration
                | I::RefreshedConfiguration
        )
        || ordinary.action_format != ORDINARY_ACTION_FORMAT
        || ordinary.actions != expected
        || record.lifecycle.outcomes.len() != index
        || record.lifecycle.outcomes.iter().any(|outcome| {
            !matches!(
                outcome,
                O::Done | O::ApplicationRole(_) | O::NativeLifecycle(_)
            )
        })
        || validate_record(state, record).is_err()
    {
        return None;
    }
    let facts = owner
        .facts(goal, input.lifecycle.deactivation.clone())
        .ok()?;
    (facts.native_role == ReplicaRole::Primary
        && facts.applied_primary_fence == Some(goal.scope.epoch))
    .then_some(facts)
}

pub(crate) fn configuration_status(
    state: &AgentState,
    facts: &NativeLifecycleFacts,
) -> Option<NativeStatusObservation> {
    state
        .public_v1_operation_state
        .as_ref()?
        .operations
        .get(&facts.operation_id)?
        .ordinary
        .as_ref()?
        .native_input
        .as_ref()?
        .promotion
        .clone()
}

pub(crate) fn validate_record(state: &AgentState, record: &PublicOperationRecord) -> Result<()> {
    let Some(input) = record
        .ordinary
        .as_ref()
        .and_then(|ordinary| ordinary.native_input.as_ref())
    else {
        if record
            .lifecycle
            .outcomes
            .iter()
            .any(|outcome| matches!(outcome, O::NativeStatus(_) | O::NativeLifecycle(_)))
        {
            return Err(missing("native receipt has no selected input"));
        }
        return Ok(());
    };
    let goal = record
        .intent
        .goal()
        .ok_or_else(|| missing("native input has no ordinary goal"))?;
    if input.lifecycle.scope != goal.scope
        || input.lifecycle.operation_id != goal.operation_id
        || input.lifecycle.revision != goal.revision
    {
        return Err(missing(
            "native input round/history/configuration/lifetime changed",
        ));
    }
    if let Some(selected) = &input.promotion {
        let captured = state
            .public_v1_operation_state
            .as_ref()
            .and_then(|preview| preview.operations.get(&selected.input.operation_id))
            .filter(|query| successful(query) && query.intent.revision < goal.revision)
            .is_some_and(|query| {
                query.lifecycle.outcomes.iter().any(|outcome| {
                matches!(outcome, O::NativeStatus(actual) if actual.as_ref() == selected)
            })
            });
        if !captured
            || !same_owner(&selected.input.scope, &goal.scope)
            || !same_configuration(&selected.input.scope, &goal.scope)
            || selected.input.scope.epoch != goal.scope.epoch
        {
            return Err(missing(
                "promotion query is not its immutable actual earlier result",
            ));
        }
    }
    for outcome in &record.lifecycle.outcomes {
        match outcome {
            O::NativeStatus(status) if status.input != input.lifecycle => {
                return Err(missing("status result crossed its selected input"));
            }
            O::NativeLifecycle(facts)
                if facts.scope != goal.scope
                    || facts.operation_id != goal.operation_id
                    || facts.revision != goal.revision =>
            {
                return Err(missing("lifecycle receipt crossed its selected input"));
            }
            _ => {}
        }
    }
    Ok(())
}

fn durable_history(state: &AgentState, scope: &ReplicaScope) -> Option<NativeDeactivationInfo> {
    state
        .public_v1_operation_state
        .as_ref()?
        .operations
        .values()
        .filter(|record| successful(record))
        .flat_map(|record| &record.lifecycle.outcomes)
        .filter_map(|outcome| match outcome {
            O::NativeLifecycle(facts)
                if facts.scope.history == scope.history && facts.scope.storage == scope.storage =>
            {
                facts.deactivation.as_ref()
            }
            _ => None,
        })
        .max_by_key(|info| (info.primary_fence, info.catch_up_lsn))
        .cloned()
}

async fn source(
    state: &AgentState,
    goal: &ReplicaGoal,
    callbacks: &PublicLifecycleCallbacks,
) -> Result<Option<NativeLifecycleFacts>> {
    let Some(primary) = goal
        .scope
        .configuration
        .current
        .members
        .iter()
        .find(|member| {
            member.role == ReplicaRole::Primary && member.identity != goal.scope.storage.replica
        })
    else {
        return Ok(None);
    };
    let (Some(address), Some(credentials)) = (&callbacks.peer_address, &callbacks.peer_credentials)
    else {
        return Ok(None);
    };
    let address = address(&primary.identity)?;
    if address.is_empty() {
        return Ok(None);
    }
    let snapshot = crate::control::resolve_native_snapshot(
        &address,
        credentials,
        &goal.scope.history,
        &primary.identity,
    )
    .await?;
    let Some(facts) = snapshot.native_lifecycle else {
        return Ok(None);
    };
    let publication = snapshot.publication.as_ref();
    if snapshot.withdrawn
        || snapshot.history != goal.scope.history
        || facts.scope.storage != snapshot.storage
        || facts.scope.process_session_id != snapshot.process_session_id
        || facts.scope.creation_id != snapshot.creation_id
        || facts.scope.epoch != goal.scope.epoch
        || facts.state != NativeReplicaState::Ready
        || facts.native_role != ReplicaRole::Primary
        || facts.application_role != Some(ReplicaRole::Primary)
        || facts.deactivation.is_none()
        || !publication.is_some_and(|offer| {
            offer.stage == ReplicaPublicationStage::AchievedScope
                && offer.role == ReplicaRole::Primary
                && same_owner(&offer.scope, &facts.scope)
                && same_configuration(&offer.scope, &facts.scope)
        })
        || !(facts.scope.configuration.current == goal.scope.configuration.current
            || goal.scope.configuration.previous.as_ref()
                == Some(&facts.scope.configuration.current))
    {
        return Err(missing(
            "source history crossed its actual Ready owner/configuration",
        ));
    }
    if let Some(local) = super::ordinary_goal::normal_current_secondary(state, goal) {
        let current = snapshot
            .goals
            .iter()
            .find(|entry| {
                entry.goal.operation_id == facts.operation_id
                    && entry.goal.revision == facts.revision
                    && entry.goal.scope == facts.scope
                    && entry.status == ReplicaGoalStatus::Achieved
                    && entry.host_settled
                    && entry.selected_call.is_none()
                    && entry.receipts == 8
                    && entry.goal.desired
                        == (ReplicaGoalKind::Reconfigure {
                            role: ReplicaRole::Primary,
                            catch_up: Some(PublicCatchUpMode::WriteQuorum),
                        })
                    && entry.goal.scope.configuration.previous.is_none()
            })
            .ok_or_else(|| {
                missing("normal final secondary lacks actual completed current Primary")
            })?;
        if !snapshot.goals.iter().any(|entry| {
            let joint = &entry.goal;
            entry.status == ReplicaGoalStatus::Achieved
                && entry.host_settled
                && entry.selected_call.is_none()
                && entry.receipts == 7
                && joint.validate().is_ok()
                && joint.revision < current.goal.revision
                && same_owner(&joint.scope, &facts.scope)
                && joint.scope.epoch == goal.scope.epoch
                && joint.scope.configuration.current == goal.scope.configuration.current
                && joint.scope.configuration.previous == local.scope.configuration.previous
                && joint.desired == current.goal.desired
                && snapshot.goals.iter().any(|prep| {
                    prep.status == ReplicaGoalStatus::Achieved
                        && prep.host_settled
                        && prep.selected_call.is_none()
                        && prep.goal.revision < joint.revision
                        && super::ordinary_goal::same_fence_swap(&prep.goal, joint)
                })
        }) || facts.applied_primary_fence != Some(goal.scope.epoch)
            || facts.scope.configuration.previous.is_some()
            || facts.deactivation.as_ref().is_none_or(|boundary| {
                boundary.primary_fence != goal.scope.epoch || boundary.catch_up_lsn < 0
            })
        {
            return Err(missing(
                "normal final secondary lost the actual selected joint/current Primary chain",
            ));
        }
    }
    Ok(Some(facts))
}

pub(super) async fn validate_normal_current_source(
    state: &AgentState,
    goal: &ReplicaGoal,
    callbacks: &PublicLifecycleCallbacks,
) -> Result<()> {
    if let Some(joint) = super::ordinary_goal::normal_current_owner(state, goal)
        .filter(|joint| joint.native_role == ReplicaRole::Primary)
    {
        let captured = state
            .public_v1_operation_state
            .as_ref()
            .and_then(|preview| preview.operations.get(&joint.operation_id))
            .and_then(|record| record.ordinary.as_ref())
            .and_then(|ordinary| ordinary.native_input.as_ref())
            .and_then(|input| input.source.as_ref())
            .ok_or_else(|| missing("normal final Primary lost its actual source input"))?;
        let address = callbacks
            .peer_address
            .as_ref()
            .ok_or_else(|| missing("normal final source resolver"))?(
            &captured.scope.storage.replica,
        )?;
        let snapshot = crate::control::resolve_native_snapshot(
            &address,
            callbacks
                .peer_credentials
                .as_ref()
                .ok_or_else(|| missing("normal final source credentials"))?,
            &goal.scope.history,
            &captured.scope.storage.replica,
        )
        .await?;
        let actual = snapshot
            .native_lifecycle
            .as_ref()
            .ok_or_else(|| missing("normal final source lost its current native owner"))?;
        if snapshot.withdrawn
            || snapshot.history != captured.scope.history
            || snapshot.storage != captured.scope.storage
            || snapshot.process_session_id != captured.scope.process_session_id
            || snapshot.creation_id != captured.scope.creation_id
            || !same_owner(&actual.scope, &captured.scope)
            || actual.state != NativeReplicaState::Ready
            || actual.native_role != ReplicaRole::ActiveSecondary
            || actual.application_role != Some(ReplicaRole::ActiveSecondary)
            || actual.applied_primary_fence != Some(goal.scope.epoch)
            || actual.scope.epoch != goal.scope.epoch
            || actual.deactivation != captured.deactivation
            || actual.scope.configuration.current != goal.scope.configuration.current
            || !(actual.scope.configuration.previous == captured.scope.configuration.previous
                || actual.scope.configuration.previous.is_none())
            || !snapshot.publication.as_ref().is_some_and(|publication| {
                publication.stage == ReplicaPublicationStage::AchievedScope
                    && publication.role == ReplicaRole::ActiveSecondary
                    && publication.scope == actual.scope
                    && publication.generation == actual.revision
            })
            || !snapshot.goals.iter().any(|entry| {
                entry.goal.operation_id == captured.operation_id
                    && entry.goal.revision == captured.revision
                    && entry.goal.scope == captured.scope
                    && entry.goal.validate().is_ok()
                    && entry.status == ReplicaGoalStatus::Achieved
                    && entry.host_settled
                    && entry.selected_call.is_none()
                    && entry.receipts == 7
                    && entry.goal.desired
                        == (ReplicaGoalKind::Reconfigure {
                            role: ReplicaRole::ActiveSecondary,
                            catch_up: Some(PublicCatchUpMode::WriteQuorum),
                        })
            })
        {
            return Err(missing(
                "normal final Primary crossed its full same-live source result",
            ));
        }
    }
    if super::ordinary_goal::normal_current_secondary(state, goal).is_some()
        && source(state, goal, callbacks).await?.is_none()
    {
        return Err(missing(
            "normal final secondary lost its selected live Primary",
        ));
    }
    Ok(())
}

async fn normal_source(
    state: &AgentState,
    goal: &ReplicaGoal,
    callbacks: &PublicLifecycleCallbacks,
) -> Result<Option<NativeLifecycleFacts>> {
    if goal.role() != Some(ReplicaRole::Primary)
        || !state
            .public_v1_operation_state
            .as_ref()
            .is_some_and(|preview| {
                preview.operations.values().any(|record| {
                    successful(record)
                        && record
                            .intent
                            .goal()
                            .is_some_and(|old| super::ordinary_goal::same_fence_swap(old, goal))
                })
            })
    {
        return Ok(None);
    }
    let original = goal
        .scope
        .configuration
        .previous
        .as_ref()
        .ok_or_else(|| missing("normal transfer PC"))?;
    let source = original
        .members
        .iter()
        .find(|member| member.identity.replica_id == original.primary_id)
        .ok_or_else(|| missing("normal transfer old source"))?;
    let (Some(address), Some(credentials)) = (&callbacks.peer_address, &callbacks.peer_credentials)
    else {
        return Err(missing(
            "normal transfer authenticated source snapshot route",
        ));
    };
    let snapshot = crate::control::resolve_native_snapshot(
        &address(&source.identity)?,
        credentials,
        &goal.scope.history,
        &source.identity,
    )
    .await?;
    let result = snapshot
        .normal_handoff_result()
        .ok_or_else(|| missing("normal transfer full same-live settled source result"))?;
    if result.goal.scope.history != goal.scope.history
        || result.goal.scope.configuration.current != goal.scope.configuration.current
        || result.goal.scope.configuration.previous != goal.scope.configuration.previous
        || result.goal.scope.epoch != goal.scope.epoch
        || goal.scope.configuration.current.primary_id != goal.scope.storage.replica.replica_id
    {
        return Err(missing(
            "normal transfer crossed selected target/history/PC/CC/fence",
        ));
    }
    Ok(snapshot.native_lifecycle)
}

pub(crate) async fn select(
    store: &dyn AgentStore,
    record: &PublicOperationRecord,
    callbacks: &PublicLifecycleCallbacks,
) -> Result<Option<NativeActionInput>> {
    let Some(owner) = callbacks.native.as_ref().filter(|owner| owner.is_native()) else {
        return Ok(None);
    };
    let goal = record.intent.goal().expect("ordinary native selection");
    let state = store.load_state().await?;
    if let Some(input) = state
        .public_v1_operation_state
        .as_ref()
        .and_then(|preview| preview.operations.get(&goal.operation_id))
        .and_then(|record| record.ordinary.as_ref())
        .and_then(|ordinary| ordinary.native_input.as_ref())
    {
        if input.lifecycle.scope != goal.scope
            || input.lifecycle.operation_id != goal.operation_id
            || input.lifecycle.revision != goal.revision
        {
            return Err(missing("selected native input crossed its round"));
        }
        return Ok(Some(input.clone()));
    }
    let lifecycle = owner.facts(goal, durable_history(&state, &goal.scope))?;
    if lifecycle
        .applied_primary_fence
        .is_some_and(|epoch| epoch > goal.scope.epoch)
    {
        return Err(missing(
            "selected primary fence precedes the actual applied fence",
        ));
    }
    let input = NativeActionInput {
        native_rebuild: if super::original_member::restart_close(goal) {
            Some(Box::new(select_rebuild(&state, goal, callbacks).await?))
        } else {
            None
        },
        fresh_initial_primary: goal.role() == Some(ReplicaRole::Primary)
            && lifecycle.state == NativeReplicaState::StandBy
            && (record
                .ordinary
                .as_ref()
                .is_some_and(|ordinary| ordinary.open_existing == Some(false))
                || state
                    .public_v1_operation_state
                    .as_ref()
                    .is_some_and(|preview| {
                        preview.operations.values().any(|opened| {
                            successful(opened)
                                && opened
                                    .intent
                                    .goal()
                                    .is_some_and(|old| same_owner(&old.scope, &goal.scope))
                                && opened
                                    .ordinary
                                    .as_ref()
                                    .is_some_and(|ordinary| ordinary.open_existing == Some(false))
                                && opened
                                    .lifecycle
                                    .outcomes
                                    .iter()
                                    .any(|outcome| outcome.endpoint().is_some())
                        })
                    }))
            && goal.scope.configuration.previous.is_none(),
        lifecycle: lifecycle.clone(),
        promotion: status_for(&state, &goal.scope),
        source: if (goal.role() == Some(ReplicaRole::IdleSecondary)
            && goal.scope.configuration.previous.as_ref().is_none_or(|pc| {
                !pc.members
                    .iter()
                    .any(|member| member.identity == goal.scope.storage.replica)
            }))
            || (goal.role() == Some(ReplicaRole::ActiveSecondary)
                && (lifecycle.native_role == ReplicaRole::IdleSecondary
                    || lifecycle.state == NativeReplicaState::Ready
                        && goal.scope.configuration.previous.is_none()
                        && (super::original_member::round(&state, goal)
                            || super::ordinary_goal::normal_current_secondary(&state, goal)
                                .is_some())))
        {
            source(&state, goal, callbacks).await?
        } else {
            normal_source(&state, goal, callbacks).await?
        },
    };
    if goal.role() == Some(ReplicaRole::Primary)
        && let Some(source) = &input.source
        && input.promotion.as_ref().is_none_or(|status| {
            status.progress.value().is_none_or(|last| {
                source
                    .deactivation
                    .as_ref()
                    .is_none_or(|boundary| last < boundary.catch_up_lsn)
            })
        })
    {
        return Err(missing(
            "normal marked target GetLSN precedes the final source boundary",
        ));
    }
    if goal.role() == Some(ReplicaRole::Primary)
        && input.lifecycle.native_role != ReplicaRole::Primary
        && !input.fresh_initial_primary
    {
        if input
            .lifecycle
            .deactivation
            .as_ref()
            .is_none_or(|history| history.primary_fence >= goal.scope.epoch)
        {
            return Err(missing(
                "promotion requires existing lifecycle history and a newer selected primary fence",
            ));
        }
        if input.promotion.as_ref().is_none_or(|status| {
            !matches!(status.progress, ProgressQueryResult::Value(value) if value >= 0)
                || !matches!(status.capability, ProgressQueryResult::Value(value) if value >= 0)
                || matches!(status.epoch, NativeInspectionEpoch::Failed(_))
                || status.input.deactivation != input.lifecycle.deactivation
        }) {
            return Err(missing(
                "promotion has no exact successful GetLSN/history input round",
            ));
        }
    }
    store.select_native_input(&record.intent, &input).await?;
    Ok(Some(input))
}

async fn select_rebuild(
    state: &AgentState,
    goal: &ReplicaGoal,
    callbacks: &PublicLifecycleCallbacks,
) -> Result<NativeOutOfRangeRebuild> {
    let target = status_for(state, &goal.scope)
        .ok_or_else(|| missing("native rebuild requires its actual target GetLSN"))?;
    let primary = goal
        .scope
        .configuration
        .current
        .members
        .iter()
        .find(|member| member.role == ReplicaRole::Primary)
        .ok_or_else(|| missing("native rebuild selected source"))?;
    let address =
        callbacks
            .peer_address
            .as_ref()
            .ok_or_else(|| missing("native rebuild source resolver"))?(&primary.identity)?;
    let snapshot = crate::control::resolve_native_snapshot(
        &address,
        callbacks
            .peer_credentials
            .as_ref()
            .ok_or_else(|| missing("native rebuild credentials"))?,
        &goal.scope.history,
        &primary.identity,
    )
    .await?;
    let source = snapshot
        .publication
        .as_ref()
        .ok_or_else(|| missing("native rebuild source scope"))?;
    let facts = snapshot
        .native_lifecycle
        .as_ref()
        .ok_or_else(|| missing("native rebuild Ready source"))?;
    let status = snapshot
        .native_status
        .as_ref()
        .ok_or_else(|| missing("native rebuild source GetLSN"))?;
    let settled = |entry: &&ReplicaGoalObservation| {
        entry.status == ReplicaGoalStatus::Achieved
            && entry.host_settled
            && entry.selected_call.is_none()
    };
    let source_inspection = snapshot
        .goals
        .iter()
        .filter(settled)
        .find(|entry| {
            entry.goal.operation_id == status.input.operation_id
                && entry.goal.revision == status.input.revision
                && entry.goal.scope == status.input.scope
                && matches!(entry.goal.desired, ReplicaGoalKind::Discover)
        })
        .ok_or_else(|| missing("native rebuild accepted source query"))?;
    let target_record = state
        .public_v1_operation_state
        .as_ref()
        .unwrap()
        .operations
        .get(&target.input.operation_id)
        .filter(|record| {
            successful(record)
                && record
                    .ordinary
                    .as_ref()
                    .is_some_and(|ordinary| ordinary.accepted && ordinary.host_settled)
        })
        .ok_or_else(|| missing("native rebuild accepted target query"))?;
    let (Some(source_last), Some(source_first), Some(target_last)) = (
        status.progress.value(),
        status.capability.value(),
        target.progress.value(),
    ) else {
        return Err(missing("native rebuild known range"));
    };
    if snapshot.withdrawn
        || source.stage != ReplicaPublicationStage::AchievedScope
        || source.role != ReplicaRole::Primary
        || facts.scope != source.scope
        || facts.state != NativeReplicaState::Ready
        || facts.native_role != ReplicaRole::Primary
        || facts.application_role != Some(ReplicaRole::Primary)
        || facts.applied_primary_fence != Some(goal.scope.epoch)
        || !same_configuration(&source.scope, &goal.scope)
        || source.scope.epoch != goal.scope.epoch
        || source.scope.history != goal.scope.history
        || status.input.state != NativeReplicaState::Ready
        || status.input.native_role != ReplicaRole::Primary
        || status_failure(status).is_some()
        || target.input.state != NativeReplicaState::Ready
        || target.input.native_role != ReplicaRole::ActiveSecondary
        || target.input.application_role != Some(ReplicaRole::ActiveSecondary)
        || status_failure(&target).is_some()
        || source_last < 0
        || target_last < 0
        || source_first < 0
        || target_last >= source_last
        || source_first > 0 && source_first <= target_last.saturating_add(1)
    {
        return Err(missing("actual native out-of-range restart predicate"));
    }
    Ok(NativeOutOfRangeRebuild {
        source: source.clone(),
        source_inspection: Box::new(source_inspection.goal.clone()),
        target_inspection: Box::new(target_record.intent.goal().unwrap().clone()),
        source_last,
        source_first,
        target_last,
    })
}

fn observe(result: crate::Result<i64>) -> ProgressQueryResult {
    match result {
        Ok(value) if value >= 0 => ProgressQueryResult::Value(value),
        Ok(_) | Err(crate::RuntimeError::ProgressNotReady) => ProgressQueryResult::NotReady,
        Err(error) => ProgressQueryResult::Failed(error.to_string()),
    }
}

pub(crate) async fn inspect(
    input: &NativeActionInput,
    callbacks: &PublicLifecycleCallbacks,
) -> NativeStatusObservation {
    let mut result = NativeStatusObservation {
        input: input.lifecycle.clone(),
        epoch: NativeInspectionEpoch::StandBy,
        progress: ProgressQueryResult::NotQueried("epoch/status has not completed".into()),
        capability: ProgressQueryResult::NotQueried("epoch/status has not completed".into()),
    };
    if input.lifecycle.state != NativeReplicaState::StandBy {
        if input.lifecycle.applied_primary_fence == Some(input.lifecycle.scope.epoch) {
            result.epoch = NativeInspectionEpoch::AlreadyApplied;
        } else {
            match callbacks
                .replicator
                .update_epoch(input.lifecycle.scope.epoch)
                .await
            {
                Ok(())
                    if callbacks.native.as_ref().is_some_and(|owner| {
                        owner.0.lock().unwrap().applied == Some(input.lifecycle.scope.epoch)
                    }) =>
                {
                    result.epoch = NativeInspectionEpoch::Applied
                }
                Ok(()) => {
                    result.epoch = NativeInspectionEpoch::Failed(
                        "native owner did not confirm actual epoch application".into(),
                    );
                    return result;
                }
                Err(error) => {
                    result.epoch = NativeInspectionEpoch::Failed(error.to_string());
                    return result;
                }
            }
        }
    }
    result.progress = observe(callbacks.replicator.current_progress().await);
    if !matches!(result.progress, ProgressQueryResult::Value(_)) {
        return result;
    }
    result.capability = if input.lifecycle.state == NativeReplicaState::StandBy {
        ProgressQueryResult::Value(0)
    } else {
        observe(callbacks.replicator.catch_up_capability().await)
    };
    result
}

pub(crate) fn status_failure(status: &NativeStatusObservation) -> Option<String> {
    match (&status.epoch, &status.progress, &status.capability) {
        (NativeInspectionEpoch::Failed(error), _, _)
        | (_, ProgressQueryResult::Failed(error), _)
        | (_, _, ProgressQueryResult::Failed(error)) => Some(error.clone()),
        (_, ProgressQueryResult::NotReady, _) | (_, _, ProgressQueryResult::NotReady) => {
            Some("actual native GetStatus is not ready".into())
        }
        _ => None,
    }
}

pub(crate) fn decision(goal: &ReplicaGoal, callbacks: &PublicLifecycleCallbacks) -> Result<O> {
    let Some(owner) = callbacks.native.as_ref().filter(|owner| owner.is_native()) else {
        return Ok(O::NativeUnavailable);
    };
    owner.ready_primary(goal)?;
    Ok(O::Done)
}

pub(crate) async fn demotion_status(
    epoch: Epoch,
    callbacks: &PublicLifecycleCallbacks,
) -> Result<i64> {
    let owner = callbacks
        .native
        .as_ref()
        .ok_or_else(|| missing("native owner"))?;
    {
        let live = owner.0.lock().unwrap();
        if live.observation != Some(LifecycleObservation::Ready(ReplicaRole::ActiveSecondary))
            || live.application_role != Some(ReplicaRole::ActiveSecondary)
            || live.applied != Some(epoch)
        {
            return Err(missing(
                "post-demotion GetStatus lacks actual paired demotion/epoch",
            ));
        }
    }
    let boundary = callbacks.replicator.current_progress().await?;
    callbacks.replicator.catch_up_capability().await?;
    {
        let live = owner.0.lock().unwrap();
        if live.observation != Some(LifecycleObservation::Ready(ReplicaRole::ActiveSecondary))
            || live.application_role != Some(ReplicaRole::ActiveSecondary)
            || live.applied != Some(epoch)
        {
            return Err(missing(
                "native owner changed during post-demotion GetStatus",
            ));
        }
    }
    if boundary < 0 {
        return Err(missing("post-demotion GetStatus returned unknown progress"));
    }
    Ok(boundary)
}

pub(crate) async fn complete(
    input: &NativeActionInput,
    goal: &ReplicaGoal,
    callbacks: &PublicLifecycleCallbacks,
) -> Result<NativeLifecycleFacts> {
    let owner = callbacks
        .native
        .as_ref()
        .ok_or_else(|| missing("native owner"))?;
    let mut history = input.lifecycle.deactivation.clone();
    if super::public_lifecycle::program::ordinary_handoff(goal).is_some() {
        let boundary = demotion_status(goal.scope.epoch, callbacks).await?;
        history = Some(NativeDeactivationInfo {
            primary_fence: goal.scope.epoch,
            catch_up_lsn: boundary,
        });
        owner.0.lock().unwrap().state = Some(NativeReplicaState::Ready);
    } else if goal.role() == Some(ReplicaRole::Primary) {
        if input.fresh_initial_primary {
            history = Some(NativeDeactivationInfo {
                primary_fence: goal.scope.epoch,
                catch_up_lsn: 0,
            });
        } else if matches!(
            goal.desired,
            ReplicaGoalKind::Reconfigure {
                catch_up: Some(_),
                ..
            } | ReplicaGoalKind::RefreshPrimary { catch_up: Some(_) }
        ) && history
            .as_ref()
            .is_some_and(|history| history.primary_fence < goal.scope.epoch)
        {
            let status = input
                .promotion
                .as_ref()
                .ok_or_else(|| missing("promotion GetLSN"))?;
            let ProgressQueryResult::Value(boundary) = status.progress else {
                return Err(missing("promotion input progress"));
            };
            history = Some(NativeDeactivationInfo {
                primary_fence: goal.scope.epoch,
                catch_up_lsn: boundary,
            });
        }
    } else if let Some(source) = &input.source {
        let actual = owner.facts(goal, history.clone())?;
        if Some(actual.native_role) != goal.role() || actual.application_role != goal.role() {
            return Err(missing(
                "history propagation requires actual paired native/application roles",
            ));
        }
        let new_idle = goal.role() == Some(ReplicaRole::IdleSecondary)
            && goal.scope.configuration.previous.as_ref().is_none_or(|pc| {
                !pc.members
                    .iter()
                    .any(|member| member.identity == goal.scope.storage.replica)
            });
        let active = goal.role() == Some(ReplicaRole::ActiveSecondary)
            && (input.lifecycle.native_role == ReplicaRole::IdleSecondary
                || input.lifecycle.state == NativeReplicaState::Ready);
        if active {
            // These are actual native queue queries, not provider fallback.
            if callbacks.replicator.current_progress().await? < 0 {
                return Err(missing("ready secondary progress"));
            }
            callbacks.replicator.catch_up_capability().await?;
            owner.0.lock().unwrap().state = Some(NativeReplicaState::Ready);
        }
        if (new_idle || active)
            && source.deactivation.as_ref().is_some_and(|next| {
                history.as_ref().is_none_or(|old| {
                    (next.primary_fence, next.catch_up_lsn) > (old.primary_fence, old.catch_up_lsn)
                })
            })
        {
            history = source.deactivation.clone();
        }
    }
    owner.facts(goal, history)
}
