use crate::protocol::public_operations::*;
use crate::protocol::types::ReplicaRole;
use crate::replicator::{ReplicaSetConfiguration, ReplicatorFactoryContext};
use crate::{Result, RuntimeError};

use super::engine_session::{Binding, Endpoint};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ConfigurationInvocation {
    goal: ReplicaGoal,
    call: String,
    publication: ReplicaPublication,
    current: ReplicaSetConfiguration,
    previous: Option<ReplicaSetConfiguration>,
}

impl ConfigurationInvocation {
    pub(super) fn operation(&self) -> &crate::protocol::types::OperationId {
        &self.goal.operation_id
    }

    pub(super) async fn invocation_for_binding(
        context: &ReplicatorFactoryContext,
        binding: &Binding,
    ) -> Result<ReplicaGoal> {
        let snapshot = context
            .native_operation_snapshot(binding.history(), &binding.primary().identity)
            .await?;
        let goal = selected_configuration(&snapshot)?;
        if !exact_owner(&snapshot, &binding.source)
            || goal.scope != binding.source.scope
            || goal.revision != binding.source.generation
        {
            return Err(reject());
        }
        Ok(goal.clone())
    }

    pub(super) fn matches(&self, binding: &Binding, goal: &ReplicaGoal) -> bool {
        self.goal == *goal && binding.source == self.publication
    }

    pub(super) fn goal(&self) -> &ReplicaGoal {
        &self.goal
    }

