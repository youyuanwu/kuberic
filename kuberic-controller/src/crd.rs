use std::borrow::Cow;
use std::collections::BTreeMap;

use kube::CustomResource;
use kuberic_runtime::protocol::types::AcceptedStatus;
use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

pub const API_GROUP: &str = "operator.kuberic.io";
pub const API_VERSION: &str = "v1alpha1";
pub const CONTROLLER_NAME: &str = "kuberic-controller";
pub const SET_UID_LABEL: &str = "operator.kuberic.io/set-uid";
pub const REPLICA_ID_LABEL: &str = "operator.kuberic.io/replica-id";
pub const INSTANCE_LABEL: &str = "operator.kuberic.io/instance";
pub const CONTROL_ADDRESS_ANNOTATION: &str = "operator.kuberic.io/control-address";
pub const SCALE_UP_ALLOCATION_ANNOTATION: &str =
    "operator.kuberic.io/scale-up-allocation-operation";
pub const DEFAULT_TOPOLOGY_KEY: &str = "kubernetes.io/hostname";

#[derive(CustomResource, Serialize, Deserialize, Debug, PartialEq, Clone, JsonSchema)]
#[kube(
    group = "operator.kuberic.io",
    version = "v1alpha1",
    kind = "KubericSet",
    plural = "kubericsets",
    shortname = "kset",
    derive = "PartialEq",
    namespaced,
    status = "KubericSetStatus",
    printcolumn = r#"{"name":"Replicas","type":"integer","jsonPath":".spec.replicas"}"#,
    printcolumn = r#"{"name":"Initialized","type":"boolean","jsonPath":".status.initialized"}"#,
    printcolumn = r#"{"name":"Age","type":"date","jsonPath":".metadata.creationTimestamp"}"#
)]
#[serde(rename_all = "camelCase")]
pub struct KubericSetSpec {
    #[schemars(range(min = 1))]
    pub replicas: u32,
    #[schemars(length(min = 1))]
    pub image: String,
    #[serde(default = "default_failover_delay_seconds")]
    pub failover_delay_seconds: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placement: Option<PlacementSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub primary_balancing: Option<PrimaryBalancingSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub switchover: Option<PlannedSwitchoverRequestSpec>,
}

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Clone, Hash)]
pub struct LabelKey(String);

impl LabelKey {
    pub fn new(value: impl Into<String>) -> Result<Self, String> {
        let value = value.into();
        validate_label_key(&value)?;
        Ok(Self(value))
    }

    pub fn hostname() -> Self {
        Self(DEFAULT_TOPOLOGY_KEY.to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for LabelKey {
    fn default() -> Self {
        Self::hostname()
    }
}

impl Serialize for LabelKey {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for LabelKey {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

impl JsonSchema for LabelKey {
    fn inline_schema() -> bool {
        true
    }

    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("LabelKey")
    }

    fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type": "string",
            "minLength": 1,
            "maxLength": 317,
            "pattern": "^([a-z0-9]([-a-z0-9.]*[a-z0-9])?/)?[A-Za-z0-9]([-A-Za-z0-9_.]*[A-Za-z0-9])?$"
        })
    }
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq, Clone, Copy, JsonSchema, Default)]
pub enum PlacementMode {
    #[default]
    Preferred,
    Required,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq, Clone, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct PlacementSpec {
    #[serde(default)]
    pub mode: PlacementMode,
    #[serde(default)]
    pub topology_key: LabelKey,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub node_selector: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tolerations: Vec<PlacementTolerationSpec>,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq, Clone, Copy, JsonSchema, Default)]
pub enum TolerationOperator {
    Exists,
    #[default]
    Equal,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq, Clone, Copy, JsonSchema)]
pub enum TolerationEffect {
    NoSchedule,
    PreferNoSchedule,
    NoExecute,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq, Clone, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct PlacementTolerationSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    #[serde(default)]
    pub operator: TolerationOperator,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effect: Option<TolerationEffect>,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq, Clone, Copy, JsonSchema, Default)]
