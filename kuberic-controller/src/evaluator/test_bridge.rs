use std::collections::BTreeMap;

use kuberic_runtime::protocol::observation::{
    DesiredState, ObservationFailure, ObservationSnapshot, ReplicaObservation,
    ReplicaObservationKey, ReportWatermark, RoutingObservation,
    SecondaryScaleDownResourceObservation,
};
use kuberic_runtime::protocol::public_operations::PublicOperationPreviewIdentity;
use kuberic_runtime::protocol::types::{AcceptedStatus, ResourceUid};
use serde::{Deserialize, Serialize};

use super::EvaluationConfig;

pub use super::public_lifecycle::{
    PreviewAcceptedStatus, PreviewServiceLocationPlan, PreviewServiceLocationStage,
    PreviewTransition, ServiceLocationProjection, evaluate_service_location, plan_public_lifecycle,
};
pub use crate::cluster_api::{PreviewServiceApi, preview_service_matches, preview_service_update};
pub use crate::executor::execute_preview_service_location;
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

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct SnapshotBridge {
    resource_uid: ResourceUid,
    resource_version: String,
    desired: DesiredState,
    status: AcceptedStatus,
    replicas: Vec<(ReplicaObservationKey, ReplicaObservation)>,
    #[serde(default)]
    secondary_scale_down_resources: Vec<SecondaryScaleDownResourceObservation>,
    previous_report_watermarks: Vec<(ReplicaObservationKey, ReportWatermark)>,
    durable_storage_evidence: bool,
    supporting_resources_ready: bool,
    routing: RoutingObservation,
    observation_failures: Vec<ObservationFailure>,
    now_unix_seconds: i64,
}

impl From<SnapshotBridge> for ObservationSnapshot {
    fn from(snapshot: SnapshotBridge) -> Self {
        Self {
            resource_uid: snapshot.resource_uid,
            resource_version: snapshot.resource_version,
            desired: snapshot.desired,
            status: snapshot.status,
            replicas: snapshot.replicas.into_iter().collect::<BTreeMap<_, _>>(),
            secondary_scale_down_resources: snapshot.secondary_scale_down_resources,
            previous_report_watermarks: snapshot
                .previous_report_watermarks
                .into_iter()
                .collect::<BTreeMap<_, _>>(),
            durable_storage_evidence: snapshot.durable_storage_evidence,
            supporting_resources_ready: snapshot.supporting_resources_ready,
            routing: snapshot.routing,
            observation_failures: snapshot.observation_failures,
            now_unix_seconds: snapshot.now_unix_seconds,
        }
    }
}

pub fn evaluate_json(snapshot: &[u8], config: &EvaluationConfig) -> serde_json::Result<Vec<u8>> {
    let snapshot: SnapshotBridge = serde_json::from_slice(snapshot)?;
    serde_json::to_vec(&super::evaluate(&snapshot.into(), config))
}

pub fn evaluate_preview_json(
    snapshot: &[u8],
    config: &PublicOperationPreviewEvaluationConfig,
) -> serde_json::Result<Vec<u8>> {
    let value: serde_json::Value = serde_json::from_slice(snapshot)?;
    let preview: PublicOperationPreviewIdentity =
        serde_json::from_value(value.get("preview").cloned().ok_or_else(|| {
            serde_json::Error::io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "public-operation preview observation is missing its identity",
            ))
        })?)?;
    config.validate_identity(&preview).map_err(|message| {
        serde_json::Error::io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            message,
        ))
    })?;
    let snapshot: SnapshotBridge = serde_json::from_value(value)?;
    serde_json::to_vec(&serde_json::json!({
        "preview": preview,
        "plan": super::evaluate(&snapshot.into(), &config.evaluation),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview_configuration_retains_the_exact_identity() {
        let identity = PublicOperationPreviewIdentity::new(11);
        let config = PublicOperationPreviewEvaluationConfig::new(
            identity.clone(),
            EvaluationConfig::default(),
        );
        assert_eq!(config.identity, identity);
        assert_eq!(
            config.evaluation.supported_protocol_version,
            kuberic_runtime::protocol::PROTOCOL_VERSION
        );
        assert!(config.validate_identity(&identity).is_ok());
        assert!(
            config
                .validate_identity(&PublicOperationPreviewIdentity::new(12))
                .is_err()
        );
        let error = evaluate_preview_json(
            br#"{"preview":{"protocolVersion":10,"generation":12}}"#,
            &config,
        )
        .unwrap_err();
        assert!(error.to_string().contains("preview identity mismatch"));

        let snapshot = SnapshotBridge {
            resource_uid: ResourceUid::new("resource-1"),
            resource_version: "1".into(),
            desired: DesiredState {
                generation: 1,
                replicas: 1,
                image: "test".into(),
                failover_delay_seconds: 30,
                switchover: None,
            },
            status: AcceptedStatus::default(),
            replicas: Vec::new(),
            secondary_scale_down_resources: Vec::new(),
            previous_report_watermarks: Vec::new(),
            durable_storage_evidence: false,
            supporting_resources_ready: false,
            routing: RoutingObservation::default(),
            observation_failures: Vec::new(),
            now_unix_seconds: 0,
        };
        let mut value = serde_json::to_value(snapshot).unwrap();
        value
            .as_object_mut()
            .unwrap()
            .insert("preview".into(), serde_json::to_value(&identity).unwrap());
        let output = evaluate_preview_json(&serde_json::to_vec(&value).unwrap(), &config).unwrap();
        let output: serde_json::Value = serde_json::from_slice(&output).unwrap();
        assert_eq!(output["preview"], serde_json::to_value(&identity).unwrap());
        assert!(output.get("plan").is_some());
    }
}
