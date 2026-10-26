//! Common admission facts derived from accepted programs and achieved receipts.

use super::state::{
    AgentState, PublicInstruction as I, PublicInstructionOutcome as O,
    PublicOperationDisposition as D, PublicOperationRecord, PublicOperationStage as S,
    PublicV1OperationState,
};
use super::{HostError, Result, public_lifecycle};
use crate::protocol::public_operations::{
    PublicConfiguration, PublicOperationIntent, PublicOperationProgram as P,
};
use crate::protocol::types::{Epoch, ReplicaRole};

fn epoch(intent: &PublicOperationIntent) -> Option<Epoch> {
    intent
        .lifecycle
        .as_ref()
        .map(|input| input.epoch)
        .or_else(|| match intent.program.as_ref()? {
            P::AgentGoal(goal) => Some(goal.scope.epoch),
            P::Role { epoch, .. } | P::Epoch { epoch } | P::Swap { epoch, .. } => Some(*epoch),
            P::Configuration(c)
            | P::CatchUp {
                configuration: c, ..
            } => Some(c.current.epoch),
            P::Build(b) => Some(b.epoch),
            // Remove withdraws an immutable retained attempt, not its epoch's
            // authority. Registry revision/containment and store exact-attempt
            // checks still apply to the current command.
            P::Remove(_) => None,
            _ => None,
        })
}

pub(crate) fn achieved_role(
    record: &PublicOperationRecord,
    application: bool,
) -> Option<ReplicaRole> {
    let instructions = public_lifecycle::record_instructions(record);
    instructions
        .iter()
        .zip(&record.lifecycle.outcomes)
        .find_map(|(instruction, outcome)| {
            let is_child = if application {
                matches!(instruction, I::ApplicationRole | I::ApplicationPrimary)
                    && matches!(outcome, O::ApplicationRole(_))
            } else {
                matches!(instruction, I::ReplicatorRole | I::ReplicatorPrimary)
                    && matches!(outcome, O::Done | O::RetainedNativeInitialized(_))
            };
            if !is_child {
                return None;
            }
            if record.intent.lifecycle.is_some() {
                return Some(ReplicaRole::Primary);
            }
            match record.intent.program.as_ref()? {
                P::AgentGoal(goal) => goal.role(),
                P::Role { role, .. } => Some(*role),
                P::Swap { handoff, .. } => Some(*handoff),
                _ => None,
            }
        })
}

pub(crate) fn configuration_at(
    intent: &PublicOperationIntent,
    instruction: I,
) -> Option<PublicConfiguration> {
    if !matches!(
        instruction,
        I::Configuration
            | I::StartingConfiguration
            | I::RefreshedConfiguration
            | I::CurrentConfiguration
            | I::CatchUpConfiguration
            | I::CatchUp
            | I::FirstCatchUp
            | I::SecondCatchUp
    ) {
        return None;
    }
    let mut configuration = if let Some(input) = &intent.lifecycle {
        input.configuration()
    } else {
        let program = public_lifecycle::program::selected_program(intent, instruction)?;
        public_lifecycle::program::configuration(&program, instruction).clone()
    };
    if instruction == I::CurrentConfiguration
        || (instruction == I::CatchUp
            && intent
                .lifecycle
                .as_ref()
                .is_some_and(|input| input.data_loss_recovery.is_some()))
    {
        configuration.previous = None;
        configuration.replicas.retain_mut(|replica| {
            if let Some(member) = configuration
                .current
                .members
                .iter()
                .find(|member| member.identity == replica.identity)
            {
                replica.role = member.role;
                true
            } else {
                false
            }
        });
    }
    Some(configuration)
}

fn is_catchup(instruction: I) -> bool {
    matches!(instruction, I::CatchUp | I::FirstCatchUp | I::SecondCatchUp)
}

pub(crate) fn current_scope(
    preview: &PublicV1OperationState,
    process: &crate::protocol::types::ProcessSessionId,
    record: &PublicOperationRecord,
) -> bool {
    &record.intent.process_session_id == process
        && record.accepted_storage == preview.storage_incarnation
        && (record.accepted_history == preview.history
            || (record.stage == S::Completed
                && record.disposition == Some(D::Succeeded)
                && record.intent.lifecycle.as_ref()
                    .and_then(|input| input.data_loss_recovery.as_ref())
                    .is_some_and(|input| {
                        preview.data_loss_recovery.as_ref().is_some_and(|recovery| {
                            recovery.stage == crate::protocol::public_operations::PublicDataLossRecoveryStage::Admitted
                                && Some(&recovery.proposed_history) == preview.history.as_ref()
                                && recovery.attempt == input.attempt
                                && recovery.logical_authorization() == input.authorization
                        })
                    })))
}

