#[cfg(feature = "runtime-test-bridge")]
pub async fn execute_preview_service_location(
    api: &dyn crate::cluster_api::PreviewServiceApi,
    plan: crate::evaluator::test_bridge::PreviewServiceLocationPlan,
) -> std::result::Result<(), String> {
    use crate::evaluator::test_bridge::PreviewServiceLocationPlan;
    match plan {
        PreviewServiceLocationPlan::Stable => Ok(()),
        PreviewServiceLocationPlan::PersistStatus { expected, next } => {
            if let crate::evaluator::test_bridge::PreviewServiceLocationStage::Published(
                projection,
            ) = &next.service_location_projection
            {
                let (status, service) = api.observe_preview().await?;
                if status != expected
                    || service.metadata.uid.as_ref() != Some(&projection.service_uid)
                    || service.metadata.resource_version.as_ref()
                        != Some(&projection.service_resource_version)
                    || !crate::cluster_api::preview_service_matches(&service, projection)
                {
                    return Err("stale service-location publication".into());
                }
            }
            api.persist_preview_status(&expected, &next).await
        }
        PreviewServiceLocationPlan::WriteService {
            expected_status,
            expected_uid,
            expected_version,
            projection,
        } => {
            let (status, service) = api.observe_preview().await?;
            if status != expected_status
                || service.metadata.uid.as_ref() != Some(&expected_uid)
                || service.metadata.resource_version.as_ref() != Some(&expected_version)
            {
                return Err("stale service-location write".into());
            }
            if expected_status.service_location_projection
                != crate::evaluator::test_bridge::PreviewServiceLocationStage::Pending(
                    projection.clone(),
                )
            {
                return Err(
                    "service-location mutation is not the accepted pending projection".into(),
                );
            }
            api.replace_preview_service(crate::cluster_api::preview_service_update(
                &service,
                &projection,
            ))
            .await
        }
    }
}

pub async fn execute_public_v1_history<A>(
    api: &A,
    scope: &crate::cluster_api::PublicV1Scope,
    plan: crate::evaluator::public_v1_history::PublicV1HistoryPlan,
) -> std::result::Result<(), String>
where
    A: crate::cluster_api::PublicV1HistoryApi + ?Sized,
{
    use crate::evaluator::public_v1_history::PublicV1HistoryPlan;
    match plan {
        PublicV1HistoryPlan::Stable => Ok(()),
        PublicV1HistoryPlan::PersistStatus { expected, next } => {
            if api.observe_public_v1_status(scope).await? != expected {
                return Err("stale public-v1 accepted status".into());
            }
            api.persist_public_v1_status(scope, expected.as_ref(), &next)
                .await
        }
        PublicV1HistoryPlan::Dispatch { expected, command } => {
            if api.observe_public_v1_status(scope).await?.as_ref() != Some(&expected) {
                return Err("stale public-v1 dispatch authority".into());
            }
            let kuberic_runtime::protocol::command::ProtocolCommand::PublicV1History(
                public_command,
            ) = &command
            else {
                return Err("public-v1 history executor received another command".into());
            };
            if public_command.accepted() != Some(&expected) {
                return Err("public-v1 command differs from accepted status".into());
            }
            api.dispatch_public_v1_history(scope, &expected, &command)
                .await
        }
    }
}

pub async fn execute_public_v1_session_renewal<A>(
    api: &A,
    scope: &crate::cluster_api::PublicV1Scope,
    plan: crate::evaluator::public_v1_session_renewal::PublicV1SessionRenewalPlan,
) -> std::result::Result<(), String>
where
    A: crate::cluster_api::PublicV1SessionRenewalApi + ?Sized,
{
    use crate::evaluator::public_v1_session_renewal::PublicV1SessionRenewalPlan;
    match plan {
        PublicV1SessionRenewalPlan::Stable | PublicV1SessionRenewalPlan::WaitForSurvivor { .. } => {
            Ok(())
        }
        PublicV1SessionRenewalPlan::PersistStatus { expected, next } => {
            if api.observe_public_v1_session_renewal(scope).await?.as_ref() != expected.as_deref() {
                return Err("stale public-v1 session-renewal status".into());
            }
            api.persist_public_v1_session_renewal(scope, expected.as_deref(), &next)
                .await
        }
        PublicV1SessionRenewalPlan::Dispatch { expected, command } => {
            if api.observe_public_v1_session_renewal(scope).await?.as_ref()
                != Some(expected.as_ref())
            {
                return Err("stale public-v1 session-renewal dispatch authority".into());
            }
            let kuberic_runtime::protocol::command::ProtocolCommand::PublicV1History(
                public_command,
            ) = &command
            else {
                return Err("public-v1 session-renewal executor received another command".into());
            };
            let kuberic_runtime::protocol::command::PublicV1HistoryCommand::SessionRenewal(
                renewal_command,
            ) = public_command.as_ref()
            else {
                return Err("public-v1 session-renewal executor received a history command".into());
            };
            renewal_command.validate().map_err(str::to_owned)?;
            if renewal_command.accepted() != expected.as_ref() {
                return Err(
                    "public-v1 session-renewal command differs from accepted status".into(),
                );
            }
            api.dispatch_public_v1_session_renewal(scope, &expected, &command)
                .await
        }
    }
}

