use std::collections::BTreeMap;

use kuberic_runtime::protocol::observation::{
    DesiredState, ObservationFailure, ObservationSnapshot, ReplicaObservation,
    ReplicaObservationKey, ReportWatermark, RoutingObservation,
    SecondaryScaleDownResourceObservation,
};
use kuberic_runtime::protocol::public_operations::PublicOperationPreviewIdentity;
use kuberic_runtime::protocol::types::{AcceptedStatus, ResourceUid};
use serde::Deserialize;

use super::EvaluationConfig;

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
}

#[derive(Deserialize)]
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
    }
}