pub enum PrimaryBalancingMode {
    #[default]
    Automatic,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq, Clone, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PrimaryBalancingSpec {
    #[serde(default)]
    pub mode: PrimaryBalancingMode,
    #[serde(default)]
    pub topology_key: LabelKey,
    #[serde(default = "default_primary_balance_cooldown_seconds")]
    #[schemars(range(min = 30, max = 86_400))]
    pub cooldown_seconds: u64,
    #[serde(default = "default_primary_balance_minimum_improvement")]
    #[schemars(range(min = 1))]
    pub minimum_improvement: u32,
}

impl Default for PrimaryBalancingSpec {
    fn default() -> Self {
        Self {
            mode: PrimaryBalancingMode::Automatic,
            topology_key: LabelKey::hostname(),
            cooldown_seconds: default_primary_balance_cooldown_seconds(),
            minimum_improvement: default_primary_balance_minimum_improvement(),
        }
    }
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PlannedSwitchoverRequestSpec {
    #[schemars(length(min = 1))]
    pub request_id: String,
    #[schemars(range(min = 1))]
    pub target_replica_id: u32,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct KubericSetStatus {
    #[serde(flatten)]
    pub authority: AcceptedStatus,
}

const fn default_failover_delay_seconds() -> u64 {
    30
}

const fn default_primary_balance_cooldown_seconds() -> u64 {
    300
}

const fn default_primary_balance_minimum_improvement() -> u32 {
    1
}

fn validate_label_key(value: &str) -> Result<(), String> {
    if value.is_empty() {
        return Err("label key must not be empty".to_string());
    }
    if value.len() > 317 {
        return Err("label key must be at most 317 characters".to_string());
    }
    let mut split = value.split('/');
    let first = split.next().expect("split returns one item");
    let (prefix, name) = if let Some(name) = split.next() {
        if split.next().is_some() {
            return Err("label key must contain at most one '/'".to_string());
        }
        (Some(first), name)
    } else {
        (None, first)
    };
    if let Some(prefix) = prefix {
        validate_dns_subdomain(prefix)?;
    }
    validate_label_name(name)
}

fn validate_dns_subdomain(value: &str) -> Result<(), String> {
    if value.is_empty() || value.len() > 253 {
        return Err("label key prefix must be 1-253 characters".to_string());
    }
    for segment in value.split('.') {
        if segment.is_empty() || segment.len() > 63 {
            return Err("label key prefix segments must be 1-63 characters".to_string());
        }
        if !segment
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
            || !segment
                .bytes()
                .last()
                .is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
            || !segment
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        {
            return Err("label key prefix must be a DNS subdomain".to_string());
        }
    }
    Ok(())
}

fn validate_label_name(value: &str) -> Result<(), String> {
    if value.is_empty() || value.len() > 63 {
        return Err("label key name must be 1-63 characters".to_string());
    }
    if !value
        .bytes()
        .next()
        .is_some_and(|byte| byte.is_ascii_alphanumeric())
        || !value
            .bytes()
            .last()
            .is_some_and(|byte| byte.is_ascii_alphanumeric())
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err("label key name must start and end with an alphanumeric character".to_string());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use kube::CustomResourceExt;
    use serde_json::json;

    use super::*;

    #[test]
    fn scale_down_status_schema_and_legacy_defaults_are_explicit() {
        let schema = serde_json::to_value(KubericSet::crd()).unwrap();
        let root = &schema["spec"]["versions"][0]["schema"]["openAPIV3Schema"]["properties"];
        let spec = &root["spec"]["properties"];
        assert_eq!(spec["replicas"]["minimum"], 1.0);
        assert!(spec.get("removalTarget").is_none());
        assert!(spec.get("minimumReplicas").is_none());
        let status = &root["status"]["properties"];
        let transition = &status["transition"]["properties"];
        assert!(
            transition["kind"]["enum"]
                .as_array()
                .unwrap()
                .contains(&json!("secondaryScaleDown"))
        );
        let intent = &transition["secondaryScaleDown"]["properties"];
        for field in [
            "operationId",
            "resourceUid",
            "specGeneration",
            "desiredReplicas",
            "previousConfiguration",
            "currentConfiguration",
            "previousPolicy",
            "currentPolicy",
            "primary",
            "target",
            "cleanup",
        ] {
            assert!(intent.get(field).is_some(), "{field}");
        }
        assert_eq!(intent["desiredReplicas"]["minimum"], 1.0);
        for resource in ["pod", "pvc", "endpoint"] {
            let identity = &intent["cleanup"]["properties"][resource];
            assert!(
                identity["properties"]["present"]["properties"]
                    .get("name")
                    .is_some()
            );
            assert!(
                identity["properties"]["present"]["properties"]
                    .get("uid")
                    .is_some()
            );
            assert!(
                identity["properties"]["absent"]["properties"]
                    .get("name")
                    .is_some()
            );
            assert!(identity["oneOf"].as_array().unwrap().len() >= 2);
        }
        let cleanup = &status["secondaryScaleDownCleanup"]["properties"];
        assert!(cleanup.get("evidence").is_some());
        assert!(cleanup.get("currentOnlyWriteQuorum").is_some());
        assert!(cleanup.get("retirement").is_some());
        let old: KubericSetStatus = serde_json::from_value(json!({
            "initialized": false, "observedGeneration": 0, "conditions": []
        }))
        .unwrap();
        assert!(old.authority.secondary_scale_down_cleanup.is_none());
        assert!(
            serde_json::from_value::<KubericSetSpec>(json!({"replicas": -1, "image": "db"}))
                .is_err()
        );
    }

    #[test]
    fn scale_up_status_schema_is_typed_and_zero_boundaries_are_allowed() {
        let schema = serde_json::to_value(KubericSet::crd()).unwrap();
        let status = &schema["spec"]["versions"][0]["schema"]["openAPIV3Schema"]["properties"]["status"]
            ["properties"];
        let transition = &status["transition"]["properties"];
        assert!(
            transition["kind"]["enum"]
                .as_array()
                .unwrap()
                .contains(&json!("scaleUp"))
        );
        let intent = &transition["scaleUp"]["properties"];
        for field in [
            "operationId",
            "resourceUid",
            "specGeneration",
            "desiredReplicas",
            "previousConfiguration",
            "currentConfiguration",
            "previousPolicy",
            "currentPolicy",
            "primary",
            "target",
            "buildId",
            "snapshotBoundaryLsn",
            "catchUpBoundaryLsn",
        ] {
            assert!(intent.get(field).is_some(), "{field}");
        }
        assert_eq!(intent["snapshotBoundaryLsn"]["minimum"], 0.0);
        assert_eq!(intent["catchUpBoundaryLsn"]["minimum"], 0.0);
        assert_eq!(intent["target"]["properties"]["replicaId"]["minimum"], 1.0);
        for policy in ["previousPolicy", "currentPolicy"] {
            for field in ["replicaSetSize", "writeQuorum", "readQuorum"] {
                assert_eq!(intent[policy]["properties"][field]["minimum"], 1.0);
            }
        }
        assert!(status.get("scaleUpCleanup").is_some());
        assert!(status.get("lastScaleUp").is_some());
        let failover = &transition["scaleUpFailover"]["properties"];
        for field in [
            "intent",
            "provisionalConfiguration",
            "previousReadQuorum",
            "currentReadQuorum",
            "finalElection",
        ] {
            assert!(failover.get(field).is_some(), "{field}");
        }
        let final_election = &failover["finalElection"]["properties"];
        for field in [
            "selectedPrimaryReplicaId",
            "witnesses",
            "previousReadQuorum",
            "currentReadQuorum",
        ] {
            assert!(final_election.get(field).is_some(), "{field}");
        }
        assert!(final_election.get("finalConfiguration").is_none());
        assert_eq!(final_election["selectedPrimaryReplicaId"]["minimum"], 1.0);
        let final_witness = &final_election["witnesses"]["items"]["properties"];
        for field in [
            "replicaId",
            "processSessionId",
            "reportSequence",
            "currentProgress",
            "committedLsn",
            "deactivatedLsn",
            "fenceOperationId",
        ] {
            assert!(final_witness.get(field).is_some(), "{field}");
        }
        for duplicated in [
            "resourceUid",
            "identity",
            "role",
            "epoch",
            "previousConfigurationId",
            "currentConfigurationId",
            "deactivationEpoch",
            "writeStatus",
            "writeClosed",
            "pendingOperationId",
            "retainedOperationId",
        ] {
            assert!(final_witness.get(duplicated).is_none(), "{duplicated}");
        }
        assert_eq!(final_witness["replicaId"]["minimum"], 1.0);
        assert_eq!(final_witness["reportSequence"]["minimum"], 1.0);
        assert_eq!(final_witness["currentProgress"]["minimum"], 0.0);
        assert_eq!(final_witness["committedLsn"]["minimum"], 0.0);
        assert_eq!(final_witness["deactivatedLsn"]["minimum"], 0.0);
        for quorum in ["previousReadQuorum", "currentReadQuorum"] {
            assert_eq!(final_election[quorum]["items"]["minimum"], 1.0);
        }
        let receipt_failover =
            &status["lastScaleUp"]["properties"]["failoverEvidence"]["properties"];
        for field in [
            "provisionalPrimaryReplicaId",
            "previousReadQuorum",
            "currentReadQuorum",
            "finalElection",
        ] {
            assert!(receipt_failover.get(field).is_some(), "{field}");
        }
        assert!(receipt_failover.get("intent").is_none());
        assert!(receipt_failover.get("provisionalConfiguration").is_none());
        assert_eq!(
            receipt_failover["provisionalPrimaryReplicaId"]["minimum"],
            1.0
        );
        let allocation = &status["scaleUpAllocation"]["properties"];
        for field in [
            "resourceUid",
            "specGeneration",
            "desiredReplicas",
            "previousConfigurationId",
            "acceptedConfigurationId",
            "targetReplicaId",
            "operationId",
            "previousOperationId",
            "scaffoldingRequested",
            "podUid",
            "pvcUid",
            "cancellationStarted",
        ] {
            assert!(allocation.get(field).is_some(), "{field}");
        }
        assert_eq!(allocation["targetReplicaId"]["minimum"], 1.0);

        let provisioning = &status["provisioning"]["properties"]["purpose"]["properties"];
        assert_eq!(
            provisioning["kind"]["enum"],
            json!(["replacement", "scaleUp"])
        );
        assert!(
            provisioning["scaleUp"]["properties"]
                .get("targetReplicaId")
                .is_some()
        );
        assert_eq!(
            provisioning["scaleUp"]["properties"]["targetReplicaId"]["minimum"],
            1.0
        );

        let generated = serde_json::to_string_pretty(&KubericSet::crd()).unwrap();
        assert!(
            generated.len() < 350_000,
            "generated CRD unexpectedly grew to {} bytes",
            generated.len()
        );
        assert!(
            generated.len() <= 348_700,
            "compact placement schema lost its reviewed CRD headroom at {} bytes",
            generated.len()
        );
    }

    #[test]
    fn crd_uses_only_the_level_triggered_api_group() {
        let crd = KubericSet::crd();
        assert_eq!(crd.spec.group, API_GROUP);
        assert_eq!(crd.spec.versions.len(), 1);
        assert_eq!(crd.spec.versions[0].name, API_VERSION);
        assert_eq!(crd.spec.names.plural, "kubericsets");
    }

    #[test]
    fn schema_rejects_zero_replicas_and_malformed_status() {
        let schema = serde_json::to_value(KubericSet::crd()).unwrap();
        let replicas = &schema["spec"]["versions"][0]["schema"]["openAPIV3Schema"]["properties"]["spec"]
            ["properties"]["replicas"];
        assert_eq!(replicas["minimum"].as_f64(), Some(1.0));
        let switchover = &schema["spec"]["versions"][0]["schema"]["openAPIV3Schema"]["properties"]
            ["spec"]["properties"]["switchover"]["properties"];
        assert_eq!(switchover["requestId"]["minLength"].as_u64(), Some(1));
        assert_eq!(switchover["targetReplicaId"]["minimum"].as_f64(), Some(1.0));
        let spec = &schema["spec"]["versions"][0]["schema"]["openAPIV3Schema"]["properties"]["spec"]
            ["properties"];
        let placement = &spec["placement"]["properties"];
        assert_eq!(placement["mode"]["enum"], json!(["Preferred", "Required"]));
        assert_eq!(
            placement["topologyKey"]["pattern"].as_str(),
            Some("^([a-z0-9]([-a-z0-9.]*[a-z0-9])?/)?[A-Za-z0-9]([-A-Za-z0-9_.]*[A-Za-z0-9])?$")
        );
        let toleration = &placement["tolerations"]["items"]["properties"];
        assert_eq!(toleration["operator"]["enum"], json!(["Exists", "Equal"]));
        assert_eq!(
            toleration["effect"]["enum"],
            json!(["NoSchedule", "PreferNoSchedule", "NoExecute", null])
        );
        let primary_balancing = &spec["primaryBalancing"]["properties"];
        assert_eq!(primary_balancing["mode"]["enum"], json!(["Automatic"]));
        assert_eq!(
            primary_balancing["cooldownSeconds"]["minimum"].as_f64(),
            Some(30.0)
        );
        assert_eq!(
            primary_balancing["cooldownSeconds"]["maximum"].as_f64(),
            Some(86_400.0)
        );
        assert_eq!(
            primary_balancing["minimumImprovement"]["minimum"].as_f64(),
            Some(1.0)
        );
        let defaulted: KubericSetSpec = serde_json::from_value(
            json!({"replicas": 1, "image": "db", "placement": {}, "primaryBalancing": {}}),
        )
        .unwrap();
        assert_eq!(defaulted.placement.unwrap().mode, PlacementMode::Preferred);
        assert_eq!(
            defaulted.primary_balancing.unwrap().cooldown_seconds,
            default_primary_balance_cooldown_seconds()
        );
        assert!(
            serde_json::from_value::<KubericSetSpec>(json!({
                "replicas": 1, "image": "db", "placement": {"topologyKey": "bad/key/"}
            }))
            .is_err()
        );
        assert!(
            serde_json::from_value::<KubericSetSpec>(json!({
                "replicas": 1, "image": "db", "placement": {"mode": "Disabled"}
            }))
            .is_err()
        );

        let status =
            &schema["spec"]["versions"][0]["schema"]["openAPIV3Schema"]["properties"]["status"];
        let required = status["required"].as_array().unwrap();
        assert!(required.iter().any(|value| value == "initialized"));
        assert!(required.iter().any(|value| value == "observedGeneration"));
        assert!(status["properties"].get("authority").is_none());
        let topology = &status["properties"]["topology"];
        assert!(topology["properties"].get("configuration").is_none());
        assert!(topology["properties"].get("members").is_some());
        assert!(topology["properties"].get("primaryId").is_none());
        let member = &topology["properties"]["members"]["items"];
        assert!(member["properties"].get("identity").is_none());
        assert!(member["properties"].get("replicaId").is_some());
        assert!(member["properties"].get("instanceId").is_some());
        assert!(member["properties"].get("agentGeneration").is_some());
        let provisioning = &status["properties"]["provisioning"]["properties"];
        assert_eq!(
            provisioning
                .as_object()
                .unwrap()
                .keys()
                .map(String::as_str)
                .collect::<std::collections::BTreeSet<_>>(),
            std::collections::BTreeSet::from(["operationId", "podUid", "purpose", "pvcUid"])
        );
        let purpose = &provisioning["purpose"]["properties"];
        assert_eq!(purpose["kind"]["enum"], json!(["replacement", "scaleUp"]));
        assert!(purpose.get("replaces").is_some());
        assert!(purpose.get("scaleUp").is_some());
        assert!(
            status["properties"]["transition"]["properties"]
                .get("startedAtUnixSeconds")
                .is_none()
        );
        assert!(status["properties"].get("primaryFailure").is_some());
        assert!(status["properties"].get("quorumLoss").is_some());
        assert!(status["properties"].get("lastSwitchover").is_some());
        assert!(
            !required
                .iter()
                .any(|value| value == "pendingReplacementCleanup")
        );
        assert_eq!(
            status["properties"]["pendingReplacementCleanup"]["properties"],
            status["properties"]["lastReplacement"]["properties"]
        );
        assert!(
            status["properties"]["quorumLoss"]["properties"]
                .get("startedAtUnixSeconds")
                .is_none()
        );
        assert!(
            status["properties"]["transition"]["properties"]
                .get("repair")
                .is_some()
        );
        assert!(
            status["properties"]["transition"]["properties"]
                .get("electionLsn")
                .is_some()
        );
        assert!(
            status["properties"]["transition"]["properties"]
                .get("switchover")
                .is_some()
        );
        assert!(
            status["properties"]["transition"]["properties"]["currentConfiguration"]["properties"]
                .get("primaryId")
                .is_none()
        );
    }

    #[test]
    fn malformed_status_does_not_deserialize() {
        let value = json!({
            "apiVersion": "operator.kuberic.io/v1alpha1",
            "kind": "KubericSet",
            "metadata": {"name": "db"},
            "spec": {"replicas": 3, "image": "example/db:latest"},
            "status": {"initialized": "yes"}
        });
        assert!(serde_json::from_value::<KubericSet>(value).is_err());
    }

    #[test]
    fn deploy_assets_are_isolated_from_the_classic_operator() {
        let crd: serde_yaml_ng::Value =
            serde_yaml_ng::from_str(include_str!("../deploy/crd.json")).unwrap();
        let generated = serde_json::to_value(KubericSet::crd()).unwrap();
        let checked_in: serde_json::Value =
            serde_json::from_str(include_str!("../deploy/crd.json")).unwrap();
        assert_eq!(checked_in, generated);
        assert_eq!(crd["spec"]["group"].as_str(), Some("operator.kuberic.io"));
        assert_eq!(
            crd["spec"]["versions"][0]["name"].as_str(),
            Some("v1alpha1")
        );
        assert_eq!(
            crd["spec"]["versions"][0]["schema"]["openAPIV3Schema"]["properties"]["spec"]
                ["properties"]["replicas"]["minimum"]
                .as_f64(),
            Some(1.0)
        );

        for manifest in [
            include_str!("../deploy/rbac.yaml"),
            include_str!("../deploy/service-account.yaml"),
            include_str!("../deploy/deployment.yaml"),
            include_str!("../deploy/kustomization.yaml"),
        ] {
            assert!(!manifest.contains("apiGroups:\n      - kuberic.io"));
            assert!(!manifest.contains("kuberic-operator"));
        }
        assert!(include_str!("../Dockerfile").contains("protobuf-compiler"));
    }
}
