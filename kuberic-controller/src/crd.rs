use kube::CustomResource;
use kuberic_runtime::protocol::command::PublicV1OperationCommand;
use kuberic_runtime::protocol::public_operations::{
    PublicSessionRenewalStage, PublicV1AcceptedStatus, PublicV1CallKind, PublicV1Identity,
    PublicV1SessionRenewalStatus,
};
use kuberic_runtime::protocol::types::{
    ConfigurationDescriptor, ConfigurationId, Epoch, HistoryContext, HistoryInitializationId,
    OperationId, ProcessSessionEndpoint, ReplicaId, ReplicaIdentity,
    ReplicaStorageIncarnationContext, ResourceUid,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub const API_GROUP: &str = "operator.kuberic.io";
pub const API_VERSION: &str = "v1alpha1";
pub const CONTROLLER_NAME: &str = "kuberic-controller";
pub const SET_UID_LABEL: &str = "operator.kuberic.io/set-uid";
pub const REPLICA_ID_LABEL: &str = "operator.kuberic.io/replica-id";
pub const INSTANCE_LABEL: &str = "operator.kuberic.io/instance";
pub const CONTROL_ADDRESS_ANNOTATION: &str = "operator.kuberic.io/control-address";
pub const SCALE_UP_ALLOCATION_ANNOTATION: &str =
    "operator.kuberic.io/scale-up-allocation-operation";

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
    printcolumn = r#"{"name":"Age","type":"date","jsonPath":".metadata.creationTimestamp"}"#
)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct KubericSetSpec {
    #[schemars(range(min = 1))]
    pub replicas: u32,
    #[schemars(length(min = 1))]
    pub image: String,
    #[serde(default = "default_failover_delay_seconds")]
    pub failover_delay_seconds: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub switchover: Option<PlannedSwitchoverRequestSpec>,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PlannedSwitchoverRequestSpec {
    #[schemars(length(min = 1))]
    pub request_id: String,
    #[schemars(range(min = 1))]
    pub target_replica_id: u32,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone, JsonSchema, Default)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct KubericSetStatus {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub public_v1_authority: Option<PublicV1AuthorityStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub public_v1_topology: Option<PublicV1TopologyState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub public_v1_session_renewal: Option<PublicV1SessionRenewalState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub public_v1_replacement: Option<PublicV1ReplacementState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub public_v1_transition: Option<PublicV1TransitionState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub public_v1_operation: Option<PublicV1OperationStatus>,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PublicV1TopologyState {
    pub configuration_id: ConfigurationId,
    pub epoch: Epoch,
    pub primary_id: ReplicaId,
    pub member_count: u32,
    pub sessions: Vec<ProcessSessionEndpoint>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unavailable_primary_id: Option<ReplicaId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unavailable_since_unix_seconds: Option<i64>,
    #[schemars(length(min = 2))]
    pub canonical_configuration_json: String,
}

impl PublicV1TopologyState {
    pub fn from_configuration(
        configuration: &ConfigurationDescriptor,
        sessions: &[ProcessSessionEndpoint],
    ) -> Result<Self, String> {
        if configuration.configuration_id != configuration.expected_id()
            || configuration.members.is_empty()
            || configuration
                .members
                .iter()
                .filter(|member| {
                    member.role == kuberic_runtime::protocol::types::ReplicaRole::Primary
                })
                .count()
                != 1
            || sessions.len() != configuration.members.len()
            || sessions.iter().any(|session| {
                session.validate().is_err()
                    || !configuration.members.iter().any(|member| {
                        member.identity == session.identity && member.role == session.role
                    })
            })
            || sessions
                .iter()
                .map(|session| session.identity.replica_id)
                .collect::<std::collections::BTreeSet<_>>()
                .len()
                != sessions.len()
        {
            return Err("invalid public-v1 topology configuration or sessions".into());
        }
        Ok(Self {
            configuration_id: configuration.configuration_id.clone(),
            epoch: configuration.epoch,
            primary_id: configuration.primary_id,
            member_count: configuration
                .members
                .len()
                .try_into()
                .map_err(|_| "public-v1 topology member count exceeds u32".to_string())?,
            sessions: sessions.to_vec(),
            unavailable_primary_id: None,
            unavailable_since_unix_seconds: None,
            canonical_configuration_json: serde_json::to_string(configuration)
                .map_err(|error| error.to_string())?,
        })
    }

    pub fn configuration(&self) -> Result<ConfigurationDescriptor, String> {
        let configuration: ConfigurationDescriptor =
            serde_json::from_str(&self.canonical_configuration_json)
                .map_err(|error| error.to_string())?;
        if configuration.configuration_id != self.configuration_id
            || configuration.epoch != self.epoch
            || configuration.primary_id != self.primary_id
            || configuration.members.len() != self.member_count as usize
            || configuration.configuration_id != configuration.expected_id()
            || self.sessions.len() != configuration.members.len()
            || self.sessions.iter().any(|session| {
                session.validate().is_err()
                    || !configuration.members.iter().any(|member| {
                        member.identity == session.identity && member.role == session.role
                    })
            })
            || self
                .sessions
                .iter()
                .map(|session| session.identity.replica_id)
                .collect::<std::collections::BTreeSet<_>>()
                .len()
                != self.sessions.len()
            || self.unavailable_primary_id.is_some()
                != self.unavailable_since_unix_seconds.is_some()
            || self
                .unavailable_primary_id
                .is_some_and(|primary| primary != configuration.primary_id)
            || self
                .unavailable_since_unix_seconds
                .is_some_and(|since| since < 0)
        {
            return Err("public-v1 topology summary differs from canonical configuration".into());
        }
        Ok(configuration)
    }

    pub fn record_primary_unavailable(&mut self, primary_id: ReplicaId, since: i64) {
        self.unavailable_primary_id = Some(primary_id);
        self.unavailable_since_unix_seconds = Some(since);
    }

    pub fn clear_primary_unavailable(&mut self) {
        self.unavailable_primary_id = None;
        self.unavailable_since_unix_seconds = None;
    }
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PublicV1AuthorityStatus {
    pub identity: PublicV1Identity,
    pub history_initialization_id: HistoryInitializationId,
    pub history: HistoryContext,
    pub completed_operation_count: u32,
    #[schemars(length(min = 2))]
    pub canonical_authority_json: String,
}

impl PublicV1AuthorityStatus {
    pub fn from_authority(authority: &PublicV1AcceptedStatus) -> Result<Self, String> {
        authority.validate_evidence().map_err(str::to_owned)?;
        Ok(Self {
            identity: authority.identity.clone(),
            history_initialization_id: authority.history_initialization_id.clone(),
            history: authority.history.clone(),
            completed_operation_count: authority
                .completed_operations
                .len()
                .try_into()
                .map_err(|_| "public-v1 completion count exceeds u32".to_string())?,
            canonical_authority_json: serde_json::to_string(authority)
                .map_err(|error| error.to_string())?,
        })
    }

    pub fn authority(&self) -> Result<PublicV1AcceptedStatus, String> {
        let authority: PublicV1AcceptedStatus =
            serde_json::from_str(&self.canonical_authority_json)
                .map_err(|error| error.to_string())?;
        authority.validate_evidence().map_err(str::to_owned)?;
        if authority.identity != self.identity
            || authority.history_initialization_id != self.history_initialization_id
            || authority.history != self.history
            || authority.completed_operations.len() != self.completed_operation_count as usize
        {
            return Err("public-v1 authority summary differs from canonical authority".into());
        }
        Ok(authority)
    }
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PublicV1SessionRenewalState {
    pub operation_id: OperationId,
    pub resource_uid: ResourceUid,
    pub stage: PublicSessionRenewalStage,
    #[schemars(length(min = 2))]
    pub canonical_renewal_json: String,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PublicV1ReplacementState {
    pub operation_id: OperationId,
    pub predecessor: ReplicaStorageIncarnationContext,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replacement: Option<ReplicaStorageIncarnationContext>,
}

impl PublicV1ReplacementState {
    pub fn new(
        configuration_id: &ConfigurationId,
        predecessor: ReplicaStorageIncarnationContext,
    ) -> Result<Self, String> {
        predecessor.validate().map_err(str::to_owned)?;
        Ok(Self {
            operation_id: OperationId::new(format!(
                "replacement-{}-{}-{}",
                configuration_id, predecessor.replica.replica_id, predecessor.replica.instance_id
            )),
            predecessor,
            replacement: None,
        })
    }

    pub fn bind(&self, replacement: ReplicaStorageIncarnationContext) -> Result<Self, String> {
        self.validate()?;
        replacement.validate().map_err(str::to_owned)?;
        if self.replacement.is_some()
            || replacement.replica.replica_id != self.predecessor.replica.replica_id
            || replacement.replica == self.predecessor.replica
            || replacement == self.predecessor
        {
            return Err("public-v1 replacement target differs from predecessor authority".into());
        }
        Ok(Self {
            operation_id: self.operation_id.clone(),
            predecessor: self.predecessor.clone(),
            replacement: Some(replacement),
        })
    }

    pub fn validate(&self) -> Result<(), String> {
        self.predecessor.validate().map_err(str::to_owned)?;
        if self.operation_id.is_empty()
            || self.replacement.as_ref().is_some_and(|replacement| {
                replacement.validate().is_err()
                    || replacement.replica.replica_id != self.predecessor.replica.replica_id
                    || replacement.replica == self.predecessor.replica
                    || replacement == &self.predecessor
            })
        {
            return Err("public-v1 replacement authority is invalid".into());
        }
        Ok(())
    }
}

impl PublicV1SessionRenewalState {
    pub fn from_renewal(renewal: &PublicV1SessionRenewalStatus) -> Result<Self, String> {
        renewal.validate().map_err(str::to_owned)?;
        Ok(Self {
            operation_id: renewal.required.operation_id.clone(),
            resource_uid: renewal.required.resource_uid.clone(),
            stage: renewal.stage,
            canonical_renewal_json: serde_json::to_string(renewal)
                .map_err(|error| error.to_string())?,
        })
    }

    pub fn renewal(&self) -> Result<PublicV1SessionRenewalStatus, String> {
        let renewal: PublicV1SessionRenewalStatus =
            serde_json::from_str(&self.canonical_renewal_json)
                .map_err(|error| error.to_string())?;
        renewal.validate().map_err(str::to_owned)?;
        if renewal.required.operation_id != self.operation_id
            || renewal.required.resource_uid != self.resource_uid
            || renewal.stage != self.stage
        {
            return Err("public-v1 renewal summary differs from canonical renewal".into());
        }
        Ok(renewal)
    }
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PublicV1TransitionState {
    pub resource_version: String,
    pub operation_id: OperationId,
    #[schemars(range(min = 1))]
    pub revision: u64,
    pub call_kind: PublicV1CallKind,
    pub history: HistoryContext,
    pub source_storage: ReplicaStorageIncarnationContext,
    #[schemars(length(min = 2))]
    pub canonical_transition_json: String,
}

impl PublicV1TransitionState {
    pub fn from_transition(
        transition: &crate::evaluator::public_v1_evidence::PublicV1AcceptedTransitionStatus,
    ) -> Result<Self, String> {
        transition.validate()?;
        Ok(Self {
            resource_version: transition.resource_version.clone(),
            operation_id: transition.intent.operation_id.clone(),
            revision: transition.intent.revision,
            call_kind: transition.intent.call_kind,
            history: transition.intent.history.clone(),
            source_storage: transition.intent.source.storage_incarnation.clone(),
            canonical_transition_json: serde_json::to_string(transition)
                .map_err(|error| error.to_string())?,
        })
    }

    pub fn transition(
        &self,
    ) -> Result<crate::evaluator::public_v1_evidence::PublicV1AcceptedTransitionStatus, String>
    {
        let transition = serde_json::from_str(&self.canonical_transition_json)
            .map_err(|error| error.to_string())?;
        crate::evaluator::public_v1_evidence::PublicV1AcceptedTransitionStatus::validate(
            &transition,
        )?;
        if transition.resource_version != self.resource_version
            || transition.intent.operation_id != self.operation_id
            || transition.intent.revision != self.revision
            || transition.intent.call_kind != self.call_kind
            || transition.intent.history != self.history
            || transition.intent.source.storage_incarnation != self.source_storage
        {
            return Err("public-v1 transition summary differs from canonical transition".into());
        }
        Ok(transition)
    }
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PublicV1OperationStatus {
    pub target: ReplicaIdentity,
    pub operation_id: OperationId,
    #[schemars(range(min = 1))]
    pub revision: u64,
    #[schemars(length(min = 2))]
    pub canonical_command_json: String,
}

impl PublicV1OperationStatus {
    pub fn from_command(command: &PublicV1OperationCommand) -> Result<Self, String> {
        command.validate().map_err(str::to_owned)?;
        Ok(Self {
            target: command.target.clone(),
            operation_id: command.intent.operation_id.clone(),
            revision: command.intent.revision,
            canonical_command_json: serde_json::to_string(command)
                .map_err(|error| error.to_string())?,
        })
    }

    pub fn command(&self) -> Result<PublicV1OperationCommand, String> {
        let command: PublicV1OperationCommand = serde_json::from_str(&self.canonical_command_json)
            .map_err(|error| error.to_string())?;
        command.validate().map_err(str::to_owned)?;
        if command.target != self.target
            || command.intent.operation_id != self.operation_id
            || command.intent.revision != self.revision
        {
            return Err("public-v1 operation summary differs from canonical command".into());
        }
        Ok(command)
    }
}

impl KubericSetStatus {
    pub fn validate_public_v1(&self) -> Result<(), String> {
        let authority = self
            .public_v1_authority
            .as_ref()
            .map(PublicV1AuthorityStatus::authority)
            .transpose()?;
        let renewal = self
            .public_v1_session_renewal
            .as_ref()
            .map(PublicV1SessionRenewalState::renewal)
            .transpose()?;
        let topology = self
            .public_v1_topology
            .as_ref()
            .map(PublicV1TopologyState::configuration)
            .transpose()?;
        let transition = self
            .public_v1_transition
            .as_ref()
            .map(PublicV1TransitionState::transition)
            .transpose()?;
        if let Some(replacement) = &self.public_v1_replacement {
            replacement.validate()?;
        }
        if let Some(renewal) = &renewal
            && authority.as_ref().is_none_or(|accepted| {
                accepted.history != renewal.required.history
                    || accepted.history.resource_uid != renewal.required.resource_uid
            })
        {
            return Err("public-v1 session renewal lacks exact accepted history".into());
        }
        if let Some(topology) = &topology
            && topology
                .members
                .iter()
                .filter(|member| {
                    member.role == kuberic_runtime::protocol::types::ReplicaRole::Primary
                })
                .count()
                != 1
        {
            return Err("public-v1 topology lacks exactly one primary".into());
        }
        if let Some(transition) = &transition {
            if authority.as_ref() != Some(&transition.evidence) {
                return Err("public-v1 transition evidence differs from accepted history".into());
            }
            let operation = self
                .public_v1_operation
                .as_ref()
                .ok_or_else(|| "public-v1 transition lacks its operation command".to_string())?
                .command()?;
            if !crate::cluster_api::public_v1_operations::operation_matches_transition(
                transition,
                &operation.intent,
            ) || operation.target != transition.intent.source.storage_incarnation.replica
            {
                return Err("public-v1 operation differs from accepted transition".into());
            }
        } else if self.public_v1_operation.is_some() {
            return Err("public-v1 operation lacks accepted transition authority".into());
        }
        Ok(())
    }
}

const fn default_failover_delay_seconds() -> u64 {
    30
}

#[cfg(test)]
mod tests {
    use kube::CustomResourceExt;
    use serde_json::json;

    use super::*;

    #[test]
    fn crd_schema_is_public_v1_only_and_structurally_typed() {
        let schema = serde_json::to_value(KubericSet::crd()).unwrap();
        let root = &schema["spec"]["versions"][0]["schema"]["openAPIV3Schema"]["properties"];
        let spec = &root["spec"]["properties"];
        assert_eq!(spec["replicas"]["minimum"], 1.0);
        assert!(spec.get("previewLifecycle").is_none());

        let status = &root["status"]["properties"];
        assert_eq!(
            status
                .as_object()
                .unwrap()
                .keys()
                .map(String::as_str)
                .collect::<std::collections::BTreeSet<_>>(),
            std::collections::BTreeSet::from([
                "publicV1Authority",
                "publicV1Operation",
                "publicV1Replacement",
                "publicV1SessionRenewal",
                "publicV1Topology",
                "publicV1Transition",
            ])
        );
        for field in [
            "publicV1Authority",
            "publicV1Operation",
            "publicV1SessionRenewal",
            "publicV1Topology",
            "publicV1Transition",
        ] {
            assert!(
                status[field].get("properties").is_some()
                    || status[field]
                        .get("oneOf")
                        .and_then(serde_json::Value::as_array)
                        .is_some_and(|variants| variants
                            .iter()
                            .any(|value| value.get("properties").is_some())),
                "{field} is not structurally typed: {}",
                status[field]
            );
        }
    }

    #[test]
    fn serde_rejects_legacy_and_malformed_crd_input() {
        assert!(
            serde_json::from_value::<KubericSetSpec>(json!({
                "replicas": 1,
                "image": "db",
                "previewLifecycle": {"statePersistence": "persistent"}
            }))
            .is_err()
        );
        assert!(
            serde_json::from_value::<KubericSetStatus>(json!({
                "initialized": false,
                "observedGeneration": 0,
                "conditions": []
            }))
            .is_err()
        );
        assert!(
            serde_json::from_value::<KubericSetStatus>(json!({
                "publicV1Authority": {
                    "identity": {"protocolVersion": 11, "generation": 1}
                }
            }))
            .is_err()
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
}