    pub(super) fn call(&self) -> &str {
        &self.call
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct CheckedTransfer {
    binding: Binding,
    predecessor: Option<Binding>,
    normal: ReplicaGoal,
    source: ReplicaGoal,
    receiver: ReplicaGoal,
    boundary: i64,
}

impl CheckedTransfer {
    pub(super) fn predecessor(&self, binding: &Binding) -> Result<&Binding> {
        if self.binding != *binding {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        self.predecessor
            .as_ref()
            .ok_or(RuntimeError::AuthorityNotAdmitted)
    }
}

fn reject() -> RuntimeError {
    RuntimeError::AuthorityNotAdmitted
}

fn exact_owner(snapshot: &ReplicaAgentSnapshot, publication: &ReplicaPublication) -> bool {
    !snapshot.withdrawn
        && snapshot.format == ORDINARY_ACTION_FORMAT
        && snapshot.history == publication.scope.history
        && snapshot.storage == publication.scope.storage
        && snapshot.process_session_id == publication.scope.process_session_id
        && snapshot.creation_id == publication.scope.creation_id
        && snapshot.publication.as_ref() == Some(publication)
}

fn selected_configuration(source: &ReplicaAgentSnapshot) -> Result<&ReplicaGoal> {
    let selected = source
        .goals
        .iter()
        .filter(|result| {
            result.goal.scope.process_session_id == source.process_session_id
                && result.goal.scope.creation_id == source.creation_id
        })
        .max_by_key(|result| result.goal.revision)
        .ok_or_else(reject)?;
    let goal = &selected.goal;
    goal.validate().map_err(|_| reject())?;
    let facts = source.native_lifecycle.as_ref().ok_or_else(|| {
        RuntimeError::ControlPlane(format!(
            "configuration {} has no current native prefix",
            goal.operation_id
        ))
    })?;
    if selected.status != ReplicaGoalStatus::Pending
        || !matches!(
            selected.selected_call.as_deref(),
            Some(
                "Configuration"
                    | "CurrentConfiguration"
                    | "StartingConfiguration"
                    | "RefreshedConfiguration"
            )
        )
        || facts.operation_id != goal.operation_id
        || facts.revision != goal.revision
        || facts.scope != goal.scope
        || facts.native_role != ReplicaRole::Primary
        || facts.applied_primary_fence != Some(goal.scope.epoch)
    {
        return Err(RuntimeError::ControlPlane(format!(
            "configuration {} does not own current prefix: status={:?}, call={:?}, native={:?}, applied={:?}",
            goal.operation_id,
            selected.status,
            selected.selected_call,
            facts.native_role,
            facts.applied_primary_fence,
        )));
    }
    Ok(goal)
}

pub(super) async fn invocation(
    context: &ReplicatorFactoryContext,
    source: &ReplicaPublication,
    current: &ReplicaSetConfiguration,
    previous: Option<&ReplicaSetConfiguration>,
) -> Result<ConfigurationInvocation> {
    let snapshot = context
        .native_operation_snapshot(&source.scope.history, &source.scope.storage.replica)
        .await?;
    let goal = selected_configuration(&snapshot)?;
    let call = snapshot
        .goals
        .iter()
        .find(|selected| selected.goal == *goal)
        .and_then(|selected| selected.selected_call.clone())
        .ok_or_else(reject)?;
    let (selected_current, selected_previous) =
        crate::host::public_lifecycle::selected_replica_sets(goal, &call)?;
    let mut scope = goal.scope.clone();
    scope.configuration.current = selected_current.configuration.clone();
    for peer in &mut scope.configuration.replicas {
        if let Some(member) = selected_current
            .configuration
            .members
            .iter()
            .find(|member| member.identity == peer.identity)
        {
            peer.role = member.role;
        }
    }
    if !exact_owner(&snapshot, source)
        || scope != source.scope
        || goal.revision != source.generation
        || *current != selected_current
        || previous != selected_previous.as_ref()
    {
        return Err(reject());
    }

    Ok(ConfigurationInvocation {
        goal: goal.clone(),
        call,
        publication: source.clone(),
        current: current.clone(),
        previous: previous.cloned(),
    })
}

pub(super) async fn check(
    context: &ReplicatorFactoryContext,
    binding: &Binding,
    predecessor: Option<&Binding>,
) -> Result<CheckedTransfer> {
    let source = context
        .native_operation_snapshot(binding.history(), &binding.primary().identity)
        .await?;
    let receiver = context
        .native_operation_snapshot(binding.history(), &binding.secondary().identity)
        .await?;
    let previous = binding
        .source
        .scope
        .configuration
        .previous
        .as_ref()
        .ok_or_else(reject)?;
    let old = previous
        .members
        .iter()
        .find(|member| member.identity.replica_id == previous.primary_id)
        .ok_or_else(reject)?;
    let normal = context
        .native_operation_snapshot(binding.history(), &old.identity)
        .await?;
    check_snapshots(binding, predecessor, &source, &receiver, &normal)
}

pub(super) fn check_snapshots(
    binding: &Binding,
    predecessor: Option<&Binding>,
    source: &ReplicaAgentSnapshot,
    receiver: &ReplicaAgentSnapshot,
    old_source: &ReplicaAgentSnapshot,
) -> Result<CheckedTransfer> {
    binding.validate()?;
    let goal = selected_configuration(source)?;
    let normal = old_source.normal_handoff_result().ok_or_else(reject)?;
    let receiver_goal = if predecessor.is_none() {
        receiver.normal_handoff_result().ok_or_else(reject)?
    } else {
        receiver
            .goals
            .iter()
            .filter(|result| {
                result.goal.scope.process_session_id == receiver.process_session_id
                    && result.goal.scope.creation_id == receiver.creation_id
            })
            .max_by_key(|result| result.goal.revision)
            .ok_or_else(reject)?
    };
    let boundary = old_source
        .native_lifecycle
        .as_ref()
        .and_then(|facts| facts.deactivation.as_ref())
        .ok_or_else(reject)?
        .catch_up_lsn;
    let status = source.native_status.as_ref().ok_or_else(reject)?;
    let receiver_facts = receiver.native_lifecycle.as_ref().ok_or_else(reject)?;
    if !binding.reformation
        || binding.recovery
        || binding.continuation.is_some()
        || binding.live_target.is_some()
        || binding.replacement().is_some()
        || !exact_owner(source, &binding.source)
        || !exact_owner(receiver, &binding.target)
        || goal.scope != binding.source.scope
        || goal.revision != binding.source.generation
        || normal.goal.scope.history != *binding.history()
        || normal.goal.scope.epoch != binding.epoch()
        || normal.goal.scope.configuration.current != goal.scope.configuration.current
        || normal.goal.scope.configuration.previous != goal.scope.configuration.previous
        || normal.goal.scope.configuration.current.primary_id
            != binding.primary().identity.replica_id
        || binding.primary() == binding.secondary()
        || binding.target.stage != ReplicaPublicationStage::AchievedScope
        || binding.target.role != ReplicaRole::ActiveSecondary
        || receiver_goal.goal.validate().is_err()
        || receiver_goal.goal.scope != binding.target.scope
        || receiver_goal.goal.revision != binding.target.generation
        || receiver_goal.status != ReplicaGoalStatus::Achieved
        || !receiver_goal.host_settled
        || receiver_goal.selected_call.is_some()
        || (predecessor.is_some()
            && receiver_goal.goal.desired
                != (ReplicaGoalKind::Reconfigure {
                    role: ReplicaRole::ActiveSecondary,
                    catch_up: None,
                }))
        || (predecessor.is_none()
            && (receiver_goal != normal
                || receiver.storage != old_source.storage
                || receiver.process_session_id != old_source.process_session_id
                || receiver.creation_id != old_source.creation_id))
        || receiver_facts.operation_id != receiver_goal.goal.operation_id
        || receiver_facts.revision != receiver_goal.goal.revision
        || receiver_facts.scope != receiver_goal.goal.scope
        || receiver_facts.state != NativeReplicaState::Ready
        || receiver_facts.native_role != ReplicaRole::ActiveSecondary
        || receiver_facts.application_role != Some(ReplicaRole::ActiveSecondary)
        || receiver_facts.applied_primary_fence != Some(binding.epoch())
        || status.input.scope.history != goal.scope.history
        || status.input.scope.storage != goal.scope.storage
        || status.input.scope.process_session_id != goal.scope.process_session_id
        || status.input.scope.creation_id != goal.scope.creation_id
        || status.input.scope.epoch != goal.scope.epoch
        || status.input.scope.configuration.current != goal.scope.configuration.current
        || status.input.scope.configuration.previous != goal.scope.configuration.previous
        || status.input.revision >= goal.revision
        || status.input.state != NativeReplicaState::Ready
        || status.input.native_role != ReplicaRole::ActiveSecondary
        || !matches!(
            status.epoch,
            NativeInspectionEpoch::Applied | NativeInspectionEpoch::AlreadyApplied
        )
        || status.progress.value().is_none_or(|last| last < boundary)
        || !source.goals.iter().any(|query| {
            query.goal.operation_id == status.input.operation_id
                && query.goal.revision == status.input.revision
                && query.goal.scope == status.input.scope
                && matches!(query.goal.desired, ReplicaGoalKind::Discover)
                && query.status == ReplicaGoalStatus::Achieved
                && query.host_settled
                && query.selected_call.is_none()
        })
    {
        return Err(reject());
    }
    if let Some(old) = predecessor {
        let original = normal
            .goal
            .scope
            .configuration
            .previous
            .as_ref()
            .ok_or_else(reject)?;
        let refreshed = crate::protocol::types::ConfigurationDescriptor::new(
            binding.epoch(),
            original.primary_id,
            original.members.clone(),
            original.write_quorum,
        );
        if old.primary()
            != Endpoint::publication(old_source.publication.as_ref().ok_or_else(reject)?)
            || old.secondary() != binding.secondary()
            || old.history() != binding.history()
            || old.epoch() != binding.epoch()
            || old.source.generation != normal.goal.revision
            || old.source.scope.configuration.previous.as_ref() != Some(original)
            || old.source.scope.configuration.current != refreshed
        {
            return Err(reject());
        }
    }
    Ok(CheckedTransfer {
        binding: binding.clone(),
        predecessor: predecessor.cloned(),
        normal: normal.goal.clone(),
        source: goal.clone(),
        receiver: receiver_goal.goal.clone(),
        boundary,
    })
}
