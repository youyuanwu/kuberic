use k8s_openapi::api::core::v1::Service;

use super::PUBLIC_V1_SERVICE_LOCATION_ANNOTATION;
use crate::evaluator::testing::{PublicV1LifecycleStatus, ServiceLocationProjection};

#[async_trait::async_trait]
pub trait PublicV1ServiceApi: Send + Sync {
    async fn observe_public_v1_service(&self)
    -> Result<(PublicV1LifecycleStatus, Service), String>;
    async fn persist_public_v1_lifecycle_status(
        &self,
        expected: &PublicV1LifecycleStatus,
        next: &PublicV1LifecycleStatus,
    ) -> Result<(), String>;
    /// One conditional Service write, including both selector and annotation.
    async fn replace_public_v1_service(&self, service: Service) -> Result<(), String>;
}

pub fn public_v1_service_update(
    observed: &Service,
    projection: &ServiceLocationProjection,
) -> Service {
    let mut service = observed.clone();
    let selector = match &projection.location {
        Some(location) => location.replica.instance_id.to_string(),
        None => "disabled".to_string(),
    };
    service.spec.get_or_insert_default().selector = Some(std::collections::BTreeMap::from([(
        crate::crd::INSTANCE_LABEL.to_string(),
        selector,
    )]));
    let annotations = service.metadata.annotations.get_or_insert_default();
    match &projection.location {
        Some(location) => {
            annotations.insert(
                PUBLIC_V1_SERVICE_LOCATION_ANNOTATION.into(),
                location.address.clone(),
            );
        }
        None => {
            annotations.remove(PUBLIC_V1_SERVICE_LOCATION_ANNOTATION);
        }
    }
    service
}

pub fn public_v1_service_matches(
    service: &Service,
    projection: &ServiceLocationProjection,
) -> bool {
    let desired = public_v1_service_update(service, projection);
    service
        .spec
        .as_ref()
        .and_then(|spec| spec.selector.as_ref())
        == desired
            .spec
            .as_ref()
            .and_then(|spec| spec.selector.as_ref())
        && service
            .metadata
            .annotations
            .as_ref()
            .and_then(|map| map.get(PUBLIC_V1_SERVICE_LOCATION_ANNOTATION))
            == desired
                .metadata
                .annotations
                .as_ref()
                .and_then(|map| map.get(PUBLIC_V1_SERVICE_LOCATION_ANNOTATION))
}