pub async fn execute_public_v1_operation<A>(
    api: &A,
    scope: &crate::cluster_api::PublicV1Scope,
    expected: &crate::evaluator::public_v1_evidence::PublicV1AcceptedTransitionStatus,
    command: kuberic_runtime::protocol::command::ProtocolCommand,
) -> std::result::Result<(), String>
where
    A: crate::cluster_api::PublicV1OperationApi + ?Sized,
{
    if api.observe_public_v1_operation(scope).await?.as_ref() != Some(expected) {
        return Err("stale public-v1 operation authority".into());
    }
    let kuberic_runtime::protocol::command::ProtocolCommand::PublicV1Operation(operation) =
        &command
    else {
        return Err("public-v1 operation executor received another command".into());
    };
    operation.validate().map_err(str::to_owned)?;
    if !crate::cluster_api::public_v1_operations::operation_matches_transition(
        expected,
        &operation.intent,
    ) {
        return Err("public-v1 operation differs from accepted transition".into());
    }
    if operation.target != expected.intent.source.storage_incarnation.replica {
        return Err("public-v1 command target differs from accepted source incarnation".into());
    }
    api.dispatch_public_v1_operation(scope, expected, &command)
        .await
}

pub async fn execute_public_v1_evidence<A>(
    api: &A,
    scope: &crate::cluster_api::PublicV1Scope,
    plan: crate::evaluator::public_v1_evidence::PublicV1EvidencePlan,
) -> std::result::Result<(), String>
where
    A: crate::cluster_api::PublicV1EvidenceApi + ?Sized,
{
    use crate::evaluator::public_v1_evidence::PublicV1EvidencePlan;
    match plan {
        PublicV1EvidencePlan::PersistStatus { expected, next } => {
            let observed = api.observe_public_v1_evidence_status(scope).await?;
            if observed.as_ref() != Some(expected.as_ref())
                && observed.as_ref().is_none_or(|observed| {
                    observed.evidence != *next || observed.intent != expected.intent
                })
            {
                return Err("stale public-v1 evidence status".into());
            }
            if observed.as_ref().is_some_and(|observed| {
                observed.evidence == *next && observed.intent == expected.intent
            }) {
                return Ok(());
            }
            if !next.is_evidence_successor_of(&expected.evidence) {
                return Err("public-v1 evidence status transition is not monotonic".into());
            }
            api.persist_public_v1_evidence_status(scope, &expected, &next)
                .await
        }
        PublicV1EvidencePlan::Accept {
            expected,
            completion,
        } => {
            if api.observe_public_v1_evidence_status(scope).await?.as_ref()
                != Some(expected.as_ref())
            {
                return Err("stale public-v1 completion acceptance".into());
            }
            api.accept_public_v1_completion(scope, &expected, &completion)
                .await
        }
        PublicV1EvidencePlan::RetireExactIncarnation {
            expected,
            completion,
            target,
        } => {
            if api.observe_public_v1_evidence_status(scope).await?.as_ref()
                != Some(expected.as_ref())
            {
                return Err("stale public-v1 retirement authority".into());
            }
            api.retire_public_v1_incarnation(scope, &expected, &completion, &target)
                .await
        }
        PublicV1EvidencePlan::ProvisionReplacement {
            expected,
            completion,
            target,
            rebuild,
        } => {
            if api.observe_public_v1_evidence_status(scope).await?.as_ref()
                != Some(expected.as_ref())
            {
                return Err("stale public-v1 replacement authority".into());
            }
            api.provision_public_v1_replacement(scope, &expected, &completion, &target, &rebuild)
                .await
        }
        PublicV1EvidencePlan::Wait => Ok(()),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionKind {
    Stable,
    Applied,
    Executed,
}
