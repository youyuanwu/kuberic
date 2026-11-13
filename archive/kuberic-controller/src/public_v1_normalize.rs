#![cfg(feature = "runtime-test-bridge")]

pub(crate) fn normalize_public_v1_lifecycle_service_location(
    config: &crate::evaluator::testing::PublicV1EvaluationConfig,
    authority: &kuberic_runtime::protocol::public_operations::PublicOperationIntent,
    report: Option<&kuberic_runtime::protocol::public_operations::PublicLifecycleReport>,
) -> std::result::Result<
    Option<kuberic_runtime::protocol::public_operations::ServiceLocation>,
    String,
> {
    let Some(report) = report else {
        return Ok(None);
    };
    config.validate_identity(&report.identity)?;
    let Some(input) = &authority.lifecycle else {
        return Ok(None);
    };
    if authority.class.is_terminal()
        || report.replica != input.replica
        || report.process_session_id != authority.process_session_id
        || report.revision != authority.revision
        || report.operation_id.as_ref() != Some(&authority.operation_id)
        || !report.write_access
        || report.role != kuberic_runtime::protocol::types::ReplicaRole::Primary
        || input.possible_data_loss
            != kuberic_runtime::protocol::public_operations::PossibleDataLossIntent::NotPossible
    {
        return Ok(None);
    }
    let Some(location) = &report.service_location else {
        return Ok(None);
    };
    if location.identity != authority.identity
        || location.resource_uid != report.resource_uid
        || location.replica != input.replica
        || location.process_session_id != authority.process_session_id
        || location.revision != authority.revision
        || location.epoch != input.epoch
        || location.operation_id != authority.operation_id
    {
        return Ok(None);
    }
    Ok(Some(location.clone()))
}

pub fn normalize_public_v1_service_location(
    config: &crate::evaluator::testing::PublicV1EvaluationConfig,
    authority: &kuberic_runtime::protocol::public_operations::PublicOperationIntent,
    report: Option<&kuberic_runtime::protocol::public_operations::PublicV1AgentReport>,
) -> std::result::Result<
    Option<kuberic_runtime::protocol::public_operations::ServiceLocation>,
    String,
> {
    use kuberic_runtime::protocol::public_operations::{
        PublicOperationProgram, PublicV1OperationResult,
    };
    use kuberic_runtime::protocol::types::{AccessStatus, ReplicaRole};

    let Some(report) = report else {
        return Ok(None);
    };
    report.validate().map_err(str::to_owned)?;
    config.validate_identity(&report.identity)?;
    if authority.identity != report.identity
        || authority.process_session_id != report.process_session_id
        || report.role != ReplicaRole::Primary
        || report.write_status != AccessStatus::Granted
    {
        return Ok(None);
    }
    let Some(location) = &report.service_location else {
        return Ok(None);
    };
    let expected_epoch = authority
        .lifecycle
        .as_ref()
        .map(|lifecycle| lifecycle.epoch)
        .or(match authority.program.as_ref() {
            Some(PublicOperationProgram::Role { epoch, .. })
            | Some(PublicOperationProgram::Swap { epoch, .. }) => Some(*epoch),
            _ => None,
        });
    let exact_completion = report.completed_operations.iter().any(|completion| {
        completion.operation_id == authority.operation_id
            && completion.revision == authority.revision
            && completion.process_session_id == authority.process_session_id
            && completion.input_digest == authority.input_digest
            && completion.replay_disposition == authority.replay_disposition
            && completion.result == PublicV1OperationResult::Succeeded
    });
    if !exact_completion
        || location.operation_id != authority.operation_id
        || location.revision != authority.revision
        || location.process_session_id != authority.process_session_id
        || Some(location.epoch) != expected_epoch
    {
        return Ok(None);
    }
    Ok(Some(location.clone()))
}