/// Runs under the durable store transaction on both fresh admission and retry.
/// Exact completed replay remains immutable and is not new authority.
pub(crate) fn validate(state: &AgentState, intent: &PublicOperationIntent) -> Result<()> {
    if let Some(goal) = intent.goal() {
        super::ordinary_goal::validate_admission(state, goal)?;
    }
    if !intent.identity.is_public_v1() || intent.class.is_terminal() {
        return Ok(());
    }
    let Some(preview) = &state.public_v1_operation_state else {
        return Ok(());
    };
    if preview
        .operations
        .get(&intent.operation_id)
        .is_some_and(|record| record.intent == *intent && record.stage != S::RetryPending)
    {
        return Ok(());
    }
    let relevant = preview
        .operations
        .values()
        .filter(|record| {
            record.intent.operation_id != intent.operation_id
                && current_scope(preview, &intent.process_session_id, record)
        })
        .collect::<Vec<_>>();
    let accepted_epoch = relevant
        .iter()
        .filter_map(|record| epoch(&record.intent))
        .max();
    if let Some(incoming) = epoch(intent)
        && accepted_epoch.is_some_and(|accepted| accepted > incoming)
    {
        return Err(HostError::CommandRejected(
            "message epoch is older than accepted host authority".into(),
        ));
    }
    let installed =
        public_lifecycle::installed_configuration_for_state(state, &intent.process_session_id);
    let stages = public_lifecycle::program::operation_instructions(intent);
    if stages
        .iter()
        .filter_map(|instruction| configuration_at(intent, *instruction))
        .any(|configuration| {
            accepted_epoch.is_some_and(|accepted| configuration.current.epoch < accepted)
        })
    {
        return Err(HostError::CommandRejected(
            "selected configuration stage is older than accepted host authority".into(),
        ));
    }
    if stages
        .iter()
        .filter_map(|instruction| configuration_at(intent, *instruction))
        .any(|incoming| {
            incoming.previous.is_some()
                && installed.as_ref().is_some_and(|current| {
                    current.previous.is_none() && current.current.epoch == incoming.current.epoch
                        // Secondary installation is host transport authority,
                        // not an achieved primary engine configuration stage.
                        && current.current.primary_id == state.identity.local_identity.replica_id
                        && incoming.current.primary_id == state.identity.local_identity.replica_id
                        && !ordinary_ready_expansion(state, intent, current, &incoming)
                })
        })
    {
        return Err(HostError::CommandRejected(
            "joint reconfiguration cannot regress completed current-only installation".into(),
        ));
    }

    fn ordinary_ready_expansion(
        state: &crate::host::state::AgentState,
        intent: &crate::protocol::public_operations::PublicOperationIntent,
        installed: &crate::protocol::public_operations::PublicConfiguration,
        incoming: &crate::protocol::public_operations::PublicConfiguration,
    ) -> bool {
        use crate::protocol::public_operations::ReplicaGoalKind;
        let Some(goal) = intent.goal() else {
            return false;
        };
        if matches!(goal.desired, ReplicaGoalKind::RefreshPrimary { .. }) {
            let removed = installed
                .current
                .members
                .iter()
                .filter(|member| !incoming.current.members.contains(member))
                .collect::<Vec<_>>();
            let added = incoming
                .current
                .members
                .iter()
                .filter(|member| !installed.current.members.contains(member))
                .collect::<Vec<_>>();
            return incoming.current.epoch == installed.current.epoch
                && incoming.current.primary_id == installed.current.primary_id
                && incoming.current.write_quorum == installed.current.write_quorum
                && incoming.previous.as_ref().is_some_and(|previous| {
                    previous.epoch < incoming.current.epoch
                        && previous.primary_id == installed.current.primary_id
                        && previous.members == installed.current.members
                        && previous.write_quorum == installed.current.write_quorum
                })
                && removed.len() == 1
                && added.len() == 1
                && removed[0].role == crate::protocol::types::ReplicaRole::ActiveSecondary
                && added[0].role == crate::protocol::types::ReplicaRole::ActiveSecondary
                && removed[0].identity.replica_id == added[0].identity.replica_id
                && super::ordinary_goal::admission_ready(state, goal).is_ok_and(|ready| {
                    ready.len() == 1
                        && ready[0].target.scope.storage.replica == added[0].identity
                        && ready[0].source.scope.configuration.current == installed.current
                        && ready[0].source.scope.configuration.previous.is_none()
                });
        }
        matches!(
            goal.desired,
            ReplicaGoalKind::Reconfigure {
                role: crate::protocol::types::ReplicaRole::Primary,
                ..
            }
        ) && incoming.previous.as_ref() == Some(&installed.current)
            && incoming.current.members.len() > installed.current.members.len()
            && installed
                .current
                .members
                .iter()
                .all(|member| incoming.current.members.contains(member))
            && super::ordinary_goal::admission_ready(state, goal)
                .is_ok_and(|ready| !ready.is_empty())
    }
    if let Some(P::Swap {
        starting,
        controller_composed: false,
        ..
    }) = &intent.program
        // An unfinished demotion may have receipted the refreshed native
        // configuration before its role handoff. A successor must start there,
        // not from the last completed whole operation or an older request.
        && relevant
            .iter()
            .filter(|record| matches!(record.disposition, None | Some(D::Succeeded)))
            .filter_map(|record| {
                public_lifecycle::achieved_configuration_from_record(record)
                    .map(|configuration| (record.intent.revision, configuration))
            })
            .max_by_key(|(revision, _)| *revision)
            .map(|(_, configuration)| configuration)
            .or_else(|| installed.clone())
            .as_ref()
            .is_some_and(|current| {
                starting.current != current.current
                    || starting.previous != current.previous
                    || starting.replicas.len() != current.replicas.len()
                    || starting.replicas.iter().any(|peer| {
                        !current.replicas.iter().any(|known| {
                            peer.identity == known.identity
                                && peer.process_session_id == known.process_session_id
                                && peer.storage_incarnation == known.storage_incarnation
                                && peer.replication_address == known.replication_address
                        })
                    })
            })
    {
        return Err(HostError::CommandRejected(
            "Swap starting stage differs from achieved source authority".into(),
        ));
    }
    // A standalone current-only wait is a fresh synchronization barrier, not
    // native UpdateConfiguration's stale Catchup-flag message. Cancellation is
    // nevertheless sticky for that epoch, independent of a newer revision.
    if stages
        .iter()
        .filter(|instruction| is_catchup(**instruction))
        .filter_map(|instruction| configuration_at(intent, *instruction))
        .any(|incoming| {
            relevant.iter().any(|record| {
                record.disposition == Some(D::Cancelled)
                    && record
                        .lifecycle
                        .in_flight
                        .filter(|instruction| is_catchup(*instruction))
                        .and_then(|instruction| configuration_at(&record.intent, instruction))
                        .is_some_and(|cancelled| incoming.current.epoch <= cancelled.current.epoch)
            })
        })
    {
        return Err(HostError::CommandRejected(
            "cancelled catch-up requires a newer epoch".into(),
        ));
    }
    if let Some(P::Build(build)) = &intent.program {
        let child_is_primary = |application| {
            relevant
                .iter()
                .filter_map(|record| {
                    achieved_role(record, application).map(|role| (record.intent.revision, role))
                })
                .max_by_key(|(revision, _)| *revision)
                .is_some_and(|(_, role)| role == ReplicaRole::Primary)
        };
        let installed_matches = installed.as_ref().is_some_and(|c| {
            c.current.epoch == build.epoch
                && Some(&c.current.configuration_id) == build.current_configuration_id.as_ref()
                && c.current.members.iter().any(|member| {
                    member.identity == state.identity.local_identity
                        && member.role == ReplicaRole::Primary
                })
        });
        if !child_is_primary(false) || !child_is_primary(true) || !installed_matches {
            return Err(HostError::CommandRejected(
                "Build requires achieved paired Primary roles and exact installed authority".into(),
            ));
        }
    }
    Ok(())
}
