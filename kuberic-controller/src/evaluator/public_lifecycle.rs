//! Selected repository preview only; no production Plan or AcceptedStatus variant.

use k8s_openapi::api::core::v1::Service;
use kuberic_runtime::protocol::public_operations::{
    PossibleDataLossIntent, PublicLifecycleInput, PublicLifecycleReport, PublicOperationClass,
    PublicOperationIntent, ServiceLocation, service_location_address_digest,
};
use kuberic_runtime::protocol::types::{
    OperationId, ProcessSessionId, ReplicaIdentity, ResourceUid,
};
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
        program: None,
    };
    intent.validate()?;
    Ok(intent)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceLocationProjection {
    pub authority: PublicOperationIntent,
    pub resource_uid: ResourceUid,
    pub primary: ReplicaIdentity,
    pub process_session_id: ProcessSessionId,
    pub revision: u64,
    pub revoked: bool,
    pub clear_required: bool,
    pub address_digest: String,
    pub deferred_address_digest: Option<String>,
    pub service_uid: String,
    pub service_resource_version: String,
    pub location: Option<ServiceLocation>,
    pub deferred_location: Option<ServiceLocation>,
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
    resource_uid: &ResourceUid,
    authority: &PublicOperationIntent,
    report: Option<&PublicLifecycleReport>,
    status: &PreviewAcceptedStatus,
    service: &Service,
) -> Result<PreviewServiceLocationPlan, String> {
    config.validate_identity(&authority.preview)?;
    authority.validate()?;
    if let Some(report) = report
        && &report.resource_uid != resource_uid
    {
        return Err("service-location report resource UID mismatch".into());
    }
    let service_uid = service
        .metadata
        .uid
        .clone()
        .filter(|uid| !uid.is_empty())
        .ok_or("Service UID required")?;
    let service_resource_version = service
        .metadata
        .resource_version
        .clone()
        .filter(|version| !version.is_empty())
        .ok_or("Service resourceVersion required")?;
    let existing = match &status.service_location_projection {
        PreviewServiceLocationStage::None => None,
        PreviewServiceLocationStage::Pending(projection)
        | PreviewServiceLocationStage::Published(projection) => Some(projection),
    };
    if let Some(existing) = existing {
        validate_projection(config, existing)?;
        if existing.authority.revision > authority.revision
            || (existing.authority.revision == authority.revision
                && existing.authority != *authority)
        {
            return Err("stale service-location authority".into());
        }
    }
    let location = crate::normalize::normalize_public_service_location(config, authority, report)?;
    let primary = authority
        .lifecycle
        .as_ref()
        .map(|input| input.replica.clone())
        .or_else(|| existing.map(|projection| projection.primary.clone()))
        .or_else(|| report.map(|report| report.replica.clone()))
        .ok_or("service-location projection requires primary identity")?;
    let revoked = projection_revoked(existing, authority, report, location.as_ref());
    let (location, deferred_location, revoked, clear_required) =
        projection_locations(existing, status, service, authority, location, revoked)?;
    let desired = ServiceLocationProjection {
        authority: authority.clone(),
        resource_uid: resource_uid.clone(),
        primary,
        process_session_id: authority.process_session_id.clone(),
        revision: authority.revision,
        revoked,
        clear_required,
        address_digest: service_location_address_digest(location.as_ref()),
        deferred_address_digest: deferred_location
            .as_ref()
            .map(|location| service_location_address_digest(Some(location))),
        service_uid: service_uid.clone(),
        service_resource_version: service_resource_version.clone(),
        location,
        deferred_location,
    };
    if existing.is_none_or(|existing| !same_projection(existing, &desired)) {
        return Ok(PreviewServiceLocationPlan::PersistStatus {
            expected: status.clone(),
            next: PreviewAcceptedStatus {
                service_location_projection: PreviewServiceLocationStage::Pending(desired),
            },
        });
    }
    let existing = existing.expect("projection exists after semantic match");
    if matches!(
        status.service_location_projection,
        PreviewServiceLocationStage::Published(_)
    ) {
        if !crate::cluster_api::preview_service_matches(service, &desired)
            || existing.service_uid != service_uid
            || existing.service_resource_version != service_resource_version
        {
            return Ok(PreviewServiceLocationPlan::PersistStatus {
                expected: status.clone(),
                next: PreviewAcceptedStatus {
                    service_location_projection: PreviewServiceLocationStage::Pending(desired),
                },
            });
        }
        return Ok(PreviewServiceLocationPlan::Stable);
    }
    if !crate::cluster_api::preview_service_matches(service, &desired) {
        if existing.service_uid != service_uid
            || existing.service_resource_version != service_resource_version
        {
            return Ok(PreviewServiceLocationPlan::PersistStatus {
                expected: status.clone(),
                next: PreviewAcceptedStatus {
                    service_location_projection: PreviewServiceLocationStage::Pending(desired),
                },
            });
        }
        return Ok(PreviewServiceLocationPlan::WriteService {
            expected_status: status.clone(),
            expected_uid: existing.service_uid.clone(),
            expected_version: existing.service_resource_version.clone(),
            projection: existing.clone(),
        });
    }
    Ok(PreviewServiceLocationPlan::PersistStatus {
        expected: status.clone(),
        next: PreviewAcceptedStatus {
            service_location_projection: PreviewServiceLocationStage::Published(desired),
        },
    })
}

