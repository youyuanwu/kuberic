use crate::cluster_api::PublicV1EvidenceApi;
use crate::evaluator::public_v1_evidence::{
    PublicV1EvidencePlan, plan_persisted_public_v1_evidence, plan_public_v1_evidence,
};
use kuberic_runtime::protocol::public_operations::PublicV1AgentReport;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublicV1ReconciliationOutcome {
    Waiting,
    EvidencePersisted,
    EffectApplied,
}

fn outcome(plan: &PublicV1EvidencePlan) -> PublicV1ReconciliationOutcome {
    match plan {
        PublicV1EvidencePlan::Wait => PublicV1ReconciliationOutcome::Waiting,
        PublicV1EvidencePlan::PersistStatus { .. } => {
            PublicV1ReconciliationOutcome::EvidencePersisted
        }
        PublicV1EvidencePlan::Accept { .. }
        | PublicV1EvidencePlan::RetireExactIncarnation { .. }
        | PublicV1EvidencePlan::ProvisionReplacement { .. } => {
            PublicV1ReconciliationOutcome::EffectApplied
        }
    }
}

pub async fn reconcile_public_v1_consumer<A>(
    api: &A,
    scope: &crate::cluster_api::PublicV1Scope,
    report: &PublicV1AgentReport,
) -> Result<PublicV1ReconciliationOutcome, String>
where
    A: PublicV1EvidenceApi + ?Sized,
{
    let accepted = api
        .observe_public_v1_evidence_status(scope)
        .await?
        .ok_or_else(|| "public-v1 consumer lacks accepted transition status".to_string())?;
    let plan = plan_public_v1_evidence(&accepted, report)?;
    let outcome = outcome(&plan);
    crate::executor::execute_public_v1_evidence(api, scope, plan).await?;
    Ok(outcome)
}

pub async fn reconcile_public_v1_operation<A>(
    api: &A,
    scope: &crate::cluster_api::PublicV1Scope,
    command: kuberic_runtime::protocol::command::ProtocolCommand,
) -> Result<PublicV1ReconciliationOutcome, String>
where
    A: crate::cluster_api::PublicV1OperationApi + PublicV1EvidenceApi + ?Sized,
{
    let expected = api
        .observe_public_v1_operation(scope)
        .await?
        .ok_or_else(|| "public-v1 operation lacks accepted transition status".to_string())?;
    if let Some(plan) = plan_persisted_public_v1_evidence(&expected)? {
        let outcome = outcome(&plan);
        crate::executor::execute_public_v1_evidence(api, scope, plan).await?;
        return Ok(outcome);
    }
    if let Ok(report) = api
        .observe_public_v1_operation_report(scope, &expected)
        .await
    {
        let exact_completion = report
            .completed_operations
            .iter()
            .any(|completion| expected.intent.matches(completion));
        if report.process_session_id != expected.intent.process_session_id && !exact_completion {
            api.supersede_public_v1_operation(scope, &expected, &command)
                .await?;
            return Ok(PublicV1ReconciliationOutcome::EffectApplied);
        }
        let plan = plan_public_v1_evidence(&expected, &report)?;
        if !matches!(plan, PublicV1EvidencePlan::Wait) {
            return reconcile_public_v1_consumer(api, scope, &report).await;
        }
    }
    if let Err(dispatch_error) =
        crate::executor::execute_public_v1_operation(api, scope, &expected, command.clone()).await
    {
        if let Ok(report) = api
            .observe_public_v1_operation_report(scope, &expected)
            .await
        {
            let outcome = reconcile_public_v1_consumer(api, scope, &report).await?;
            if outcome != PublicV1ReconciliationOutcome::Waiting {
                return Ok(outcome);
            }
        }
        api.contain_public_v1_operation(scope, &expected, &command)
            .await?;
        return Err(dispatch_error);
    }
    let report = api
        .observe_public_v1_operation_report(scope, &expected)
        .await?;
    reconcile_public_v1_consumer(api, scope, &report).await
}
