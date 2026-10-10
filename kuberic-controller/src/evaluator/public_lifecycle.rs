//! Selected repository preview only; no production Plan or AcceptedStatus variant.

use k8s_openapi::api::core::v1::Service;
use kuberic_runtime::protocol::public_operations::{
    PossibleDataLossIntent, PublicLifecycleInput, PublicLifecycleReport, PublicOperationClass,
    PublicOperationIntent, ServiceLocation,
};
use kuberic_runtime::protocol::types::{OperationId, ProcessSessionId};
use serde::{Deserialize, Serialize};

use super::test_bridge::PublicOperationPreviewEvaluationConfig;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreviewTransition {
    Ordinary,
    PossibleDataLoss,
}

pub fn plan_public_lifecycle(
    config: &PublicOperationPreviewEvaluationConfig,
    operation_id: OperationId,
    revision: u64,
    source_session: ProcessSessionId,
    mut input: PublicLifecycleInput,
    transition: PreviewTransition,
) -> Result<PublicOperationIntent, String> {
    input.possible_data_loss = match transition {
        PreviewTransition::Ordinary => PossibleDataLossIntent::NotPossible,
        PreviewTransition::PossibleDataLoss => PossibleDataLossIntent::Possible,
    };
    let intent = PublicOperationIntent {
        preview: config.identity.clone(),
        operation_id,
        revision,
        process_session_id: source_session,
        class: PublicOperationClass::Authority,
        input_digest: input.digest(),
        lifecycle: Some(input),
    };
    intent.validate()?;
    Ok(intent)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceLocationProjection {
    pub authority: PublicOperationIntent,
    pub location: Option<ServiceLocation>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum PreviewServiceLocationStage {
    #[default]
    None,
    Pending(ServiceLocationProjection),
    Published(ServiceLocationProjection),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PreviewAcceptedStatus {
    pub service_location_projection: PreviewServiceLocationStage,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreviewServiceLocationPlan {
    PersistStatus {
        expected: PreviewAcceptedStatus,
        next: PreviewAcceptedStatus,
        service_evidence: Option<(String, String)>,
    },
    WriteService {
        expected_status: PreviewAcceptedStatus,
        expected_uid: String,
        expected_version: String,
        projection: ServiceLocationProjection,
    },
    Stable,
}

pub fn evaluate_service_location(
    config: &PublicOperationPreviewEvaluationConfig,
    authority: &PublicOperationIntent,
    report: Option<&PublicLifecycleReport>,
    status: &PreviewAcceptedStatus,
    service: &Service,
) -> Result<PreviewServiceLocationPlan, String> {
    config.validate_identity(&authority.preview)?;
    authority.validate()?;
    let existing = match &status.service_location_projection {
        PreviewServiceLocationStage::None => None,
        PreviewServiceLocationStage::Pending(projection)
        | PreviewServiceLocationStage::Published(projection) => Some(projection),
    };
    if let Some(existing) = existing {
        config.validate_identity(&existing.authority.preview)?;
        if existing.authority.revision > authority.revision
            || (existing.authority.revision == authority.revision
                && existing.authority != *authority)
        {
            return Err("stale service-location authority".into());
        }
    }
    let location = crate::normalize::normalize_public_service_location(config, authority, report)?;
    let desired = ServiceLocationProjection {
        authority: authority.clone(),
        location,
    };
    if existing != Some(&desired) {
        return Ok(PreviewServiceLocationPlan::PersistStatus {
            expected: status.clone(),
            next: PreviewAcceptedStatus {
                service_location_projection: PreviewServiceLocationStage::Pending(desired),
            },
            service_evidence: None,
        });
    }
    if !crate::cluster_api::preview_service_matches(service, &desired) {
        // Published status is withdrawn before repairing any routing drift.
        if matches!(
            status.service_location_projection,
            PreviewServiceLocationStage::Published(_)
        ) {
            return Ok(PreviewServiceLocationPlan::PersistStatus {
                expected: status.clone(),
                next: PreviewAcceptedStatus {
                    service_location_projection: PreviewServiceLocationStage::Pending(desired),
                },
                service_evidence: None,
            });
        }
        return Ok(PreviewServiceLocationPlan::WriteService {
            expected_status: status.clone(),
            expected_uid: service
                .metadata
                .uid
                .clone()
                .filter(|uid| !uid.is_empty())
                .ok_or("Service UID required")?,
            expected_version: service
                .metadata
                .resource_version
                .clone()
                .filter(|rv| !rv.is_empty())
                .ok_or("Service resourceVersion required")?,
            projection: desired,
        });
    }
    if matches!(
        status.service_location_projection,
        PreviewServiceLocationStage::Pending(_)
    ) {
        return Ok(PreviewServiceLocationPlan::PersistStatus {
            expected: status.clone(),
            next: PreviewAcceptedStatus {
                service_location_projection: PreviewServiceLocationStage::Published(desired),
            },
            service_evidence: Some((
                service
                    .metadata
                    .uid
                    .clone()
                    .filter(|uid| !uid.is_empty())
                    .ok_or("Service UID required")?,
                service
                    .metadata
                    .resource_version
                    .clone()
                    .filter(|rv| !rv.is_empty())
                    .ok_or("Service resourceVersion required")?,
            )),
        });
    }
    Ok(PreviewServiceLocationPlan::Stable)
}
