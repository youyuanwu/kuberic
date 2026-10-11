use std::cmp::Ordering;

use kuberic_runtime::protocol::public_operations::{
    DataLossAttempt, DataLossAuthorization, PublicDataLossRecoveryStage,
    PublicDataLossRecoveryStatus, PublicOperationPreviewIdentity, PublicProgressRole,
    PublicV1AcceptedStatus, PublicV1AgentReport,
};
use kuberic_runtime::protocol::types::{
    HistoryContext, PublicProtocolGeneration, ResourceUid, derive_bootstrap_history,
    derive_history_initialization_id,
};

pub fn ensure_bootstrap_history(
    accepted: Option<PublicV1AcceptedStatus>,
    resource_uid: &ResourceUid,
    identity: &PublicOperationPreviewIdentity,
) -> Result<PublicV1AcceptedStatus, String> {
    if !identity.is_public_v1() {
        return Err("public-v1 bootstrap history requires protocol 11".into());
    }
    if let Some(accepted) = accepted {
        if &accepted.identity != identity
            || &accepted.history.resource_uid != resource_uid
            || accepted.history.protocol_generation.value() != identity.generation
        {
            return Err("accepted public-v1 bootstrap history changed identity".into());
        }
        return Ok(accepted);
    }
    let generation = PublicProtocolGeneration::new(identity.generation);
    let history_initialization_id = derive_history_initialization_id(resource_uid, generation);
    let history = derive_bootstrap_history(resource_uid, generation, &history_initialization_id);
    Ok(PublicV1AcceptedStatus {
        identity: identity.clone(),
        history_initialization_id,
        history,
        data_loss_recovery: None,
        completed_operations: Vec::new(),
        extensions: Default::default(),
    })
}

pub fn compare_election_progress(
    left_history: &HistoryContext,
    left_progress: i64,
    right_history: &HistoryContext,
    right_progress: i64,
) -> Option<Ordering> {
    left_history.compare_progress(left_progress, right_history, right_progress)
}

pub fn invalidate_for_data_loss(
    accepted: &PublicV1AcceptedStatus,
    report: &PublicV1AgentReport,
    authorization: &DataLossAuthorization,
    attempt: &DataLossAttempt,
) -> Result<PublicV1AcceptedStatus, String> {
    report.validate().map_err(str::to_owned)?;
    authorization.validate().map_err(str::to_owned)?;
    attempt
        .validate_for(authorization, -1)
        .map_err(str::to_owned)?;
    if accepted.identity != report.identity
        || accepted.data_loss_recovery.is_some()
        || accepted.history != authorization.predecessor
        || report.history != authorization.predecessor
        || report.storage_incarnation != authorization.provisional_primary
        || attempt.authorization_id != authorization.authorization_id
        || attempt.process.process_session_id != report.process_session_id
        || attempt.process.data_loss_replay != authorization.replay_disposition
    {
        return Err("public-v1 data-loss invalidation changed accepted authority".into());
    }
    let mut invalidated = accepted.clone();
    invalidated.data_loss_recovery = Some(PublicDataLossRecoveryStatus {
        authorization_id: authorization.authorization_id.clone(),
        attempt: attempt.clone(),
        predecessor_history: authorization.predecessor.clone(),
        proposed_history: authorization.proposed.clone(),
        provisional_primary: authorization.provisional_primary.clone(),
        stage: PublicDataLossRecoveryStage::Invalidated,
        observed_progress: None,
    });
    invalidated.completed_operations.clear();
    Ok(invalidated)
}

pub fn is_provisional_primary(report: &PublicV1AgentReport) -> bool {
    report.validate().is_ok()
        && report.role == kuberic_runtime::protocol::types::ReplicaRole::Primary
        && report.progress.role == PublicProgressRole::Recovery
        && report.data_loss_recovery.as_ref().is_some_and(|recovery| {
            recovery.stage != PublicDataLossRecoveryStage::Admitted
                && report.read_status != kuberic_runtime::protocol::types::AccessStatus::Granted
                && report.write_status != kuberic_runtime::protocol::types::AccessStatus::Granted
        })
}