fn validate_projection(
    config: &PublicOperationPreviewEvaluationConfig,
    projection: &ServiceLocationProjection,
) -> Result<(), String> {
    config.validate_identity(&projection.authority.preview)?;
    projection.authority.validate()?;
    if projection
        .authority
        .lifecycle
        .as_ref()
        .is_some_and(|input| projection.primary != input.replica)
        || (projection.authority.lifecycle.is_none() && !projection.authority.class.is_terminal())
        || projection.process_session_id != projection.authority.process_session_id
        || projection.revision != projection.authority.revision
        || (projection.revoked
            && (projection.location.is_some() || projection.deferred_location.is_some()))
        || (!projection.revoked && projection.authority.class.is_terminal())
        || projection.address_digest
            != service_location_address_digest(projection.location.as_ref())
        || projection.deferred_address_digest
            != projection
                .deferred_location
                .as_ref()
                .map(|location| service_location_address_digest(Some(location)))
        || projection.service_uid.is_empty()
        || projection.service_resource_version.is_empty()
        || projection
            .location
            .iter()
            .chain(projection.deferred_location.iter())
            .any(|location| !location_matches_projection(location, projection))
        || (projection.authority.class.is_terminal()
            && (projection.location.is_some() || projection.deferred_location.is_some()))
    {
        return Err("invalid service-location projection fence".into());
    }
    Ok(())
}

fn same_projection(
    existing: &ServiceLocationProjection,
    desired: &ServiceLocationProjection,
) -> bool {
    existing.authority == desired.authority
        && existing.resource_uid == desired.resource_uid
        && existing.primary == desired.primary
        && existing.process_session_id == desired.process_session_id
        && existing.revision == desired.revision
        && existing.revoked == desired.revoked
        && existing.clear_required == desired.clear_required
        && existing.address_digest == desired.address_digest
        && existing.deferred_address_digest == desired.deferred_address_digest
        && existing.service_uid == desired.service_uid
        && existing.location == desired.location
        && existing.deferred_location == desired.deferred_location
}

fn projection_locations(
    existing: Option<&ServiceLocationProjection>,
    status: &PreviewAcceptedStatus,
    service: &Service,
    authority: &PublicOperationIntent,
    location: Option<ServiceLocation>,
    revoked: bool,
) -> Result<(Option<ServiceLocation>, Option<ServiceLocation>, bool, bool), String> {
    let Some(existing) = existing else {
        return Ok((location, None, revoked, false));
    };
    if existing.authority == *authority && existing.location.is_none() {
        return match (&existing.deferred_location, location) {
            (None, Some(_)) if existing.revoked => {
                Err("cleared service-location authority cannot be republished".into())
            }
            (None, Some(location)) if existing.clear_required => {
                if projection_is_published(status, service, existing) {
                    Ok((Some(location), None, false, false))
                } else {
                    Ok((None, Some(location), false, true))
                }
            }
            (None, Some(location)) => Ok((Some(location), None, false, false)),
            (Some(deferred), Some(location)) if deferred == &location => {
                let clear_published = projection_is_published(status, service, existing);
                if clear_published {
                    Ok((Some(location), None, false, false))
                } else {
                    Ok((None, Some(location), false, true))
                }
            }
            (Some(_), Some(_)) => {
                Err("service-location address changed within one authority".into())
            }
            (_, None) => Ok((None, None, revoked, existing.clear_required)),
        };
    }
    if existing.authority == *authority
        && existing.location.is_some()
        && location.is_some()
        && existing.location != location
    {
        return Err("service-location address changed within one authority".into());
    }
    if existing.authority != *authority
        && (existing.location.is_some()
            || existing.clear_required
            || !projection_is_published(status, service, existing))
    {
        return Ok((None, location, revoked, true));
    }
    if existing.authority == *authority && existing.location.is_some() && location.is_none() {
        return Ok((None, None, revoked, true));
    }
    Ok((location, None, revoked, false))
}

fn projection_revoked(
    existing: Option<&ServiceLocationProjection>,
    authority: &PublicOperationIntent,
    report: Option<&PublicLifecycleReport>,
    location: Option<&ServiceLocation>,
) -> bool {
    authority.class.is_terminal()
        || existing.is_some_and(|projection| {
            projection.authority == *authority
                && (projection.revoked || (projection.location.is_some() && location.is_none()))
        })
        || report.is_some_and(|report| {
            report.preview != authority.preview
                || report.process_session_id != authority.process_session_id
                || report.revision != authority.revision
                || report.operation_id.as_ref() != Some(&authority.operation_id)
                || (report.service_location.is_some() && location.is_none())
        })
}

fn projection_is_published(
    status: &PreviewAcceptedStatus,
    service: &Service,
    projection: &ServiceLocationProjection,
) -> bool {
    matches!(
        status.service_location_projection,
        PreviewServiceLocationStage::Published(_)
    ) && crate::cluster_api::preview_service_matches(service, projection)
        && service.metadata.uid.as_ref() == Some(&projection.service_uid)
        && service.metadata.resource_version.as_ref() == Some(&projection.service_resource_version)
}

fn location_matches_projection(
    location: &ServiceLocation,
    projection: &ServiceLocationProjection,
) -> bool {
    location.preview == projection.authority.preview
        && location.resource_uid == projection.resource_uid
        && location.operation_id == projection.authority.operation_id
        && location.replica == projection.primary
        && location.process_session_id == projection.process_session_id
        && location.revision == projection.revision
        && projection
            .authority
            .lifecycle
            .as_ref()
            .is_some_and(|input| location.epoch == input.epoch)
}
