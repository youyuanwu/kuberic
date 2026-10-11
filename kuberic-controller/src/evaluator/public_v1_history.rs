use std::cmp::Ordering;

use kuberic_runtime::protocol::command::{ProtocolCommand, PublicV1HistoryCommand};
use kuberic_runtime::protocol::public_operations::{
    DataLossAttempt, DataLossAuthorization, PublicDataLossRecoveryStage,
    PublicDataLossRecoveryStatus, PublicOperationPreviewIdentity, PublicProgressRole,
    PublicV1AcceptedStatus, PublicV1AgentReport,
};
use kuberic_runtime::protocol::types::{
    HistoryContext, PublicProtocolGeneration, ReplicaStorageIncarnationContext, ResourceUid,
    derive_bootstrap_history, derive_history_initialization_id,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PublicV1HistoryPlan {
    PersistStatus {
        expected: Option<PublicV1AcceptedStatus>,
        next: Box<PublicV1AcceptedStatus>,
    },
    Dispatch {
        expected: PublicV1AcceptedStatus,
        command: ProtocolCommand,
    },
    Stable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublicV1ElectionStatus {
    Ineligible,
    RecoveryPending,
    ProvisionalPrimary,
    Eligible,
}

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

pub fn compare_election_reports(
    left: &PublicV1AgentReport,
    right: &PublicV1AgentReport,
    admitted_storage: &[ReplicaStorageIncarnationContext],
) -> Option<Ordering> {
    if !matches!(
        election_status(left, admitted_storage),
        PublicV1ElectionStatus::Eligible | PublicV1ElectionStatus::ProvisionalPrimary
    ) || !matches!(
        election_status(right, admitted_storage),
        PublicV1ElectionStatus::Eligible | PublicV1ElectionStatus::ProvisionalPrimary
    ) {
        return None;
    }
    compare_election_progress(
        &left.history,
        left.progress.last,
        &right.history,
        right.progress.last,
    )
}

pub fn election_status(
    report: &PublicV1AgentReport,
    admitted_storage: &[ReplicaStorageIncarnationContext],
) -> PublicV1ElectionStatus {
    if report.validate().is_err() {
        return PublicV1ElectionStatus::Ineligible;
    }
    if report.data_loss_recovery.as_ref().is_some_and(|recovery| {
        matches!(
            recovery.stage,
            PublicDataLossRecoveryStage::Authorized
                | PublicDataLossRecoveryStage::Invalidated
                | PublicDataLossRecoveryStage::CallbackProcessing
                | PublicDataLossRecoveryStage::HistoryPending
                | PublicDataLossRecoveryStage::ConfigurationPending
        )
    }) {
        return if is_provisional_primary(report) {
            PublicV1ElectionStatus::ProvisionalPrimary
        } else {
            PublicV1ElectionStatus::RecoveryPending
        };
    }
    if admitted_storage.contains(&report.storage_incarnation) {
        PublicV1ElectionStatus::Eligible
    } else {
        PublicV1ElectionStatus::Ineligible
    }
}

pub fn plan_bootstrap_history(
    accepted: Option<PublicV1AcceptedStatus>,
    resource_uid: &ResourceUid,
    identity: &PublicOperationPreviewIdentity,
    storage: ReplicaStorageIncarnationContext,
) -> Result<PublicV1HistoryPlan, String> {
    let next = ensure_bootstrap_history(accepted.clone(), resource_uid, identity)?;
    if accepted.is_none() {
        return Ok(PublicV1HistoryPlan::PersistStatus {
            expected: None,
            next: Box::new(next),
        });
    }
    let command = PublicV1HistoryCommand::Initialize {
        preview: identity.clone(),
        accepted: next.clone(),
        storage,
    };
    command.validate().map_err(str::to_owned)?;
    Ok(PublicV1HistoryPlan::Dispatch {
        expected: next,
        command: ProtocolCommand::PublicV1History(Box::new(command)),
    })
}

pub fn plan_data_loss_recovery(
    accepted: &PublicV1AcceptedStatus,
    report: &PublicV1AgentReport,
    authorization: &DataLossAuthorization,
    attempt: &DataLossAttempt,
    intent: &kuberic_runtime::protocol::public_operations::PublicOperationIntent,
    predecessor_contained: bool,
) -> Result<PublicV1HistoryPlan, String> {
    intent.validate().map_err(str::to_owned)?;
    let lifecycle = intent
        .lifecycle
        .as_ref()
        .ok_or_else(|| "public-v1 recovery dispatch lacks lifecycle input".to_string())?;
    let input = lifecycle
        .data_loss_recovery
        .as_ref()
        .ok_or_else(|| "public-v1 recovery dispatch lacks typed lifecycle input".to_string())?;
    if input.authorization != *authorization || input.attempt != *attempt {
        return Err("public-v1 recovery intent differs from proposed invalidation".into());
    }
    let minimum_configuration_number = lifecycle
        .previous
        .as_ref()
        .map_or(-1, |configuration| configuration.epoch.configuration_number);
    if accepted.data_loss_recovery.is_none() {
        return Ok(PublicV1HistoryPlan::PersistStatus {
            expected: Some(accepted.clone()),
            next: Box::new(invalidate_for_data_loss(
                accepted,
                report,
                authorization,
                attempt,
                minimum_configuration_number,
            )?),
        });
    }
    let recovery = accepted
        .data_loss_recovery
        .as_ref()
        .expect("checked accepted recovery");
    if recovery.attempt != *attempt {
        report.validate().map_err(str::to_owned)?;
        if !predecessor_contained
            || recovery.stage != PublicDataLossRecoveryStage::CallbackProcessing
            || accepted.identity != report.identity
            || accepted.history != authorization.predecessor
            || report.history != authorization.predecessor
            || report.storage_incarnation != authorization.provisional_primary
            || report.process_session_id != attempt.process.process_session_id
            || report.read_status == kuberic_runtime::protocol::types::AccessStatus::Granted
            || report.write_status == kuberic_runtime::protocol::types::AccessStatus::Granted
            || recovery.logical_authorization() != *authorization
            || attempt
                .validate_for(
                    authorization,
                    recovery.attempt.process.epoch.configuration_number,
                )
                .is_err()
            || attempt.attempt_id == recovery.attempt.attempt_id
            || attempt.process.process_session_id == recovery.attempt.process.process_session_id
        {
            return Err("public-v1 recovery renewal lacks exact contained authority".into());
        }
        let mut next = accepted.clone();
        let mut renewed = recovery.clone();
        renewed.attempt = attempt.clone();
        renewed.stage = PublicDataLossRecoveryStage::Invalidated;
        renewed.observed_progress = None;
        next.data_loss_recovery = Some(renewed);
        return Ok(PublicV1HistoryPlan::PersistStatus {
            expected: Some(accepted.clone()),
            next: Box::new(next),
        });
    }
    if matches!(
        recovery.stage,
        PublicDataLossRecoveryStage::Failed
            | PublicDataLossRecoveryStage::ReplacementRequired
            | PublicDataLossRecoveryStage::Retired
    ) {
        return Ok(PublicV1HistoryPlan::Stable);
    }
    if recovery.stage != PublicDataLossRecoveryStage::Invalidated {
        return Ok(PublicV1HistoryPlan::Stable);
    }
    if recovery.logical_authorization() != *authorization
        || recovery.attempt != *attempt
        || input.authorization != *authorization
        || input.attempt != *attempt
    {
        return Err("public-v1 recovery dispatch differs from accepted invalidation".into());
    }
    let command = PublicV1HistoryCommand::Recover {
        accepted: accepted.clone(),
        intent: Box::new(intent.clone()),
    };
    command.validate().map_err(str::to_owned)?;
    Ok(PublicV1HistoryPlan::Dispatch {
        expected: accepted.clone(),
        command: ProtocolCommand::PublicV1History(Box::new(command)),
    })
}

pub fn invalidate_for_data_loss(
    accepted: &PublicV1AcceptedStatus,
    report: &PublicV1AgentReport,
    authorization: &DataLossAuthorization,
    attempt: &DataLossAttempt,
    minimum_configuration_number: i64,
) -> Result<PublicV1AcceptedStatus, String> {
    report.validate().map_err(str::to_owned)?;
    authorization.validate().map_err(str::to_owned)?;
    attempt
        .validate_for(authorization, minimum_configuration_number)
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
        reset_scope: authorization.reset_scope.clone(),
        replay_disposition: authorization.replay_disposition,
        stage: PublicDataLossRecoveryStage::Invalidated,
        observed_progress: None,
    });
    invalidated.completed_operations.clear();
    Ok(invalidated)
}

