use crate::cluster_api::PublicV1EvidenceApi;
use crate::evaluator::public_v1_evidence::{PublicV1EvidencePlan, plan_public_v1_evidence};
use kuberic_runtime::protocol::public_operations::PublicV1AgentReport;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublicV1ReconciliationOutcome {
    Waiting,
    EvidencePersisted,
    EffectApplied,
}

pub async fn reconcile_public_v1_consumer(
    api: &dyn PublicV1EvidenceApi,
    report: &PublicV1AgentReport,
) -> Result<PublicV1ReconciliationOutcome, String> {
    let accepted = api
        .observe_public_v1_evidence_status()
        .await?
        .ok_or_else(|| "public-v1 consumer lacks accepted transition status".to_string())?;
    let plan = plan_public_v1_evidence(&accepted, report)?;
    let outcome = match &plan {
        PublicV1EvidencePlan::Wait => PublicV1ReconciliationOutcome::Waiting,
        PublicV1EvidencePlan::PersistStatus { .. } => {
            PublicV1ReconciliationOutcome::EvidencePersisted
        }
        PublicV1EvidencePlan::Accept { .. }
        | PublicV1EvidencePlan::RetireExactIncarnation { .. }
        | PublicV1EvidencePlan::ProvisionReplacement { .. } => {
            PublicV1ReconciliationOutcome::EffectApplied
        }
    };
    crate::executor::execute_public_v1_evidence(api, plan).await?;
    Ok(outcome)
}
