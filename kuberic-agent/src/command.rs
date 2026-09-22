use kuberic_protocol::command::{EnsureConfiguration, InitializeAgentStore};
use kuberic_protocol::types::{ReplicaRole, TransitionKind};
use kuberic_protocol::validation::validate_transition_relationship;
use kuberic_runtime_internal::authority::AdmittedAuthority;

use crate::provisioning::{
    InitializationAuthority, ObservedStorageIdentity, authorize_initialization,
};
use crate::state::AgentState;
use crate::state::StorageIdentity;
use crate::{AgentError, Result};

pub fn admit_initialization(
    command: &InitializeAgentStore,
    observed: &ObservedStorageIdentity,
    authority: InitializationAuthority<'_>,
) -> Result<StorageIdentity> {
    authorize_initialization(command, observed, authority)
}

pub fn admit_configuration(
    command: &EnsureConfiguration,
    state: &AgentState,
) -> Result<AdmittedAuthority> {
    if command.operation_id.is_empty() {
        return Err(AgentError::CommandRejected(
            "operation ID must not be empty".into(),
        ));
    }
    let identity = &state.identity.local_identity;
    if command.local_replica_id != identity.replica_id
        || command.expected_instance_id != identity.instance_id
        || command.expected_agent_generation != identity.agent_generation
    {
        return Err(AgentError::CommandRejected(
            "command target does not match durable replica identity".into(),
        ));
    }
    if command.current_epoch != command.current_configuration.epoch {
        return Err(AgentError::CommandRejected(
            "current epoch differs from Current Configuration".into(),
        ));
    }
    if command
        .previous_configuration
        .as_ref()
        .map(|configuration| configuration.epoch)
        != command.previous_epoch
    {
        return Err(AgentError::CommandRejected(
            "previous epoch differs from Previous Configuration".into(),
        ));
    }
    if command.current_epoch < state.highest_epoch {
        return Err(AgentError::CommandRejected(
            "command epoch regresses durable authority".into(),
        ));
    }
    if command.effective_policy != state.identity.effective_policy {
        return Err(AgentError::CommandRejected(
            "command policy differs from initialized policy".into(),
        ));
    }
    validate_transition_relationship(
        command.transition_kind,
        command.previous_configuration.as_ref(),
        &command.current_configuration,
        &command.effective_policy,
    )
    .map_err(|error| AgentError::CommandRejected(error.to_string()))?;
    let admitted = AdmittedAuthority {
        local_identity: identity.clone(),
        transition_kind: Some(command.transition_kind),
        previous_configuration: command.previous_configuration.clone(),
        current_configuration: command.current_configuration.clone(),
    };
    admitted
        .validate()
        .map_err(|error| AgentError::CommandRejected(error.to_string()))?;
    if command.transition_kind == TransitionKind::Bootstrap
        && admitted.local_role() == ReplicaRole::None
    {
        return Err(AgentError::CommandRejected(
            "bootstrap authority must assign a runtime role".into(),
        ));
    }
    Ok(admitted)
}
