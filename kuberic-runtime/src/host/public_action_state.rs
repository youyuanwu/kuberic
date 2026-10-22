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
            P::Role { epoch, .. } | P::Epoch { epoch } | P::Swap { epoch, .. } => Some(*epoch),
            P::Configuration(c)
            | P::CatchUp {
                configuration: c, ..
            } => Some(c.current.epoch),
            P::Build(b) | P::Remove(b) => Some(b.epoch),
            _ => None,
        })
}

fn achieved_role(record: &PublicOperationRecord, application: bool) -> Option<ReplicaRole> {
    let instructions = public_lifecycle::program::operation_instructions(&record.intent);
    instructions
        .iter()
        .zip(&record.lifecycle.outcomes)
        .find_map(|(instruction, outcome)| {
            let is_child = if application {
                matches!(instruction, I::ApplicationRole | I::ApplicationPrimary)
                    && matches!(outcome, O::ApplicationRole(_))
            } else {
                matches!(instruction, I::ReplicatorRole | I::ReplicatorPrimary)
                    && matches!(outcome, O::Done)
            };
            if !is_child {
                return None;
            }
            if record.intent.lifecycle.is_some() {
                return Some(ReplicaRole::Primary);
            }
            match record.intent.program.as_ref()? {
                P::Role { role, .. } => Some(*role),
                P::Swap { handoff, .. } => Some(*handoff),
                _ => None,
            }
        })
}

fn configuration_at(intent: &PublicOperationIntent, instruction: I) -> Option<PublicConfiguration> {
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
        public_lifecycle::program::configuration(intent.program.as_ref()?, instruction).clone()
    };
    if instruction == I::CurrentConfiguration {
        configuration.previous = None;
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
    if let Some(incoming) = epoch(intent)
        && relevant
            .iter()
            .filter_map(|record| epoch(&record.intent))
            .any(|accepted| accepted > incoming)
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
        .any(|incoming| {
            incoming.previous.is_some()
                && installed.as_ref().is_some_and(|current| {
                    current.previous.is_none() && current.current.epoch == incoming.current.epoch
                        // Secondary installation is host transport authority,
                        // not an achieved primary engine configuration stage.
                        && current.current.primary_id == state.identity.local_identity.replica_id
                        && incoming.current.primary_id == state.identity.local_identity.replica_id
                })
        })
    {
        return Err(HostError::CommandRejected(
            "joint reconfiguration cannot regress completed current-only installation".into(),
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
