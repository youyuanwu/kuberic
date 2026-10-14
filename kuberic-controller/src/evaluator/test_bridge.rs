use kuberic_runtime::protocol::public_operations::PublicOperationPreviewIdentity;

use super::EvaluationConfig;

pub use super::public_lifecycle::{
    PreviewAcceptedStatus, PreviewServiceLocationPlan, PreviewServiceLocationStage,
    PreviewTransition, ServiceLocationProjection, evaluate_service_location, plan_public_lifecycle,
};
pub use super::public_v1_evidence::{
    PublicV1AcceptedTransitionIntent, PublicV1AcceptedTransitionStatus, PublicV1EvidenceConsumer,
    PublicV1EvidencePlan, PublicV1ResourceAuthority, plan_public_v1_evidence,
};
pub use super::public_v1_history::{
    PublicV1ElectionStatus, PublicV1HistoryPlan,
    accept_recovery_report as accept_public_v1_recovery_report,
    compare_election_progress as compare_public_v1_election_progress,
    compare_election_reports as compare_public_v1_election_reports,
    election_status as public_v1_election_status,
    ensure_bootstrap_history as ensure_public_v1_bootstrap_history,
    invalidate_for_data_loss as invalidate_public_v1_for_data_loss,
    is_provisional_primary as is_public_v1_provisional_primary,
    plan_bootstrap_history as plan_public_v1_bootstrap_history,
    plan_data_loss_recovery as plan_public_v1_data_loss_recovery,
    plan_recovery_report as plan_public_v1_recovery_report,
};
pub use super::public_v1_session_renewal::{
    PublicV1SessionRenewalPlan, accept_access_closed as accept_public_v1_session_access_closed,
    accept_epoch_updated as accept_public_v1_session_epoch_updated,
    accept_fresh_configuration as accept_public_v1_fresh_configuration,
    accept_replacement_role as accept_public_v1_replacement_role,
    accept_session_renewal as accept_public_v1_session_renewal,
    accept_survivor_retired as accept_public_v1_survivor_retired,
    plan_session_renewal as plan_public_v1_session_renewal,
};
pub use crate::cluster_api::{
    HostedPreviewReplica, HostedPublicV1Adapter, PreviewServiceApi, PublicV1EvidenceApi,
    PublicV1EvidenceEffect, PublicV1EvidenceResource, PublicV1EvidenceState, PublicV1HistoryApi,
    PublicV1OperationApi, PublicV1ResourceDisposition, PublicV1ResourceProof,
    PublicV1SessionRenewalApi, preview_service_matches, preview_service_update,
};
pub use crate::executor::{
    execute_preview_service_location, execute_public_v1_evidence, execute_public_v1_history,
    execute_public_v1_operation, execute_public_v1_session_renewal,
};
pub use crate::normalize::normalize_public_v1_service_location;
pub use crate::public_v1_reconciliation::{
    PublicV1ReconciliationOutcome, reconcile_public_v1_consumer, reconcile_public_v1_operation,
};
pub use k8s_openapi::api::core::v1::Service as PreviewWriteService;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicOperationPreviewEvaluationConfig {
    pub identity: PublicOperationPreviewIdentity,
    pub evaluation: EvaluationConfig,
}

impl PublicOperationPreviewEvaluationConfig {
    pub fn new(identity: PublicOperationPreviewIdentity, evaluation: EvaluationConfig) -> Self {
        assert!(
            identity.is_valid(),
            "public-operation preview identity must be valid"
        );
        Self {
            identity,
            evaluation,
        }
    }

    pub fn validate_identity(
        &self,
        observed: &PublicOperationPreviewIdentity,
    ) -> Result<(), String> {
        if observed != &self.identity {
            return Err("public-operation preview identity mismatch".into());
        }
        Ok(())
    }
}