pub fn accept_recovery_report(
    accepted: &PublicV1AcceptedStatus,
    report: &PublicV1AgentReport,
) -> Result<PublicV1AcceptedStatus, String> {
    report.validate().map_err(str::to_owned)?;
    let expected = accepted
        .data_loss_recovery
        .as_ref()
        .ok_or_else(|| "accepted public-v1 status has no recovery to advance".to_string())?;
    let observed = report
        .data_loss_recovery
        .as_ref()
        .ok_or_else(|| "public-v1 recovery report omitted accepted recovery".to_string())?;
    if accepted.identity != report.identity
        || expected.logical_authorization() != observed.logical_authorization()
        || expected.attempt != observed.attempt
    {
        return Err("public-v1 recovery report changed accepted authority or attempt".into());
    }
    let rank = |stage: PublicDataLossRecoveryStage| match stage {
        PublicDataLossRecoveryStage::Authorized => 0,
        PublicDataLossRecoveryStage::Invalidated => 1,
        PublicDataLossRecoveryStage::CallbackProcessing => 2,
        PublicDataLossRecoveryStage::HistoryPending => 3,
        PublicDataLossRecoveryStage::ConfigurationPending => 4,
        PublicDataLossRecoveryStage::Admitted => 5,
        PublicDataLossRecoveryStage::Failed
        | PublicDataLossRecoveryStage::ReplacementRequired
        | PublicDataLossRecoveryStage::Retired => 6,
    };
    if rank(observed.stage) < rank(expected.stage)
        || matches!(
            expected.stage,
            PublicDataLossRecoveryStage::Failed
                | PublicDataLossRecoveryStage::ReplacementRequired
                | PublicDataLossRecoveryStage::Retired
        ) && observed.stage != expected.stage
    {
        return Err("public-v1 recovery report regressed or replaced terminal state".into());
    }
    let mut next = accepted.clone();
    next.history = report.history.clone();
    next.data_loss_recovery = Some(observed.clone());
    Ok(next)
}

