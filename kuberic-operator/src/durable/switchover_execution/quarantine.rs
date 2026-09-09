use kuberic_durable_execution::ExactBytes;

use crate::durable::{
    OperationObservations, effects::command_generation_change_proves_no_admission,
};

use super::{
    activities::EffectObservation,
    model::DirectSwitchoverDefinition,
    prepare::{DirectActivity, DirectEvaluation},
};

pub enum DirectQuarantineOutcome {
    Observe(ExactBytes),
    AwaitEvidence,
}

pub fn resolve_direct_quarantine(
    activity: &DirectActivity,
    definition: &DirectSwitchoverDefinition,
    observations: &OperationObservations,
    now: i64,
) -> Result<DirectQuarantineOutcome, String> {
    match activity.evaluate(definition, observations, now)? {
        DirectEvaluation::Observe(result) => Ok(DirectQuarantineOutcome::Observe(result)),
        DirectEvaluation::AwaitEvidence => Ok(DirectQuarantineOutcome::AwaitEvidence),
        DirectEvaluation::DispatchReplica { .. } => {
            let command = activity.prepared_replica_command().ok_or_else(|| {
                "quarantined direct replica activity has no persisted exact command".to_string()
            })?;
            let Some(observed) = observations.get(&command.target_id) else {
                return Ok(DirectQuarantineOutcome::AwaitEvidence);
            };
            if command_generation_change_proves_no_admission(
                &command.expected_agent_generation,
                &command.action_id,
                &observed.status,
            ) {
                return activity
                    .encode_effect_observation(EffectObservation::ProvenNoAdmission {
                        observed_at_unix_seconds: now,
                    })
                    .map(DirectQuarantineOutcome::Observe);
            }
            Ok(DirectQuarantineOutcome::AwaitEvidence)
        }
        DirectEvaluation::DispatchLabel => {
            if activity.prepared_label_command().is_none() {
                return Err(
                    "quarantined direct label activity has no persisted exact command".to_string(),
                );
            }
            // A UID-fenced label patch is never redelivered. Only an exact
            // observed postcondition can resolve the exposed command.
            Ok(DirectQuarantineOutcome::AwaitEvidence)
        }
    }
}