pub fn plan_recovery_report(
    accepted: &PublicV1AcceptedStatus,
    report: &PublicV1AgentReport,
) -> Result<PublicV1HistoryPlan, String> {
    let next = accept_recovery_report(accepted, report)?;
    if &next == accepted {
        Ok(PublicV1HistoryPlan::Stable)
    } else {
        Ok(PublicV1HistoryPlan::PersistStatus {
            expected: Some(accepted.clone()),
            next: Box::new(next),
        })
    }
}

pub fn is_provisional_primary(report: &PublicV1AgentReport) -> bool {
    report.validate().is_ok()
        && report.role == kuberic_runtime::protocol::types::ReplicaRole::Primary
        && report.progress.role == PublicProgressRole::Recovery
        && report.data_loss_recovery.as_ref().is_some_and(|recovery| {
            matches!(
                recovery.stage,
                PublicDataLossRecoveryStage::Authorized
                    | PublicDataLossRecoveryStage::Invalidated
                    | PublicDataLossRecoveryStage::CallbackProcessing
                    | PublicDataLossRecoveryStage::HistoryPending
                    | PublicDataLossRecoveryStage::ConfigurationPending
            ) && report.read_status != kuberic_runtime::protocol::types::AccessStatus::Granted
                && report.write_status != kuberic_runtime::protocol::types::AccessStatus::Granted
        })
}
