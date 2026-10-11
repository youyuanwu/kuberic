//! Dormant public-operation preview identities and admission inputs.

use base64::prelude::{BASE64_STANDARD, Engine as _};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::protocol::authorization::{
    AuthorizationAuditDigest as SignedAuthorizationAuditDigest, AuthorizationKeyId,
    AuthorizationSignature, AuthorizationVerificationKey, BuildAuthorizationClaimsV1,
    BuildAuthorizationV1, PeerSessionAuthorizationClaimsV1, PeerSessionAuthorizationV1,
};
use crate::protocol::types::{
    AccessStatus, ConfigurationDescriptor, ConfigurationId, Epoch, FaultType, HistoryContext,
    OperationId, PodUid, ProcessSessionEndpoint, ProcessSessionId, ProcessSessionRenewalReason,
    PublicProtocolGeneration, PvcUid, ReplicaId, ReplicaIdentity, ReplicaRole,
    ReplicaStorageIdentity, ReplicaStorageIncarnationContext, ReplicatorCreationId, ResourceUid,
    derive_data_loss_history,
};

pub const PUBLIC_OPERATION_PREVIEW_PROTOCOL_VERSION: u32 = 10;
pub const PUBLIC_V1_PREVIEW_PROTOCOL_VERSION: u32 = 11;
pub const INVALID_LSN: i64 = -1;

#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "camelCase")]
pub struct PublicOperationPreviewIdentity {
    pub protocol_version: u32,
    pub generation: u64,
}

impl PublicOperationPreviewIdentity {
    pub const fn new(generation: u64) -> Self {
        Self {
            protocol_version: PUBLIC_OPERATION_PREVIEW_PROTOCOL_VERSION,
            generation,
        }
    }

    pub const fn public_v1(generation: u64) -> Self {
        Self {
            protocol_version: PUBLIC_V1_PREVIEW_PROTOCOL_VERSION,
            generation,
        }
    }

    pub const fn is_legacy_preview(&self) -> bool {
        self.protocol_version == PUBLIC_OPERATION_PREVIEW_PROTOCOL_VERSION && self.generation > 0
    }

    pub const fn is_public_v1(&self) -> bool {
        self.protocol_version == PUBLIC_V1_PREVIEW_PROTOCOL_VERSION && self.generation > 0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum StatePersistence {
    Persisted,
    Volatile,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PreviewLifecycleBinding {
    pub preview: PublicOperationPreviewIdentity,
    pub resource_uid: ResourceUid,
    pub spec_generation: u64,
    pub state_persistence: StatePersistence,
}

impl PreviewLifecycleBinding {
    pub fn validate(&self) -> Result<(), &'static str> {
        if !self.preview.is_valid() {
            return Err("invalid public-operation preview identity");
        }
        if self.resource_uid.is_empty() {
            return Err("preview lifecycle resource UID is empty");
        }
        if self.spec_generation == 0 {
            return Err("preview lifecycle spec generation must be positive");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum PublicFaultActionKind {
    Restart,
    DropReplacement,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FrozenReplicaResources {
    pub pod_name: String,
    pub pod_uid: PodUid,
    pub pvc_name: String,
    pub pvc_uid: PvcUid,
    pub endpoint_name: String,
    pub endpoint_uid: String,
    pub endpoint_resource_version: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PublicFaultAction {
    pub action_id: OperationId,
    pub fault_operation_id: OperationId,
    pub binding: PreviewLifecycleBinding,
    pub target: ReplicaIdentity,
    pub resources: FrozenReplicaResources,
    pub predecessor_session: ProcessSessionId,
    pub predecessor_process_id: u32,
    pub fault_revision: u64,
    pub fault: FaultType,
    pub kind: PublicFaultActionKind,
}

impl PublicFaultAction {
    pub fn expected_kind(persistence: StatePersistence, fault: FaultType) -> PublicFaultActionKind {
        if fault == FaultType::Transient && persistence == StatePersistence::Persisted {
            PublicFaultActionKind::Restart
        } else {
            PublicFaultActionKind::DropReplacement
        }
    }

    pub fn expected_id(&self) -> OperationId {
        use sha2::Digest;
        let digest = sha2::Sha256::digest(
            format!(
                "{kind:?}|{fault:?}|{fault_operation_id}|{persistence:?}|{resource_uid}|{protocol_version}|\
                 {preview_generation}|{spec_generation}|{replica_id}|{instance_id}|\
                 {agent_generation}|{predecessor_session}|{fault_revision}|{pod_name}|\
                 {predecessor_process_id}|{pod_uid}|{pvc_name}|{pvc_uid}|{endpoint_name}|{endpoint_uid}|\
                 {endpoint_resource_version}",
                kind = self.kind,
                fault = self.fault,
                fault_operation_id = self.fault_operation_id,
                persistence = self.binding.state_persistence,
                resource_uid = self.binding.resource_uid,
                protocol_version = self.binding.preview.protocol_version,
                preview_generation = self.binding.preview.generation,
                spec_generation = self.binding.spec_generation,
                replica_id = self.target.replica_id,
                instance_id = self.target.instance_id,
                agent_generation = self.target.agent_generation,
                predecessor_session = self.predecessor_session,
                fault_revision = self.fault_revision,
                pod_name = self.resources.pod_name,
                predecessor_process_id = self.predecessor_process_id,
                pod_uid = self.resources.pod_uid,
                pvc_name = self.resources.pvc_name,
                pvc_uid = self.resources.pvc_uid,
                endpoint_name = self.resources.endpoint_name,
                endpoint_uid = self.resources.endpoint_uid,
                endpoint_resource_version = self.resources.endpoint_resource_version,
            )
            .as_bytes(),
        );
        OperationId::new(format!(
            "fault-{}",
            digest
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        ))
    }

    pub fn validate(&self) -> Result<(), &'static str> {
        self.binding.validate()?;
        if self.action_id.is_empty()
            || self.fault_operation_id.is_empty()
            || self.predecessor_session.is_empty()
            || self.predecessor_process_id == 0
            || self.fault_revision == 0
            || self.resources.pod_name.is_empty()
            || self.resources.pod_uid.is_empty()
            || self.resources.pvc_name.is_empty()
            || self.resources.pvc_uid.is_empty()
            || self.resources.endpoint_name.is_empty()
            || self.resources.endpoint_uid.is_empty()
            || self.resources.endpoint_resource_version.is_empty()
        {
            return Err("incomplete exact fault action identity");
        }
        if self.kind != Self::expected_kind(self.binding.state_persistence, self.fault) {
            return Err("fault action conflicts with persistence classification");
        }
        if self.action_id != self.expected_id() {
            return Err("fault action ID does not match frozen identity");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum RestartActionStage {
    Accepted,
    PredecessorContained,
    SuccessorLaunching,
    SuccessorStarted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum PublicServiceClearStage {
    Pending,
    PublishedAbsent,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PublicServiceClear {
    pub action_id: OperationId,
    pub stage: PublicServiceClearStage,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_uid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_resource_version: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RestartActionRecord {
    pub action: PublicFaultAction,
    pub stage: RestartActionStage,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub successor_session: Option<ProcessSessionId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub successor_process_id: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch_nonce: Option<String>,
}

impl PublicOperationPreviewIdentity {
    pub const fn is_valid(&self) -> bool {
        self.is_legacy_preview() || self.is_public_v1()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", tag = "kind")]
pub enum PublicOperationClass {
    Authority,
    PlannedSwap,
    Build { target: ReplicaId },
    Remove { target: ReplicaId },
    Close,
    Abort,
    TransientFault,
    PermanentFault,
    Restart,
    DropReplacement,
}

impl PublicOperationClass {
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Close
                | Self::Abort
                | Self::TransientFault
                | Self::PermanentFault
                | Self::Restart
                | Self::DropReplacement
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CanonicalOperationInputDigestV1 {
    pub sha256: String,
}

impl CanonicalOperationInputDigestV1 {
    fn from_bytes(bytes: &[u8]) -> Self {
        use sha2::Digest;
        Self {
            sha256: sha2::Sha256::digest(bytes)
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect(),
        }
    }

    fn validate(&self) -> Result<(), &'static str> {
        if self.sha256.len() != 64
            || !self
                .sha256
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        {
            return Err("canonical operation-input digest is not lowercase SHA-256");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", tag = "version", content = "value")]
pub enum CanonicalOperationInputDigest {
    V1(CanonicalOperationInputDigestV1),
}

impl CanonicalOperationInputDigest {
    pub fn opaque_v1(domain: &str, value: &[u8]) -> Self {
        let mut encoder = CanonicalEncoder::new(domain);
        encoder.bytes(value);
        Self::V1(CanonicalOperationInputDigestV1::from_bytes(
            &encoder.finish(),
        ))
    }

    pub const fn version(&self) -> u16 {
        match self {
            Self::V1(_) => 1,
        }
    }

    pub fn sha256(&self) -> &str {
        match self {
            Self::V1(value) => &value.sha256,
        }
    }

    fn validate(&self) -> Result<(), &'static str> {
        match self {
            Self::V1(value) => value.validate(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum DataLossReplay {
    Convergent,
    ReplaceOnAmbiguity,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", tag = "kind", content = "detail")]
pub enum PublicOperationReplayDisposition {
    Convergent,
    RepeatableRead,
    ReevaluateCatchUp,
    RetainBuildSuccessRetireAmbiguity,
    DataLoss(DataLossReplay),
    ExactRemoval,
    Containment,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PublicOperationIntent {
    pub preview: PublicOperationPreviewIdentity,
    pub operation_id: OperationId,
    pub revision: u64,
    pub process_session_id: ProcessSessionId,
    pub class: PublicOperationClass,
    pub input_digest: CanonicalOperationInputDigest,
    pub replay_disposition: PublicOperationReplayDisposition,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lifecycle: Option<PublicLifecycleInput>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub program: Option<PublicOperationProgram>,
}

impl PublicOperationIntent {
    pub fn validate(&self) -> Result<(), &'static str> {
        if !self.preview.is_valid() {
            return Err("invalid public-operation preview identity");
        }
        let public_v1 = self.preview.is_public_v1();
        if self.operation_id.is_empty() {
            return Err("public-operation ID is empty");
        }
        if self.revision == 0 {
            return Err("public-operation revision must be positive");
        }
        if self.process_session_id.is_empty() {
            return Err("public-operation process session is empty");
        }
        self.input_digest.validate()?;
        if let Some(input) = &self.lifecycle {
            if self.program.is_some() {
                return Err("operation cannot carry two programs");
            }
            if self.class != PublicOperationClass::Authority {
                return Err("lifecycle input requires authority operation");
            }
            input.validate_for_preview(public_v1)?;
            input.validate_authorization_revision(self.revision)?;
            if self.input_digest != input.digest() {
                return Err("lifecycle digest does not match frozen input");
            }
            if public_v1 {
                match input.possible_data_loss {
                    PossibleDataLossIntent::NotPossible
                        if self.replay_disposition
                            != PublicOperationReplayDisposition::Convergent =>
                    {
                        return Err("ordinary lifecycle replay disposition changed");
                    }
                    PossibleDataLossIntent::Possible
                        if !matches!(
                            self.replay_disposition,
                            PublicOperationReplayDisposition::DataLoss(_)
                        ) =>
                    {
                        return Err("data-loss lifecycle requires captured replay capability");
                    }
                    _ => {}
                }
                if let Some(recovery) = &input.data_loss_recovery
                    && (recovery.attempt.process.process_session_id != self.process_session_id
                        || recovery.attempt.attempt_id != self.operation_id
                        || recovery.attempt.process.attempt_revision != self.revision
                        || self.replay_disposition
                            != PublicOperationReplayDisposition::DataLoss(
                                recovery.authorization.replay_disposition,
                            ))
                {
                    return Err(
                        "data-loss attempt session or replay differs from captured operation",
                    );
                }
            }
        }
        if let Some(program) = &self.program {
            program.validate(&self.class, &self.operation_id, public_v1)?;
            program.validate_authorization_revision(self.revision)?;
            if self.input_digest != program.digest() {
                return Err("program digest does not match frozen input");
            }
            if public_v1 && self.replay_disposition != program.replay_disposition() {
                return Err("program replay disposition does not match frozen input");
            }
        }
        if !public_v1
            && (self.lifecycle.is_some() || self.program.is_some())
            && self.replay_disposition != PublicOperationReplayDisposition::Convergent
        {
            return Err("protocol-10 operation cannot use public-v1 replay disposition");
        }
        Ok(())
    }
}

/// Captures the caller's selection, not the predicate or value policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum PublicCatchUpMode {
    WriteQuorum,
    All,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PublicConfiguration {
    pub current: ConfigurationDescriptor,
    pub previous: Option<ConfigurationDescriptor>,
    #[serde(default)]
    pub replicas: Vec<PublicReplicaDescriptor>,
}

impl PublicConfiguration {
    pub fn validate(&self) -> Result<(), &'static str> {
        self.validate_for_local(self.current.primary_id)
    }

    fn validate_descriptors(&self) -> Result<(), &'static str> {
        for configuration in std::iter::once(&self.current).chain(self.previous.iter()) {
            if configuration.configuration_id != configuration.expected_id()
                || configuration.write_quorum == 0
                || configuration.write_quorum as usize > configuration.members.len()
                || configuration
                    .members
                    .iter()
                    .filter(|m| m.role == ReplicaRole::Primary)
                    .count()
                    != 1
                || !configuration.members.iter().any(|m| {
                    m.identity.replica_id == configuration.primary_id
                        && m.role == ReplicaRole::Primary
                })
                || configuration
                    .members
                    .iter()
                    .map(|m| &m.identity)
                    .collect::<std::collections::BTreeSet<_>>()
                    .len()
                    != configuration.members.len()
            {
                return Err("invalid exact public configuration");
            }
        }
        Ok(())
    }

    fn validate_for_preview(&self, public_v1: bool) -> Result<(), &'static str> {
        if public_v1 {
            self.validate()
        } else {
            self.validate_descriptors()?;
            if !self.replicas.is_empty() {
                return Err("protocol-10 configuration cannot carry public-v1 descriptors");
            }
            Ok(())
        }
    }

    pub fn validate_for_local(&self, local_replica_id: ReplicaId) -> Result<(), &'static str> {
        self.validate_descriptors()?;
        let mut identities = std::collections::BTreeSet::new();
        let mut successor = None;
        for replica in &self.replicas {
            replica.validate_for_configuration(&self.current, self.previous.as_ref())?;
            if !identities.insert(&replica.identity)
                || replica.identity.replica_id == local_replica_id
                || replica.role != ReplicaRole::ActiveSecondary
                || replica.build_id != OperationId::default()
                || !self.current.members.iter().any(|member| {
                    member.identity == replica.identity
                        && member.role == ReplicaRole::ActiveSecondary
                })
            {
                return Err(
                    "public configuration contains a local, idle, build, or duplicate peer",
                );
            }
            if replica.must_catchup && successor.replace(&replica.identity).is_some() {
                return Err("public configuration contains multiple must-catchup successors");
            }
        }
        let expected_ready_secondaries = self
            .current
            .members
            .iter()
            .filter(|member| {
                member.role == ReplicaRole::ActiveSecondary
                    && member.identity.replica_id != local_replica_id
            })
            .count();
        if self.replicas.len() != expected_ready_secondaries {
            return Err("public configuration lacks an exact ready remote-secondary descriptor");
        }
        Ok(())
    }

    pub fn successor(&self) -> Option<&ReplicaIdentity> {
        self.replicas
            .iter()
            .find(|replica| replica.must_catchup)
            .map(|replica| &replica.identity)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PublicReplicaDescriptor {
    pub build_id: OperationId,
    pub identity: ReplicaIdentity,
    pub replication_address: String,
    pub process_session_id: ProcessSessionId,
    pub role: ReplicaRole,
    pub current_progress: i64,
    pub catch_up_capability: i64,
    pub must_catchup: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peer_authorization: Option<PeerSessionAuthorizationV1>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub configuration_revision: Option<u64>,
}

impl PublicReplicaDescriptor {
    pub fn validate(&self) -> Result<(), &'static str> {
        validate_exact_replica_identity(&self.identity)?;
        if self.replication_address.is_empty() || self.process_session_id.is_empty() {
            return Err("public replica descriptor lacks an exact address or process session");
        }
        validate_progress_value(self.current_progress)?;
        validate_progress_value(self.catch_up_capability)?;
        if self.role == ReplicaRole::IdleSecondary
            && (self.current_progress != INVALID_LSN
                || self.catch_up_capability != INVALID_LSN
                || self.must_catchup)
        {
            return Err("idle public build descriptor must use invalid progress");
        }
        match (&self.peer_authorization, self.configuration_revision) {
            (Some(authorization), Some(revision)) => {
                authorization
                    .claims
                    .validate()
                    .map_err(|_| "invalid peer-session authorization claims")?;
                if revision == 0
                    || authorization.claims.configuration_revision != revision
                    || authorization.claims.secondary.replica != self.identity
                    || authorization.claims.secondary.process_session_id != self.process_session_id
                    || authorization.claims.secondary.replication_address
                        != self.replication_address
                    || authorization.claims.secondary.role != self.role
                    || self.role != ReplicaRole::ActiveSecondary
                    || !self.build_id.is_empty()
                {
                    return Err("peer authorization differs from the active remote descriptor");
                }
            }
            (None, None) => {}
            _ => {
                return Err(
                    "peer authorization and configuration revision must be present together",
                );
            }
        }
        Ok(())
    }

    fn validate_for_configuration(
        &self,
        current: &ConfigurationDescriptor,
        previous: Option<&ConfigurationDescriptor>,
    ) -> Result<(), &'static str> {
        self.validate()?;
        let Some(authorization) = &self.peer_authorization else {
            return Ok(());
        };
        let primary = current
            .members
            .iter()
            .find(|member| {
                member.identity.replica_id == current.primary_id
                    && member.role == ReplicaRole::Primary
            })
            .ok_or("public configuration lacks its exact primary")?;
        if authorization.claims.epoch != current.epoch
            || authorization.claims.current_configuration_id != current.configuration_id
            || authorization.claims.previous_configuration_id
                != previous.map(|configuration| configuration.configuration_id.clone())
            || authorization.claims.primary.replica != primary.identity
            || authorization.claims.primary.role != ReplicaRole::Primary
        {
            return Err("peer authorization differs from the exact PC/CC descriptor");
        }
        Ok(())
    }
}

fn validate_exact_replica_identity(identity: &ReplicaIdentity) -> Result<(), &'static str> {
    if identity.replica_id.value() <= 0
        || identity.instance_id.is_empty()
        || identity.agent_generation.is_empty()
    {
        return Err("replica identity is incomplete");
    }
    Ok(())
}

pub fn validate_progress_value(progress: i64) -> Result<(), &'static str> {
    if progress < -1 {
        Err("public progress is below INVALID_LSN")
    } else {
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct AuthorizationAuditDigest(pub String);

impl AuthorizationAuditDigest {
    pub fn validate(&self) -> Result<(), &'static str> {
        CanonicalOperationInputDigestV1 {
            sha256: self.0.clone(),
        }
        .validate()
        .map_err(|_| "authorization audit digest is not lowercase SHA-256")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PublicBuildAuthorizationReference {
    pub resource_uid: ResourceUid,
    pub epoch: Epoch,
    pub build_id: OperationId,
    pub source: ReplicaIdentity,
    pub source_storage: PvcUid,
    pub source_session: ProcessSessionId,
    pub target: ReplicaIdentity,
    pub target_storage: PvcUid,
    pub target_session: ProcessSessionId,
    pub audit_digest: AuthorizationAuditDigest,
}

impl PublicBuildAuthorizationReference {
    pub(crate) fn validate(&self) -> Result<(), &'static str> {
        if self.resource_uid.is_empty()
            || self.build_id.is_empty()
            || self.source_storage.is_empty()
            || self.target_storage.is_empty()
            || self.source_session.is_empty()
            || self.target_session.is_empty()
        {
            return Err("public build authorization reference is incomplete");
        }
        if self.epoch.data_loss_number < 0 || self.epoch.configuration_number < 0 {
            return Err("public build authorization epoch is invalid");
        }
        validate_exact_replica_identity(&self.source)?;
        validate_exact_replica_identity(&self.target)?;
        self.audit_digest.validate()
    }
}

fn validate_signed_build_reference(
    reference: &PublicBuildAuthorizationReference,
    authorization: &BuildAuthorizationV1,
) -> Result<(), &'static str> {
    let claims = &authorization.claims;
    let ReplicaStorageIdentity::Persistent {
        pvc_uid: source_storage,
        ..
    } = &claims.source.storage_incarnation.storage
    else {
        return Err("signed build source storage is not persistent");
    };
    let ReplicaStorageIdentity::Persistent {
        pvc_uid: target_storage,
        ..
    } = &claims.target.storage_incarnation.storage
    else {
        return Err("signed build target storage is not persistent");
    };
    let audit_digest = authorization
        .audit_digest()
        .map_err(|_| "invalid signed build authorization envelope")?
        .to_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    if reference.resource_uid != claims.resource_uid
        || reference.epoch != claims.epoch
        || reference.build_id != claims.build_id
        || reference.source != claims.source.storage_incarnation.replica
        || reference.source_storage != *source_storage
        || reference.source_session != claims.source.process_session_id
        || reference.target != claims.target.storage_incarnation.replica
        || reference.target_storage != *target_storage
        || reference.target_session != claims.target.process_session_id
        || reference.audit_digest.0 != audit_digest
    {
        return Err("public build reference differs from the signed authorization");
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PublicBuildInput {
    pub attempt: OperationId,
    pub replica: ReplicaIdentity,
    pub process_session_id: ProcessSessionId,
    pub replication_address: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub history: Option<HistoryContext>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authorization: Option<PublicBuildAuthorizationReference>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authorization_verification_key: Option<AuthorizationVerificationKey>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build_authorization: Option<BuildAuthorizationV1>,
}

impl PublicBuildInput {
    #[cfg(any(test, feature = "testing"))]
    pub(crate) fn same_attempt_as(&self, other: &Self) -> bool {
        self.attempt == other.attempt
            && self.replica == other.replica
            && self.process_session_id == other.process_session_id
            && self.replication_address == other.replication_address
            && self.history == other.history
            && self.authorization == other.authorization
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum PublicOperationProgram {
    Open {
        replica: ReplicaIdentity,
        existing: bool,
    },
    Role {
        epoch: Epoch,
        role: ReplicaRole,
    },
    Epoch {
        epoch: Epoch,
    },
    Configuration(PublicConfiguration),
    CatchUp {
        configuration: PublicConfiguration,
        mode: PublicCatchUpMode,
    },
    Progress {
        capability: bool,
    },
    Swap {
        starting: PublicConfiguration,
        refreshed: PublicConfiguration,
        epoch: Epoch,
        handoff: ReplicaRole,
        catch_up_specific_quorum: bool,
        mode: PublicCatchUpMode,
    },
    Build(PublicBuildInput),
    Remove(PublicBuildInput),
    Close,
    Abort,
}

impl PublicOperationProgram {
    pub fn digest(&self) -> CanonicalOperationInputDigest {
        let mut encoder = CanonicalEncoder::new("kuberic-public-operation-input-v1");
        self.encode(&mut encoder);
        CanonicalOperationInputDigest::V1(CanonicalOperationInputDigestV1::from_bytes(
            &encoder.finish(),
        ))
    }

    pub const fn replay_disposition(&self) -> PublicOperationReplayDisposition {
        match self {
            Self::Progress { .. } => PublicOperationReplayDisposition::RepeatableRead,
            Self::CatchUp { .. } => PublicOperationReplayDisposition::ReevaluateCatchUp,
            Self::Build(_) => PublicOperationReplayDisposition::RetainBuildSuccessRetireAmbiguity,
            Self::Remove(_) => PublicOperationReplayDisposition::ExactRemoval,
            Self::Close | Self::Abort => PublicOperationReplayDisposition::Containment,
            Self::Open { .. }
            | Self::Role { .. }
            | Self::Epoch { .. }
            | Self::Configuration(_)
            | Self::Swap { .. } => PublicOperationReplayDisposition::Convergent,
        }
    }

    #[cfg(any(test, feature = "testing"))]
    pub(crate) fn validate_history(
        &self,
        expected_history: &HistoryContext,
    ) -> Result<(), &'static str> {
        let expected_data_loss_number = expected_history.data_loss_number;
        let matches = |epoch: Epoch| epoch.data_loss_number == expected_data_loss_number;
        let configuration_matches = |configuration: &PublicConfiguration| {
            matches(configuration.current.epoch)
                && configuration
                    .previous
                    .as_ref()
                    .is_none_or(|previous| matches(previous.epoch))
        };
        let valid = match self {
            Self::Role { epoch, .. } | Self::Epoch { epoch } => matches(*epoch),
            Self::Configuration(configuration) | Self::CatchUp { configuration, .. } => {
                configuration_matches(configuration)
            }
            Self::Swap {
                starting,
                refreshed,
                epoch,
                ..
            } => {
                matches(*epoch)
                    && configuration_matches(starting)
                    && configuration_matches(refreshed)
            }
            Self::Build(build) | Self::Remove(build) => {
                build.history.as_ref() == Some(expected_history)
                    && build
                        .authorization
                        .as_ref()
                        .is_none_or(|authorization| matches(authorization.epoch))
            }
            Self::Open { .. } | Self::Progress { .. } | Self::Close | Self::Abort => true,
        };
        if valid {
            Ok(())
        } else {
            Err("ordinary public-v1 operation changed the history data-loss number")
        }
    }

    fn validate(
        &self,
        class: &PublicOperationClass,
        id: &OperationId,
        public_v1: bool,
    ) -> Result<(), &'static str> {
        let expected = match self {
            Self::Swap {
                starting,
                refreshed,
                epoch,
                handoff,
                catch_up_specific_quorum,
                mode,
            } => {
                starting.validate_for_preview(public_v1)?;
                refreshed.validate_for_preview(public_v1)?;
                if starting.current.epoch >= *epoch
                    || refreshed.current.epoch != *epoch
                    || !matches!(handoff, ReplicaRole::ActiveSecondary | ReplicaRole::None)
                {
                    return Err("invalid swap epoch or handoff");
                }
                if public_v1 {
                    let expected_mode = if *catch_up_specific_quorum {
                        PublicCatchUpMode::WriteQuorum
                    } else {
                        PublicCatchUpMode::All
                    };
                    if *mode != expected_mode || starting.successor() != refreshed.successor() {
                        return Err("swap capability, successor, or catch-up mode changed");
                    }
                    if *catch_up_specific_quorum && starting.successor().is_none() {
                        return Err("specific-quorum swap lacks a must-catchup successor");
                    }
                } else if *catch_up_specific_quorum {
                    return Err("protocol-10 swap cannot carry public-v1 capability metadata");
                }
                PublicOperationClass::PlannedSwap
            }
            Self::Configuration(configuration) | Self::CatchUp { configuration, .. } => {
                configuration.validate_for_preview(public_v1)?;
                PublicOperationClass::Authority
            }
            Self::Build(build) | Self::Remove(build) => {
                if build.attempt.is_empty()
                    || build.process_session_id.is_empty()
                    || build.replication_address.is_empty()
                {
                    return Err("incomplete exact build identity");
                }
                if !public_v1 && build.authorization.is_some() {
                    return Err("protocol-10 build cannot carry public-v1 authorization");
                }
                if !public_v1 && build.build_authorization.is_some() {
                    return Err("protocol-10 build cannot carry signed authorization");
                }
                if !public_v1 && build.authorization_verification_key.is_some() {
                    return Err("protocol-10 build cannot carry authorization trust");
                }
                if public_v1 {
                    let history = build
                        .history
                        .as_ref()
                        .ok_or("public-v1 build lacks exact history")?;
                    history.validate()?;
                } else if build.history.is_some() {
                    return Err("protocol-10 build cannot carry public-v1 history");
                }
                if let Some(authorization) = &build.authorization {
                    authorization.validate()?;
                    if authorization.build_id != build.attempt
                        || authorization.target != build.replica
                        || authorization.target_session != build.process_session_id
                    {
                        return Err("build input differs from captured authorization claims");
                    }
                }
                if let Some(authorization) = &build.build_authorization {
                    let verification_key = build
                        .authorization_verification_key
                        .as_ref()
                        .ok_or("signed build authorization lacks its verification key")?;
                    authorization
                        .claims
                        .validate()
                        .map_err(|_| "invalid signed build authorization claims")?;
                    verification_key
                        .verify_build_authorization(authorization)
                        .map_err(|_| "signed build authorization signature is invalid")?;
                    if matches!(self, Self::Remove(_)) {
                        return Err("remove operation cannot carry signed build authorization");
                    }
                    if authorization.claims.operation_id != *id
                        || authorization.claims.build_id != build.attempt
                        || authorization.claims.target.storage_incarnation.replica != build.replica
                        || authorization.claims.target.process_session_id
                            != build.process_session_id
                        || authorization.claims.target.replication_address
                            != build.replication_address
                        || authorization.claims.target.role != ReplicaRole::IdleSecondary
                        || build.history.as_ref() != Some(&authorization.claims.history)
                    {
                        return Err(
                            "signed build authorization differs from the idle target input",
                        );
                    }
                    if let Some(reference) = &build.authorization {
                        validate_signed_build_reference(reference, authorization)?;
                    }
                } else if build.authorization_verification_key.is_some() {
                    return Err("authorization verification key lacks a signed build envelope");
                }
                if matches!(self, Self::Build(_)) {
                    if &build.attempt != id {
                        return Err("build operation is not its exact attempt");
                    }
                    PublicOperationClass::Build {
                        target: build.replica.replica_id,
                    }
                } else {
                    PublicOperationClass::Remove {
                        target: build.replica.replica_id,
                    }
                }
            }
            Self::Close => PublicOperationClass::Close,
            Self::Abort => PublicOperationClass::Abort,
            _ => PublicOperationClass::Authority,
        };
        if *class != expected {
            return Err("program does not match operation class");
        }
        Ok(())
    }

    fn validate_authorization_revision(&self, revision: u64) -> Result<(), &'static str> {
        let validate = |configuration: &PublicConfiguration| {
            configuration.replicas.iter().try_for_each(|replica| {
                if replica.peer_authorization.is_some()
                    && replica.configuration_revision != Some(revision)
                {
                    Err("peer authorization revision differs from the operation revision")
                } else {
                    Ok(())
                }
            })
        };
        match self {
            Self::Configuration(configuration) | Self::CatchUp { configuration, .. } => {
                validate(configuration)
            }
            Self::Swap {
                starting,
                refreshed,
                ..
            } => {
                validate(starting)?;
                validate(refreshed)
            }
            _ => Ok(()),
        }
    }

    fn encode(&self, encoder: &mut CanonicalEncoder) {
        match self {
            Self::Open { replica, existing } => {
                encoder.u8(1);
                encoder.replica_identity(replica);
                encoder.bool(*existing);
            }
            Self::Role { epoch, role } => {
                encoder.u8(2);
                encoder.epoch(*epoch);
                encoder.replica_role(*role);
            }
            Self::Epoch { epoch } => {
                encoder.u8(3);
                encoder.epoch(*epoch);
            }
            Self::Configuration(configuration) => {
                encoder.u8(4);
                encoder.public_configuration(configuration);
            }
            Self::CatchUp {
                configuration,
                mode,
            } => {
                encoder.u8(5);
                encoder.public_configuration(configuration);
                encoder.catch_up_mode(*mode);
            }
            Self::Progress { capability } => {
                encoder.u8(6);
                encoder.bool(*capability);
            }
            Self::Swap {
                starting,
                refreshed,
                epoch,
                handoff,
                catch_up_specific_quorum,
                mode,
            } => {
                encoder.u8(7);
                encoder.public_configuration(starting);
                encoder.public_configuration(refreshed);
                encoder.epoch(*epoch);
                encoder.replica_role(*handoff);
                encoder.bool(*catch_up_specific_quorum);
                encoder.catch_up_mode(*mode);
            }
            Self::Build(build) => {
                encoder.u8(8);
                encoder.public_build(build);
            }
            Self::Remove(build) => {
                encoder.u8(9);
                encoder.public_build(build);
            }
            Self::Close => encoder.u8(10),
            Self::Abort => encoder.u8(11),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum PossibleDataLossIntent {
    NotPossible,
    Possible,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum PublicLifecycleRecipe {
    InitialPrimary,
    FailoverPromotion,
    SecondaryEpochAdvance,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", tag = "kind")]
pub enum ProviderProgressReset {
    Preserve,
    ResetToEmpty,
    ReplaceIncarnation {
        retired: ReplicaStorageIncarnationContext,
    },
}

#[allow(dead_code)]
#[derive(JsonSchema)]
#[serde(rename_all = "camelCase")]
struct ProviderProgressResetSchema {
    kind: String,
    retired: Option<ReplicaStorageIncarnationContext>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DataLossAuthorization {
    pub authorization_id: OperationId,
    pub predecessor: HistoryContext,
    pub proposed: HistoryContext,
    pub provisional_primary: ReplicaStorageIncarnationContext,
    pub reset_scope: ProviderProgressReset,
    pub replay_disposition: DataLossReplay,
}

impl DataLossAuthorization {
    pub fn validate(&self) -> Result<(), &'static str> {
        self.predecessor.validate()?;
        self.proposed.validate()?;
        self.provisional_primary.validate()?;
        if self.authorization_id.is_empty()
            || self.predecessor.resource_uid != self.proposed.resource_uid
            || self.predecessor.protocol_generation != self.proposed.protocol_generation
            || self.proposed
                != derive_data_loss_history(
                    &self.authorization_id,
                    &self.predecessor,
                    self.proposed.data_loss_number,
                    &self.provisional_primary,
                )?
        {
            return Err("data-loss authorization does not name the exact successor history");
        }
        if let ProviderProgressReset::ReplaceIncarnation { retired } = &self.reset_scope {
            retired.validate()?;
            if retired == &self.provisional_primary {
                return Err("data-loss replacement scope cannot retire its provisional primary");
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProcessAttemptContext {
    pub process_session_id: ProcessSessionId,
    pub replicator_creation_id: ReplicatorCreationId,
    pub catch_up_specific_quorum: bool,
    pub data_loss_replay: DataLossReplay,
    pub epoch: Epoch,
    pub attempt_revision: u64,
}

impl ProcessAttemptContext {
    fn validate(&self) -> Result<(), &'static str> {
        if self.process_session_id.is_empty()
            || self.replicator_creation_id.is_empty()
            || self.epoch.data_loss_number < 0
            || self.epoch.configuration_number < 0
            || self.attempt_revision == 0
        {
            return Err("process attempt context is incomplete");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DataLossAttempt {
    pub authorization_id: OperationId,
    pub attempt_id: OperationId,
    pub process: ProcessAttemptContext,
}

impl DataLossAttempt {
    pub fn validate_for(
        &self,
        authorization: &DataLossAuthorization,
        minimum_configuration_number: i64,
    ) -> Result<(), &'static str> {
        self.process.validate()?;
        if self.authorization_id != authorization.authorization_id
            || self.attempt_id.is_empty()
            || self.process.epoch.data_loss_number != authorization.proposed.data_loss_number
            || self.process.epoch.configuration_number <= minimum_configuration_number
        {
            return Err("data-loss attempt differs from its logical authorization");
        }
        Ok(())
    }

    pub fn renewal_decision(
        authorization: &DataLossAuthorization,
        predecessor: &Self,
        successor: &Self,
        predecessor_contained: bool,
    ) -> DataLossRenewalDecision {
        if !predecessor_contained
            || predecessor.authorization_id != authorization.authorization_id
            || successor.authorization_id != authorization.authorization_id
            || successor.attempt_id == predecessor.attempt_id
            || successor.process.process_session_id == predecessor.process.process_session_id
            || successor.process.replicator_creation_id
                == predecessor.process.replicator_creation_id
            || successor.process.epoch.data_loss_number != authorization.proposed.data_loss_number
            || successor.process.epoch.configuration_number
                <= predecessor.process.epoch.configuration_number
        {
            return DataLossRenewalDecision::Terminal;
        }
        match authorization.replay_disposition {
            DataLossReplay::Convergent
                if successor.process.data_loss_replay == DataLossReplay::Convergent =>
            {
                DataLossRenewalDecision::InvokeCallback
            }
            DataLossReplay::Convergent => DataLossRenewalDecision::RetireWithoutCallback,
            DataLossReplay::ReplaceOnAmbiguity => DataLossRenewalDecision::Terminal,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum DataLossRenewalDecision {
    InvokeCallback,
    RetireWithoutCallback,
    Terminal,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PublicDataLossSessionRenewal {
    pub authorization: DataLossAuthorization,
    pub predecessor_attempt: DataLossAttempt,
    pub replacement_attempt: DataLossAttempt,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProcessSessionRenewalRequired {
    pub operation_id: OperationId,
    pub reason: ProcessSessionRenewalReason,
    pub resource_uid: ResourceUid,
    pub history: HistoryContext,
    pub previous_epoch: Epoch,
    pub renewal_epoch: Epoch,
    pub predecessor: ProcessSessionEndpoint,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replacement: Option<ProcessSessionEndpoint>,
    #[serde(default)]
    pub surviving_sessions: Vec<ProcessSessionEndpoint>,
    pub fresh_configuration: ConfigurationDescriptor,
    pub predecessor_contained: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_loss: Option<PublicDataLossSessionRenewal>,
}

impl ProcessSessionRenewalRequired {
    pub fn validate(&self) -> Result<(), &'static str> {
        self.history.validate()?;
        self.predecessor.validate()?;
        if self.operation_id.is_empty()
            || self.resource_uid.is_empty()
            || self.history.resource_uid != self.resource_uid
            || self.previous_epoch.data_loss_number < 0
            || self.previous_epoch.configuration_number < 0
            || self.renewal_epoch.data_loss_number != self.previous_epoch.data_loss_number
            || self.renewal_epoch.configuration_number <= self.previous_epoch.configuration_number
            || self.fresh_configuration.epoch != self.renewal_epoch
            || self.fresh_configuration.configuration_id != self.fresh_configuration.expected_id()
            || self.fresh_configuration.write_quorum == 0
            || self.fresh_configuration.write_quorum as usize
                > self.fresh_configuration.members.len()
            || !self.predecessor_contained
        {
            return Err("process-session renewal is incomplete or does not advance its epoch");
        }
        let mut configuration_members = std::collections::BTreeSet::new();
        if self.fresh_configuration.members.iter().any(|member| {
            !configuration_members.insert(member.identity.replica_id)
                || !matches!(
                    member.role,
                    ReplicaRole::Primary | ReplicaRole::ActiveSecondary
                )
        }) || self
            .fresh_configuration
            .members
            .iter()
            .filter(|member| member.role == ReplicaRole::Primary)
            .count()
            != 1
            || !self.fresh_configuration.members.iter().any(|member| {
                member.role == ReplicaRole::Primary
                    && member.identity.replica_id == self.fresh_configuration.primary_id
            })
        {
            return Err("fresh session-renewal configuration is invalid");
        }
        if !matches!(
            self.predecessor.role,
            ReplicaRole::Primary | ReplicaRole::ActiveSecondary
        ) {
            return Err("process-session renewal predecessor is not active");
        }
        match self.reason {
            ProcessSessionRenewalReason::PrimaryReplacement
                if self.predecessor.role != ReplicaRole::Primary =>
            {
                return Err("primary renewal does not replace a primary");
            }
            ProcessSessionRenewalReason::ActiveSecondaryReplacement
                if self.predecessor.role != ReplicaRole::ActiveSecondary =>
            {
                return Err("secondary renewal does not replace an active secondary");
            }
            ProcessSessionRenewalReason::DataLossAttempt
                if self.predecessor.role != ReplicaRole::Primary =>
            {
                return Err("data-loss renewal does not replace a primary");
            }
            _ => {}
        }

        let replacement_required =
            self.reason != ProcessSessionRenewalReason::PeerAuthorizationWithdrawal;
        if self.replacement.is_some() != replacement_required {
            return Err("process-session renewal replacement shape differs from its reason");
        }
        if let Some(replacement) = &self.replacement {
            replacement.validate()?;
            if replacement.identity != self.predecessor.identity
                || replacement.storage_incarnation != self.predecessor.storage_incarnation
                || replacement.process_session_id == self.predecessor.process_session_id
                || replacement.replicator_creation_id == self.predecessor.replicator_creation_id
                || replacement.role != ReplicaRole::None
            {
                return Err(
                    "replacement session must be fresh, unassigned, and on the same incarnation",
                );
            }
        }

        let mut survivor_ids = std::collections::BTreeSet::new();
        for survivor in &self.surviving_sessions {
            survivor.validate()?;
            if !matches!(
                survivor.role,
                ReplicaRole::Primary | ReplicaRole::ActiveSecondary
            ) || survivor.identity.replica_id == self.predecessor.identity.replica_id
                || !survivor_ids.insert(survivor.identity.replica_id)
            {
                return Err("process-session renewal surviving set is invalid");
            }
        }
        for member in &self.fresh_configuration.members {
            if member.identity.replica_id == self.predecessor.identity.replica_id {
                if replacement_required
                    && (member.identity != self.predecessor.identity
                        || member.role != self.predecessor.role)
                {
                    return Err("fresh configuration changed the replacement member");
                }
                if !replacement_required {
                    return Err("withdrawn peer remains in the fresh configuration");
                }
                continue;
            }
            if !self.surviving_sessions.iter().any(|survivor| {
                survivor.identity == member.identity && survivor.role == member.role
            }) {
                return Err("fresh configuration contains an unfrozen survivor");
            }
        }

        match (&self.reason, &self.data_loss) {
            (
                ProcessSessionRenewalReason::DataLossAttempt,
                Some(PublicDataLossSessionRenewal {
                    authorization,
                    predecessor_attempt,
                    replacement_attempt,
                }),
            ) => {
                authorization.validate()?;
                predecessor_attempt.validate_for(
                    authorization,
                    self.previous_epoch.configuration_number.saturating_sub(1),
                )?;
                replacement_attempt
                    .validate_for(authorization, self.previous_epoch.configuration_number)?;
                let replacement = self
                    .replacement
                    .as_ref()
                    .expect("validated replacement shape");
                if self.history != authorization.predecessor
                    || self.predecessor.storage_incarnation != authorization.provisional_primary
                    || predecessor_attempt.process.process_session_id
                        != self.predecessor.process_session_id
                    || predecessor_attempt.process.replicator_creation_id
                        != self.predecessor.replicator_creation_id
                    || predecessor_attempt.process.epoch != self.previous_epoch
                    || replacement_attempt.process.process_session_id
                        != replacement.process_session_id
                    || replacement_attempt.process.replicator_creation_id
                        != replacement.replicator_creation_id
                    || replacement_attempt.process.epoch != self.renewal_epoch
                    || DataLossAttempt::renewal_decision(
                        authorization,
                        predecessor_attempt,
                        replacement_attempt,
                        self.predecessor_contained,
                    ) == DataLossRenewalDecision::Terminal
                {
                    return Err("data-loss session renewal changed its captured authority");
                }
            }
            (ProcessSessionRenewalReason::DataLossAttempt, None) => {
                return Err("data-loss session renewal lacks attempt authority");
            }
            (_, Some(_)) => {
                return Err("ordinary session renewal carries data-loss authority");
            }
            (_, None) if self.previous_epoch.data_loss_number != self.history.data_loss_number => {
                return Err("ordinary session renewal changed history scope");
            }
            (_, None) => {}
        }
        Ok(())
    }

    pub fn validate_replacement_report(
        &self,
        report: &PublicV1AgentReport,
    ) -> Result<(), &'static str> {
        self.validate()?;
        let replacement = self
            .replacement
            .as_ref()
            .ok_or("peer withdrawal has no replacement session")?;
        report.history.validate()?;
        report.storage_incarnation.validate()?;
        report
            .progress
            .validate(report.role, report.read_status, report.write_status)?;
        if !report.identity.is_public_v1()
            || report.resource_uid != self.resource_uid
            || report.report_sequence == 0
            || report.replica != replacement.identity
            || report.storage_incarnation != replacement.storage_incarnation
            || report.process_session_id != replacement.process_session_id
            || report.history != self.history
            || report.role != ReplicaRole::None
            || report.read_status == AccessStatus::Granted
            || report.write_status == AccessStatus::Granted
            || report.progress.role != PublicProgressRole::Recovery
            || report.data_loss_recovery.is_some()
        {
            return Err(
                "replacement session must begin unassigned, access-closed, and history-scoped",
            );
        }
        Ok(())
    }

    pub fn data_loss_decision(&self) -> Option<DataLossRenewalDecision> {
        self.data_loss.as_ref().map(|renewal| {
            DataLossAttempt::renewal_decision(
                &renewal.authorization,
                &renewal.predecessor_attempt,
                &renewal.replacement_attempt,
                self.predecessor_contained,
            )
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum PublicSessionRenewalStage {
    Required,
    AccessClosed,
    SurvivingSetFrozen,
    EpochBarriersPending,
    ReplacementRolePending,
    ConfigurationPending,
    Authorized,
    Retired,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum PublicSessionEpochBarrierStage {
    Pending,
    Updated,
    Excluded,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PublicSessionEpochBarrier {
    pub survivor: ProcessSessionEndpoint,
    pub epoch: Epoch,
    pub stage: PublicSessionEpochBarrierStage,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retirement_operation_id: Option<OperationId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PublicV1SessionRenewalStatus {
    pub required: ProcessSessionRenewalRequired,
    pub stage: PublicSessionRenewalStage,
    #[serde(default)]
    pub barriers: Vec<PublicSessionEpochBarrier>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replayed_role: Option<ReplicaRole>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub installed_configuration: Option<ConfigurationDescriptor>,
}

impl PublicV1SessionRenewalStatus {
    pub fn required(required: ProcessSessionRenewalRequired) -> Result<Self, &'static str> {
        required.validate()?;
        Ok(Self {
            required,
            stage: PublicSessionRenewalStage::Required,
            barriers: Vec::new(),
            replayed_role: None,
            installed_configuration: None,
        })
    }

    pub fn validate(&self) -> Result<(), &'static str> {
        self.required.validate()?;
        let mut survivor_ids = std::collections::BTreeSet::new();
        for barrier in &self.barriers {
            if barrier.epoch != self.required.renewal_epoch
                || !self
                    .required
                    .surviving_sessions
                    .iter()
                    .any(|survivor| survivor == &barrier.survivor)
                || !survivor_ids.insert(barrier.survivor.identity.replica_id)
                || (barrier.stage == PublicSessionEpochBarrierStage::Excluded)
                    != barrier.retirement_operation_id.is_some()
            {
                return Err("session-renewal epoch barrier is inconsistent");
            }
        }
        let barriers_frozen = self.barriers.len() == self.required.surviving_sessions.len();
        let barriers_resolved = barriers_frozen
            && self
                .barriers
                .iter()
                .all(|barrier| barrier.stage != PublicSessionEpochBarrierStage::Pending);
        match self.stage {
            PublicSessionRenewalStage::Required | PublicSessionRenewalStage::AccessClosed
                if !self.barriers.is_empty()
                    || self.replayed_role.is_some()
                    || self.installed_configuration.is_some() =>
            {
                return Err("early session-renewal stage contains later authority");
            }
            PublicSessionRenewalStage::SurvivingSetFrozen
            | PublicSessionRenewalStage::EpochBarriersPending
                if !barriers_frozen
                    || self.replayed_role.is_some()
                    || self.installed_configuration.is_some() =>
            {
                return Err("session-renewal surviving set is not exactly frozen");
            }
            PublicSessionRenewalStage::ReplacementRolePending
                if !barriers_resolved
                    || self.required.replacement.is_none()
                    || self.replayed_role.is_some()
                    || self.installed_configuration.is_some() =>
            {
                return Err("replacement role is not fenced by exact survivor barriers");
            }
            PublicSessionRenewalStage::ConfigurationPending
                if !barriers_resolved
                    || (self.required.replacement.is_some()
                        && self.replayed_role != Some(self.required.predecessor.role))
                    || self.installed_configuration.is_some() =>
            {
                return Err("fresh configuration is not fenced by replacement-role replay");
            }
            PublicSessionRenewalStage::Authorized
                if !barriers_resolved
                    || (self.required.replacement.is_some()
                        && self.replayed_role != Some(self.required.predecessor.role))
                    || self.installed_configuration.as_ref()
                        != Some(&self.required.fresh_configuration) =>
            {
                return Err("authorized session renewal differs from fresh configuration");
            }
            PublicSessionRenewalStage::Retired
                if self.required.data_loss_decision()
                    != Some(DataLossRenewalDecision::RetireWithoutCallback)
                    || !barriers_resolved
                    || self.replayed_role.is_some()
                    || self.installed_configuration.is_some() =>
            {
                return Err("retired session renewal is not the captured replay disposition");
            }
            _ => {}
        }
        if barriers_resolved
            && matches!(
                self.stage,
                PublicSessionRenewalStage::ReplacementRolePending
                    | PublicSessionRenewalStage::ConfigurationPending
                    | PublicSessionRenewalStage::Authorized
            )
        {
            for barrier in &self.barriers {
                let participates = self
                    .required
                    .fresh_configuration
                    .members
                    .iter()
                    .any(|member| member.identity == barrier.survivor.identity);
                if (participates && barrier.stage != PublicSessionEpochBarrierStage::Updated)
                    || (!participates && barrier.stage != PublicSessionEpochBarrierStage::Excluded)
                {
                    return Err("survivor barrier does not match fresh configuration membership");
                }
            }
        }
        Ok(())
    }

    pub fn close_access(&self) -> Result<Self, &'static str> {
        if self.stage != PublicSessionRenewalStage::Required {
            return Err("session-renewal access closure is out of order");
        }
        let mut next = self.clone();
        next.stage = PublicSessionRenewalStage::AccessClosed;
        next.validate()?;
        Ok(next)
    }

    pub fn freeze_surviving_set(&self) -> Result<Self, &'static str> {
        if self.stage != PublicSessionRenewalStage::AccessClosed {
            return Err("session-renewal surviving-set freeze is out of order");
        }
        let mut next = self.clone();
        next.barriers = self
            .required
            .surviving_sessions
            .iter()
            .cloned()
            .map(|survivor| PublicSessionEpochBarrier {
                survivor,
                epoch: self.required.renewal_epoch,
                stage: PublicSessionEpochBarrierStage::Pending,
                retirement_operation_id: None,
            })
            .collect();
        next.stage = PublicSessionRenewalStage::SurvivingSetFrozen;
        next.validate()?;
        Ok(next)
    }

    pub fn begin_epoch_barriers(&self) -> Result<Self, &'static str> {
        if self.stage != PublicSessionRenewalStage::SurvivingSetFrozen {
            return Err("session-renewal epoch barriers are out of order");
        }
        let mut next = self.clone();
        next.stage = PublicSessionRenewalStage::EpochBarriersPending;
        next.validate()?;
        Ok(next)
    }

    pub fn record_epoch_updated(
        &self,
        survivor: &ProcessSessionEndpoint,
        epoch: Epoch,
    ) -> Result<Self, &'static str> {
        if self.stage != PublicSessionRenewalStage::EpochBarriersPending
            || epoch != self.required.renewal_epoch
        {
            return Err("session-renewal epoch observation is stale");
        }
        let mut next = self.clone();
        let barrier = next
            .barriers
            .iter_mut()
            .find(|barrier| &barrier.survivor == survivor)
            .ok_or("session-renewal epoch observation names an unfrozen survivor")?;
        if barrier.stage != PublicSessionEpochBarrierStage::Pending {
            return Err("session-renewal epoch barrier was already resolved");
        }
        barrier.stage = PublicSessionEpochBarrierStage::Updated;
        next.validate()?;
        Ok(next)
    }

    pub fn exclude_survivor(
        &self,
        survivor: &ProcessSessionEndpoint,
        retirement_operation_id: OperationId,
    ) -> Result<Self, &'static str> {
        if self.stage != PublicSessionRenewalStage::EpochBarriersPending
            || retirement_operation_id.is_empty()
        {
            return Err("session-renewal survivor exclusion is stale or incomplete");
        }
        if self
            .required
            .fresh_configuration
            .members
            .iter()
            .any(|member| member.identity == survivor.identity)
        {
            return Err("participating survivor cannot be excluded from its epoch barrier");
        }
        let mut next = self.clone();
        let barrier = next
            .barriers
            .iter_mut()
            .find(|barrier| &barrier.survivor == survivor)
            .ok_or("session-renewal exclusion names an unfrozen survivor")?;
        if barrier.stage != PublicSessionEpochBarrierStage::Pending {
            return Err("session-renewal epoch barrier was already resolved");
        }
        barrier.stage = PublicSessionEpochBarrierStage::Excluded;
        barrier.retirement_operation_id = Some(retirement_operation_id);
        next.validate()?;
        Ok(next)
    }

    pub fn finish_epoch_barriers(&self) -> Result<Self, &'static str> {
        if self.stage != PublicSessionRenewalStage::EpochBarriersPending
            || self
                .barriers
                .iter()
                .any(|barrier| barrier.stage == PublicSessionEpochBarrierStage::Pending)
        {
            return Err("session-renewal epoch barriers are unresolved");
        }
        let mut next = self.clone();
        next.stage = if self.required.data_loss_decision()
            == Some(DataLossRenewalDecision::RetireWithoutCallback)
        {
            PublicSessionRenewalStage::Retired
        } else if self.required.replacement.is_some() {
            PublicSessionRenewalStage::ReplacementRolePending
        } else {
            PublicSessionRenewalStage::ConfigurationPending
        };
        next.validate()?;
        Ok(next)
    }

    pub fn replay_replacement_role(
        &self,
        process_session_id: &ProcessSessionId,
        epoch: Epoch,
        role: ReplicaRole,
    ) -> Result<Self, &'static str> {
        let replacement = self
            .required
            .replacement
            .as_ref()
            .ok_or("session renewal has no replacement")?;
        if self.stage != PublicSessionRenewalStage::ReplacementRolePending
            || process_session_id != &replacement.process_session_id
            || epoch != self.required.renewal_epoch
            || role != self.required.predecessor.role
        {
            return Err("replacement role replay is stale or differs from captured authority");
        }
        let mut next = self.clone();
        next.replayed_role = Some(role);
        next.stage = PublicSessionRenewalStage::ConfigurationPending;
        next.validate()?;
        Ok(next)
    }

    pub fn install_fresh_configuration(
        &self,
        configuration: &ConfigurationDescriptor,
    ) -> Result<Self, &'static str> {
        if self.stage != PublicSessionRenewalStage::ConfigurationPending
            || configuration != &self.required.fresh_configuration
        {
            return Err("fresh session configuration differs from captured authority");
        }
        let mut next = self.clone();
        next.installed_configuration = Some(configuration.clone());
        next.stage = PublicSessionRenewalStage::Authorized;
        next.validate()?;
        Ok(next)
    }

    pub fn is_successor_of(&self, previous: &Self) -> bool {
        self.required == previous.required
            && matches!(
                (previous.stage, self.stage),
                (
                    PublicSessionRenewalStage::Required,
                    PublicSessionRenewalStage::AccessClosed
                ) | (
                    PublicSessionRenewalStage::AccessClosed,
                    PublicSessionRenewalStage::SurvivingSetFrozen
                ) | (
                    PublicSessionRenewalStage::SurvivingSetFrozen,
                    PublicSessionRenewalStage::EpochBarriersPending
                ) | (
                    PublicSessionRenewalStage::EpochBarriersPending,
                    PublicSessionRenewalStage::EpochBarriersPending
                ) | (
                    PublicSessionRenewalStage::EpochBarriersPending,
                    PublicSessionRenewalStage::ReplacementRolePending
                        | PublicSessionRenewalStage::ConfigurationPending
                        | PublicSessionRenewalStage::Retired
                ) | (
                    PublicSessionRenewalStage::ReplacementRolePending,
                    PublicSessionRenewalStage::ConfigurationPending
                ) | (
                    PublicSessionRenewalStage::ConfigurationPending,
                    PublicSessionRenewalStage::Authorized
                )
            )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PublicDataLossRecoveryInput {
    pub authorization: DataLossAuthorization,
    pub attempt: DataLossAttempt,
}

impl PublicDataLossRecoveryInput {
    fn validate(&self, previous_configuration_number: i64) -> Result<(), &'static str> {
        self.authorization.validate()?;
        self.attempt
            .validate_for(&self.authorization, previous_configuration_number)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum PublicDataLossRecoveryStage {
    Authorized,
    Invalidated,
    CallbackProcessing,
    HistoryPending,
    ConfigurationPending,
    Admitted,
    Failed,
    ReplacementRequired,
    Retired,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PublicDataLossRecoveryStatus {
    pub authorization_id: OperationId,
    pub attempt: DataLossAttempt,
    pub predecessor_history: HistoryContext,
    pub proposed_history: HistoryContext,
    pub provisional_primary: ReplicaStorageIncarnationContext,
    #[schemars(with = "ProviderProgressResetSchema")]
    pub reset_scope: ProviderProgressReset,
    pub replay_disposition: DataLossReplay,
    pub stage: PublicDataLossRecoveryStage,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_progress: Option<PublicProgress>,
}

impl PublicDataLossRecoveryStatus {
    pub fn logical_authorization(&self) -> DataLossAuthorization {
        DataLossAuthorization {
            authorization_id: self.authorization_id.clone(),
            predecessor: self.predecessor_history.clone(),
            proposed: self.proposed_history.clone(),
            provisional_primary: self.provisional_primary.clone(),
            reset_scope: self.reset_scope.clone(),
            replay_disposition: self.replay_disposition,
        }
    }

    fn validate_for_report(
        &self,
        identity: &PublicOperationPreviewIdentity,
        resource_uid: &ResourceUid,
        replica: &ReplicaIdentity,
        storage_incarnation: &ReplicaStorageIncarnationContext,
        process_session_id: &ProcessSessionId,
        current_history: &HistoryContext,
    ) -> Result<(), &'static str> {
        self.predecessor_history.validate()?;
        self.proposed_history.validate()?;
        self.provisional_primary.validate()?;
        self.attempt.process.validate()?;
        self.logical_authorization().validate()?;
        if self.authorization_id.is_empty()
            || self.attempt.attempt_id.is_empty()
            || self.authorization_id != self.attempt.authorization_id
            || self.predecessor_history.resource_uid != *resource_uid
            || self.predecessor_history.protocol_generation.value() != identity.generation
            || self.proposed_history.resource_uid != *resource_uid
            || self.proposed_history.protocol_generation.value() != identity.generation
            || self.proposed_history.data_loss_number
                != self.predecessor_history.data_loss_number + 1
            || self.proposed_history
                != derive_data_loss_history(
                    &self.authorization_id,
                    &self.predecessor_history,
                    self.proposed_history.data_loss_number,
                    &self.provisional_primary,
                )?
            || self.attempt.process.process_session_id != *process_session_id
            || self.provisional_primary != *storage_incarnation
            || self.provisional_primary.replica != *replica
            || (!matches!(
                self.stage,
                PublicDataLossRecoveryStage::ReplacementRequired
                    | PublicDataLossRecoveryStage::Retired
            ) && self.attempt.process.data_loss_replay != self.replay_disposition)
        {
            return Err("public-v1 recovery status differs from report authority");
        }
        if self.stage == PublicDataLossRecoveryStage::Admitted {
            if &self.proposed_history != current_history {
                return Err("admitted recovery history differs from current report history");
            }
        } else if &self.predecessor_history != current_history {
            return Err("pending recovery history differs from current report history");
        }
        if let Some(progress) = &self.observed_progress {
            progress.validate(
                ReplicaRole::None,
                AccessStatus::NotPrimary,
                AccessStatus::NotPrimary,
            )?;
            if progress.role != PublicProgressRole::Recovery {
                return Err("data-loss recovery progress is not election-only");
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PublicLifecycleInput {
    pub recipe: PublicLifecycleRecipe,
    pub replica: ReplicaIdentity,
    pub epoch: Epoch,
    pub possible_data_loss: PossibleDataLossIntent,
    pub current: ConfigurationDescriptor,
    pub previous: Option<ConfigurationDescriptor>,
    #[serde(default)]
    pub replicas: Vec<PublicReplicaDescriptor>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_loss_recovery: Option<PublicDataLossRecoveryInput>,
}

impl PublicLifecycleInput {
    pub fn digest(&self) -> CanonicalOperationInputDigest {
        let mut encoder = CanonicalEncoder::new("kuberic-public-lifecycle-input-v1");
        encoder.u8(match self.recipe {
            PublicLifecycleRecipe::InitialPrimary => 1,
            PublicLifecycleRecipe::FailoverPromotion => 2,
            PublicLifecycleRecipe::SecondaryEpochAdvance => 3,
        });
        encoder.replica_identity(&self.replica);
        encoder.epoch(self.epoch);
        encoder.u8(match self.possible_data_loss {
            PossibleDataLossIntent::NotPossible => 0,
            PossibleDataLossIntent::Possible => 1,
        });
        encoder.configuration(&self.current);
        encoder.option(self.previous.as_ref(), |encoder, previous| {
            encoder.configuration(previous);
        });
        encoder.vec(&self.replicas, |encoder, replica| {
            encoder.public_replica(replica);
        });
        encoder.option(self.data_loss_recovery.as_ref(), |encoder, recovery| {
            encoder.data_loss_recovery(recovery)
        });
        CanonicalOperationInputDigest::V1(CanonicalOperationInputDigestV1::from_bytes(
            &encoder.finish(),
        ))
    }

    pub fn validate(&self) -> Result<(), &'static str> {
        self.validate_for_preview(true)
    }

    fn validate_authorization_revision(&self, revision: u64) -> Result<(), &'static str> {
        self.replicas.iter().try_for_each(|replica| {
            if replica.peer_authorization.is_some()
                && replica.configuration_revision != Some(revision)
            {
                Err("peer authorization revision differs from the lifecycle revision")
            } else {
                Ok(())
            }
        })
    }

    fn validate_for_preview(&self, public_v1: bool) -> Result<(), &'static str> {
        if self.current.epoch != self.epoch {
            return Err("lifecycle epoch does not match configuration");
        }
        for configuration in std::iter::once(&self.current).chain(self.previous.iter()) {
            if configuration.configuration_id != configuration.expected_id()
                || configuration.write_quorum == 0
                || configuration.write_quorum as usize > configuration.members.len()
                || configuration
                    .members
                    .iter()
                    .map(|member| &member.identity)
                    .collect::<std::collections::BTreeSet<_>>()
                    .len()
                    != configuration.members.len()
                || configuration
                    .members
                    .iter()
                    .filter(|member| member.role == ReplicaRole::Primary)
                    .count()
                    != 1
            {
                return Err("invalid frozen lifecycle configuration");
            }
            if self
                .previous
                .as_ref()
                .is_some_and(|previous| previous.epoch >= self.epoch)
            {
                return Err("lifecycle epoch must advance the previous epoch");
            }
        }
        let configuration = PublicConfiguration {
            current: self.current.clone(),
            previous: self.previous.clone(),
            replicas: self.replicas.clone(),
        };
        if public_v1 {
            configuration.validate_for_local(self.replica.replica_id)?;
        } else {
            configuration.validate_for_preview(false)?;
        }
        let role = if self.recipe == PublicLifecycleRecipe::SecondaryEpochAdvance {
            if self.possible_data_loss != PossibleDataLossIntent::NotPossible {
                return Err("secondary epoch update cannot request data loss");
            }
            ReplicaRole::ActiveSecondary
        } else {
            if self.current.primary_id != self.replica.replica_id {
                return Err("promotion target is not configuration primary");
            }
            ReplicaRole::Primary
        };
        if !self
            .current
            .members
            .iter()
            .any(|member| member.identity == self.replica && member.role == role)
        {
            return Err("lifecycle target is not an exact configuration member");
        }
        if self.recipe == PublicLifecycleRecipe::FailoverPromotion && self.previous.is_none() {
            return Err("failover requires frozen previous configuration");
        }
        match (public_v1, self.possible_data_loss, &self.data_loss_recovery) {
            (true, PossibleDataLossIntent::Possible, Some(recovery)) => {
                let previous_configuration_number = self
                    .previous
                    .as_ref()
                    .map_or(-1, |previous| previous.epoch.configuration_number);
                recovery.validate(previous_configuration_number)?;
                if recovery.authorization.provisional_primary.replica != self.replica
                    || recovery.attempt.process.epoch != self.epoch
                {
                    return Err("data-loss recovery target or attempt epoch changed");
                }
            }
            (true, PossibleDataLossIntent::Possible, None) => {
                return Err("public-v1 data loss requires typed authorization");
            }
            (_, PossibleDataLossIntent::NotPossible, Some(_)) => {
                return Err("ordinary lifecycle cannot carry data-loss authorization");
            }
            (false, _, Some(_)) => {
                return Err("protocol-10 lifecycle cannot carry public-v1 recovery");
            }
            _ => {}
        }
        Ok(())
    }

    pub fn configuration(&self) -> PublicConfiguration {
        PublicConfiguration {
            current: self.current.clone(),
            previous: self.previous.clone(),
            replicas: self.replicas.clone(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum ReplicaRebuildReason {
    IncompatibleRetainedHistory,
    ProviderMismatch,
    RemoteCopyConflict,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProviderReplicaIdentity {
    pub replica: ReplicaIdentity,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct LocalProviderRebuildRequired {
    pub reason: ReplicaRebuildReason,
    pub provider: ProviderReplicaIdentity,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RemoteBuildTargetRebuildRequired {
    pub reason: ReplicaRebuildReason,
    pub authorization: PublicBuildAuthorizationReference,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", tag = "kind", content = "detail")]
pub enum ReplicaRebuildRequired {
    LocalProvider(LocalProviderRebuildRequired),
    RemoteBuildTarget(Box<RemoteBuildTargetRebuildRequired>),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CapturedPublicOperationIdentity {
    pub resource_uid: ResourceUid,
    pub preview: PublicOperationPreviewIdentity,
    pub epoch: Epoch,
    pub operation_id: OperationId,
    pub revision: u64,
    pub detecting_session: ProcessSessionId,
}

impl CapturedPublicOperationIdentity {
    pub(crate) fn validate(&self) -> Result<(), &'static str> {
        if self.resource_uid.is_empty()
            || !self.preview.is_public_v1()
            || self.epoch.data_loss_number < 0
            || self.epoch.configuration_number < 0
            || self.operation_id.is_empty()
            || self.revision == 0
            || self.detecting_session.is_empty()
        {
            return Err("captured public-v1 operation identity is incomplete");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", tag = "kind", content = "detail")]
pub enum CanonicalReplicaRebuildRequired {
    LocalProvider {
        reason: ReplicaRebuildReason,
        provider: ProviderReplicaIdentity,
        operation: CapturedPublicOperationIdentity,
        local_replica: ReplicaIdentity,
        local_storage: PvcUid,
    },
    RemoteBuildTarget {
        reason: ReplicaRebuildReason,
        operation: CapturedPublicOperationIdentity,
        authorization: PublicBuildAuthorizationReference,
    },
}

impl CanonicalReplicaRebuildRequired {
    pub(crate) fn validate(&self) -> Result<(), &'static str> {
        match self {
            Self::LocalProvider {
                provider,
                operation,
                local_replica,
                local_storage,
                ..
            } => {
                operation.validate()?;
                validate_exact_replica_identity(&provider.replica)?;
                validate_exact_replica_identity(local_replica)?;
                if provider.replica != *local_replica || local_storage.is_empty() {
                    return Err("local rebuild identity differs from the captured provider");
                }
            }
            Self::RemoteBuildTarget {
                operation,
                authorization,
                ..
            } => {
                operation.validate()?;
                authorization.validate()?;
                if authorization.resource_uid != operation.resource_uid
                    || authorization.epoch != operation.epoch
                    || authorization.build_id != operation.operation_id
                {
                    return Err("remote rebuild authorization differs from captured operation");
                }
            }
        }
        Ok(())
    }

    fn validate_for_report(
        &self,
        completion_id: &OperationId,
        completion_revision: u64,
        report_identity: &PublicOperationPreviewIdentity,
        report_resource_uid: &ResourceUid,
        report_replica: &ReplicaIdentity,
        report_session: &ProcessSessionId,
    ) -> Result<(), &'static str> {
        self.validate()?;
        let operation = match self {
            Self::LocalProvider { operation, .. } | Self::RemoteBuildTarget { operation, .. } => {
                operation
            }
        };
        if &operation.operation_id != completion_id
            || operation.revision != completion_revision
            || &operation.preview != report_identity
            || &operation.resource_uid != report_resource_uid
            || &operation.detecting_session != report_session
        {
            return Err("rebuild evidence differs from its containing completion or report");
        }
        match self {
            Self::LocalProvider { local_replica, .. } if local_replica != report_replica => {
                Err("local rebuild evidence differs from the reporting replica")
            }
            Self::RemoteBuildTarget { authorization, .. }
                if &authorization.source != report_replica
                    || &authorization.source_session != report_session =>
            {
                Err("remote rebuild source differs from the reporting replica")
            }
            _ => Ok(()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum PublicProgressRole {
    Primary,
    Secondary,
    Recovery,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PublicProgress {
    pub role: PublicProgressRole,
    pub first: i64,
    pub last: i64,
}

impl PublicProgress {
    pub fn validate(
        &self,
        replica_role: ReplicaRole,
        read: AccessStatus,
        write: AccessStatus,
    ) -> Result<(), &'static str> {
        validate_progress_value(self.first)?;
        validate_progress_value(self.last)?;
        if self.first == -1 || self.last == -1 || self.first > self.last {
            return Err("completed public progress must be a valid ordered range");
        }
        if matches!(self.role, PublicProgressRole::Primary) && replica_role != ReplicaRole::Primary
        {
            return Err("primary progress requires the primary replica role");
        }
        if matches!(self.role, PublicProgressRole::Secondary)
            && replica_role != ReplicaRole::ActiveSecondary
        {
            return Err("secondary progress requires the active-secondary replica role");
        }
        if self.role == PublicProgressRole::Recovery
            && (read == AccessStatus::Granted || write == AccessStatus::Granted)
        {
            return Err("recovery progress cannot imply serving access");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", tag = "kind", content = "detail")]
pub enum PublicV1OperationResult {
    Succeeded,
    Failed(String),
    Ambiguous(String),
    Cancelled,
    RebuildRequired(Box<CanonicalReplicaRebuildRequired>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum PublicV1CallKind {
    InitialPrimary,
    FailoverPromotion,
    SecondaryEpochAdvance,
    Open,
    Role,
    Epoch,
    Configuration,
    CatchUp,
    Progress,
    Swap,
    Build,
    Remove,
    Close,
    Abort,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PublicV1OperationCompletion {
    pub operation_id: OperationId,
    pub revision: u64,
    pub call_kind: PublicV1CallKind,
    pub process_session_id: ProcessSessionId,
    pub input_digest: CanonicalOperationInputDigest,
    pub replay_disposition: PublicOperationReplayDisposition,
    pub history: HistoryContext,
    pub storage_incarnation: ReplicaStorageIncarnationContext,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_configuration_id: Option<ConfigurationId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_configuration_id: Option<ConfigurationId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub catch_up_mode: Option<PublicCatchUpMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub catch_up_specific_quorum: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authorization_key_id: Option<AuthorizationKeyId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authorization_audit_digest: Option<SignedAuthorizationAuditDigest>,
    pub result: PublicV1OperationResult,
}

impl PublicV1OperationCompletion {
    fn validate(
        &self,
        report_identity: &PublicOperationPreviewIdentity,
        report_resource_uid: &ResourceUid,
        report_replica: &ReplicaIdentity,
        report_session: &ProcessSessionId,
    ) -> Result<(), &'static str> {
        if self.operation_id.is_empty() || self.revision == 0 {
            return Err("public-v1 operation completion is incomplete");
        }
        if self.process_session_id.is_empty()
            || &self.process_session_id != report_session
            || self.history.resource_uid != *report_resource_uid
            || self.history.protocol_generation.value() != report_identity.generation
            || self.storage_incarnation.replica != *report_replica
        {
            return Err(
                "public-v1 operation completion differs from its report history, storage, or session",
            );
        }
        self.history.validate()?;
        self.storage_incarnation.validate()?;
        if self
            .previous_configuration_id
            .as_ref()
            .is_some_and(ConfigurationId::is_empty)
            || self
                .current_configuration_id
                .as_ref()
                .is_some_and(ConfigurationId::is_empty)
            || self.authorization_key_id.is_some() != self.authorization_audit_digest.is_some()
        {
            return Err("public-v1 operation completion authority metadata is incomplete");
        }
        self.input_digest.validate()?;
        match &self.result {
            PublicV1OperationResult::Failed(detail)
            | PublicV1OperationResult::Ambiguous(detail)
                if detail.is_empty() =>
            {
                return Err("public-v1 failure completion lacks detail");
            }
            PublicV1OperationResult::RebuildRequired(rebuild) => rebuild.validate_for_report(
                &self.operation_id,
                self.revision,
                report_identity,
                report_resource_uid,
                report_replica,
                report_session,
            )?,
            _ => {}
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PublicV1ExtensionSlot {
    pub version: u16,
}

impl PublicV1ExtensionSlot {
    fn validate(&self) -> Result<(), &'static str> {
        if self.version == 0 {
            return Err("public-v1 extension version must be positive");
        }
        Ok(())
    }
}

pub const MAX_PUBLIC_V1_AUTHORIZATION_INTENTS: usize = 16;
pub const MAX_PUBLIC_V1_BUILD_AUTHORIZATION_INTENTS: usize = 8;
pub const MAX_PUBLIC_V1_PEER_AUTHORIZATION_INTENTS: usize = 8;
const MAX_PUBLIC_V1_AUTHORIZATION_CANONICAL_BYTES: usize = 16 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PublicV1AuthorizationSecretBinding {
    #[schemars(length(min = 1, max = 253))]
    pub name: String,
    #[schemars(length(min = 1, max = 256))]
    pub uid: String,
    #[schemars(length(min = 1, max = 256))]
    pub resource_version: String,
    pub resource_uid: ResourceUid,
    pub generation: PublicProtocolGeneration,
    pub key_id: AuthorizationKeyId,
    pub verification_key: AuthorizationVerificationKey,
}

impl PublicV1AuthorizationSecretBinding {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.name.is_empty()
            || self.name.len() > 253
            || self.uid.is_empty()
            || self.uid.len() > 256
            || self.resource_version.is_empty()
            || self.resource_version.len() > 256
            || self.resource_uid.is_empty()
            || self.generation.value() == 0
        {
            return Err("public-v1 authorization Secret binding is incomplete");
        }
        if self.key_id != self.verification_key.key_id() {
            return Err("public-v1 authorization Secret binding key ID is invalid");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum PublicV1AuthorizationIntentKind {
    Build,
    Peer,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum PublicV1AuthorizationIntentScope {
    Build,
    CurrentPeer,
    PreviousPeer,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum PublicV1AuthorizationIntentStage {
    Prepared,
    Signed,
    Dispatched,
    Observed,
    Retired,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum PublicV1AuthorizationCleanupDisposition {
    RetainUntilSuperseded,
    RetireAfterObservation,
    Retired,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PublicV1AuthorizationClaims {
    pub kind: PublicV1AuthorizationIntentKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build: Option<Box<BuildAuthorizationClaimsV1>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peer: Option<Box<PeerSessionAuthorizationClaimsV1>>,
}

impl PublicV1AuthorizationClaims {
    pub fn build(claims: BuildAuthorizationClaimsV1) -> Self {
        Self {
            kind: PublicV1AuthorizationIntentKind::Build,
            build: Some(Box::new(claims)),
            peer: None,
        }
    }

    pub fn peer(claims: PeerSessionAuthorizationClaimsV1) -> Self {
        Self {
            kind: PublicV1AuthorizationIntentKind::Peer,
            build: None,
            peer: Some(Box::new(claims)),
        }
    }

    pub const fn kind(&self) -> PublicV1AuthorizationIntentKind {
        self.kind
    }

    pub fn canonical_bytes(&self) -> Result<Vec<u8>, &'static str> {
        match (self.kind, self.build.as_deref(), self.peer.as_deref()) {
            (PublicV1AuthorizationIntentKind::Build, Some(claims), None) => claims
                .canonical_bytes()
                .map_err(|_| "invalid public-v1 build authorization claims"),
            (PublicV1AuthorizationIntentKind::Peer, None, Some(claims)) => claims
                .canonical_bytes()
                .map_err(|_| "invalid public-v1 peer authorization claims"),
            _ => Err("public-v1 authorization claims kind is inconsistent"),
        }
    }

    fn validate(&self) -> Result<(), &'static str> {
        match (self.kind, self.build.as_deref(), self.peer.as_deref()) {
            (PublicV1AuthorizationIntentKind::Build, Some(claims), None) => claims
                .validate()
                .map_err(|_| "invalid public-v1 build authorization claims"),
            (PublicV1AuthorizationIntentKind::Peer, None, Some(claims)) => claims
                .validate()
                .map_err(|_| "invalid public-v1 peer authorization claims"),
            _ => Err("public-v1 authorization claims kind is inconsistent"),
        }
    }

    fn resource_uid(&self) -> &ResourceUid {
        self.build
            .as_deref()
            .map(|claims| &claims.resource_uid)
            .or_else(|| self.peer.as_deref().map(|claims| &claims.resource_uid))
            .expect("validated public-v1 authorization claims")
    }

    fn generation(&self) -> PublicProtocolGeneration {
        self.build
            .as_deref()
            .map(|claims| claims.protocol_generation)
            .or_else(|| {
                self.peer
                    .as_deref()
                    .map(|claims| claims.protocol_generation)
            })
            .expect("validated public-v1 authorization claims")
    }

    fn key_id(&self) -> AuthorizationKeyId {
        self.build
            .as_deref()
            .map(|claims| claims.key_id)
            .or_else(|| self.peer.as_deref().map(|claims| claims.key_id))
            .expect("validated public-v1 authorization claims")
    }

    fn history(&self) -> &HistoryContext {
        self.build
            .as_deref()
            .map(|claims| &claims.history)
            .or_else(|| self.peer.as_deref().map(|claims| &claims.history))
            .expect("validated public-v1 authorization claims")
    }

    pub fn build_target(&self) -> Option<&ReplicaIdentity> {
        self.build.as_deref().map(|claims| claims.target.replica())
    }

    pub fn peer_secondary(&self) -> Option<&ReplicaIdentity> {
        self.peer.as_deref().map(|claims| &claims.secondary.replica)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PublicV1AuthorizationEnvelope {
    pub kind: PublicV1AuthorizationIntentKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build: Option<Box<BuildAuthorizationV1>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peer: Option<Box<PeerSessionAuthorizationV1>>,
}

impl PublicV1AuthorizationEnvelope {
    pub fn build(authorization: BuildAuthorizationV1) -> Self {
        Self {
            kind: PublicV1AuthorizationIntentKind::Build,
            build: Some(Box::new(authorization)),
            peer: None,
        }
    }

    pub fn peer(authorization: PeerSessionAuthorizationV1) -> Self {
        Self {
            kind: PublicV1AuthorizationIntentKind::Peer,
            build: None,
            peer: Some(Box::new(authorization)),
        }
    }

    pub const fn kind(&self) -> PublicV1AuthorizationIntentKind {
        self.kind
    }

    fn validate_shape(&self) -> Result<(), &'static str> {
        match (self.kind, self.build.as_ref(), self.peer.as_ref()) {
            (PublicV1AuthorizationIntentKind::Build, Some(_), None)
            | (PublicV1AuthorizationIntentKind::Peer, None, Some(_)) => Ok(()),
            _ => Err("public-v1 authorization envelope kind is inconsistent"),
        }
    }

    pub fn claims(&self) -> Result<PublicV1AuthorizationClaims, &'static str> {
        match (self.kind, self.build.as_deref(), self.peer.as_deref()) {
            (PublicV1AuthorizationIntentKind::Build, Some(authorization), None) => Ok(
                PublicV1AuthorizationClaims::build(authorization.claims.clone()),
            ),
            (PublicV1AuthorizationIntentKind::Peer, None, Some(authorization)) => Ok(
                PublicV1AuthorizationClaims::peer(authorization.claims.clone()),
            ),
            _ => Err("public-v1 authorization envelope kind is inconsistent"),
        }
    }

    pub fn signature(&self) -> Result<AuthorizationSignature, &'static str> {
        self.build
            .as_deref()
            .map(|authorization| authorization.signature)
            .or_else(|| {
                self.peer
                    .as_deref()
                    .map(|authorization| authorization.signature)
            })
            .ok_or("public-v1 authorization envelope kind is inconsistent")
    }

    pub fn canonical_bytes(&self) -> Result<Vec<u8>, &'static str> {
        match (self.kind, self.build.as_deref(), self.peer.as_deref()) {
            (PublicV1AuthorizationIntentKind::Build, Some(authorization), None) => authorization
                .canonical_bytes()
                .map_err(|_| "invalid public-v1 build authorization envelope"),
            (PublicV1AuthorizationIntentKind::Peer, None, Some(authorization)) => authorization
                .canonical_bytes()
                .map_err(|_| "invalid public-v1 peer authorization envelope"),
            _ => Err("public-v1 authorization envelope kind is inconsistent"),
        }
    }

    pub fn audit_digest(&self) -> Result<SignedAuthorizationAuditDigest, &'static str> {
        match (self.kind, self.build.as_deref(), self.peer.as_deref()) {
            (PublicV1AuthorizationIntentKind::Build, Some(authorization), None) => authorization
                .audit_digest()
                .map_err(|_| "invalid public-v1 build authorization envelope"),
            (PublicV1AuthorizationIntentKind::Peer, None, Some(authorization)) => authorization
                .audit_digest()
                .map_err(|_| "invalid public-v1 peer authorization envelope"),
            _ => Err("public-v1 authorization envelope kind is inconsistent"),
        }
    }

    fn verify(&self, verification_key: &AuthorizationVerificationKey) -> Result<(), &'static str> {
        match (self.kind, self.build.as_deref(), self.peer.as_deref()) {
            (PublicV1AuthorizationIntentKind::Build, Some(authorization), None) => authorization
                .verify(verification_key)
                .map_err(|_| "invalid public-v1 build authorization envelope"),
            (PublicV1AuthorizationIntentKind::Peer, None, Some(authorization)) => authorization
                .verify(verification_key)
                .map_err(|_| "invalid public-v1 peer authorization envelope"),
            _ => Err("public-v1 authorization envelope kind is inconsistent"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PublicV1AuthorizationIntent {
    pub kind: PublicV1AuthorizationIntentKind,
    pub scope: PublicV1AuthorizationIntentScope,
    pub operation_id: OperationId,
    pub revision: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claims: Option<PublicV1AuthorizationClaims>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(min = 1, max = 21848))]
    pub canonical_claims_base64: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub envelope: Option<PublicV1AuthorizationEnvelope>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<AuthorizationSignature>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audit_digest: Option<SignedAuthorizationAuditDigest>,
    pub stage: PublicV1AuthorizationIntentStage,
    pub cleanup_disposition: PublicV1AuthorizationCleanupDisposition,
}

impl PublicV1AuthorizationIntent {
    pub fn prepared(
        scope: PublicV1AuthorizationIntentScope,
        operation_id: OperationId,
        revision: u64,
        claims: PublicV1AuthorizationClaims,
        cleanup_disposition: PublicV1AuthorizationCleanupDisposition,
    ) -> Result<Self, &'static str> {
        if cleanup_disposition == PublicV1AuthorizationCleanupDisposition::Retired {
            return Err("prepared public-v1 authorization cannot already be retired");
        }
        let canonical_claims = claims.canonical_bytes()?;
        if canonical_claims.len() > MAX_PUBLIC_V1_AUTHORIZATION_CANONICAL_BYTES {
            return Err("public-v1 authorization claims exceed the accepted bound");
        }
        let intent = Self {
            kind: claims.kind(),
            scope,
            operation_id,
            revision,
            claims: Some(claims),
            canonical_claims_base64: Some(BASE64_STANDARD.encode(canonical_claims)),
            envelope: None,
            signature: None,
            audit_digest: None,
            stage: PublicV1AuthorizationIntentStage::Prepared,
            cleanup_disposition,
        };
        intent.validate(None)?;
        Ok(intent)
    }

    pub fn validate(
        &self,
        binding: Option<&PublicV1AuthorizationSecretBinding>,
    ) -> Result<(), &'static str> {
        let valid_scope = matches!(
            (self.kind, self.scope),
            (
                PublicV1AuthorizationIntentKind::Build,
                PublicV1AuthorizationIntentScope::Build
            ) | (
                PublicV1AuthorizationIntentKind::Peer,
                PublicV1AuthorizationIntentScope::CurrentPeer
                    | PublicV1AuthorizationIntentScope::PreviousPeer
            )
        );
        if self.operation_id.is_empty() || self.revision == 0 || !valid_scope {
            return Err("public-v1 authorization intent identity is incomplete");
        }
        match self.stage {
            PublicV1AuthorizationIntentStage::Prepared => {
                self.validate_active_claims(binding)?;
                if self.envelope.is_some()
                    || self.signature.is_some()
                    || self.audit_digest.is_some()
                    || self.cleanup_disposition == PublicV1AuthorizationCleanupDisposition::Retired
                {
                    return Err("prepared public-v1 authorization contains signed authority");
                }
            }
            PublicV1AuthorizationIntentStage::Signed
            | PublicV1AuthorizationIntentStage::Dispatched
            | PublicV1AuthorizationIntentStage::Observed => {
                let claims = self.validate_active_claims(binding)?;
                let binding =
                    binding.ok_or("signed public-v1 authorization lacks its Secret binding")?;
                let envelope = self
                    .envelope
                    .as_ref()
                    .ok_or("signed public-v1 authorization lacks its envelope")?;
                envelope.validate_shape()?;
                if envelope.kind() != self.kind
                    || envelope.claims()? != *claims
                    || self.signature != Some(envelope.signature()?)
                    || self.audit_digest != Some(envelope.audit_digest()?)
                {
                    return Err("signed public-v1 authorization differs from its exact envelope");
                }
                envelope.verify(&binding.verification_key)?;
                if self.cleanup_disposition == PublicV1AuthorizationCleanupDisposition::Retired {
                    return Err("active public-v1 authorization has a retired disposition");
                }
            }
            PublicV1AuthorizationIntentStage::Retired => {
                if self.cleanup_disposition != PublicV1AuthorizationCleanupDisposition::Retired {
                    return Err("public-v1 authorization retirement disposition is inconsistent");
                }
                if self.claims.is_some()
                    || self.canonical_claims_base64.is_some()
                    || self.envelope.is_some()
                    || self.signature.is_some()
                {
                    return Err("retired public-v1 authorization must retain audit identity only");
                }
            }
        }
        Ok(())
    }

    fn validate_active_claims(
        &self,
        binding: Option<&PublicV1AuthorizationSecretBinding>,
    ) -> Result<&PublicV1AuthorizationClaims, &'static str> {
        let claims = self
            .claims
            .as_ref()
            .ok_or("active public-v1 authorization lacks typed claims")?;
        let canonical_claims_base64 = self
            .canonical_claims_base64
            .as_ref()
            .ok_or("active public-v1 authorization lacks canonical claims")?;
        if self.kind != claims.kind() {
            return Err("public-v1 authorization intent kind differs from its claims");
        }
        claims.validate()?;
        let canonical_claims = BASE64_STANDARD
            .decode(canonical_claims_base64)
            .map_err(|_| "public-v1 authorization claims are not canonical base64")?;
        if canonical_claims.len() > MAX_PUBLIC_V1_AUTHORIZATION_CANONICAL_BYTES
            || BASE64_STANDARD.encode(&canonical_claims) != *canonical_claims_base64
            || canonical_claims != claims.canonical_bytes()?
        {
            return Err("public-v1 authorization claims differ from their canonical bytes");
        }
        if let Some(binding) = binding
            && (claims.resource_uid() != &binding.resource_uid
                || claims.generation() != binding.generation
                || claims.key_id() != binding.key_id)
        {
            return Err("public-v1 authorization claims differ from the Secret binding");
        }
        Ok(claims)
    }

    pub fn is_successor_of(&self, previous: &Self) -> bool {
        let identity_matches = self.kind == previous.kind
            && self.scope == previous.scope
            && self.operation_id == previous.operation_id
            && self.revision == previous.revision
            && (self.cleanup_disposition == previous.cleanup_disposition
                || self.stage == PublicV1AuthorizationIntentStage::Retired
                    && self.cleanup_disposition
                        == PublicV1AuthorizationCleanupDisposition::Retired);
        if !identity_matches {
            return false;
        }
        let valid_transition = matches!(
            (previous.stage, self.stage),
            (
                PublicV1AuthorizationIntentStage::Prepared,
                PublicV1AuthorizationIntentStage::Signed
            ) | (
                PublicV1AuthorizationIntentStage::Signed,
                PublicV1AuthorizationIntentStage::Dispatched
            ) | (
                PublicV1AuthorizationIntentStage::Dispatched,
                PublicV1AuthorizationIntentStage::Observed
            ) | (
                PublicV1AuthorizationIntentStage::Observed,
                PublicV1AuthorizationIntentStage::Retired
            ) | (
                PublicV1AuthorizationIntentStage::Prepared
                    | PublicV1AuthorizationIntentStage::Signed
                    | PublicV1AuthorizationIntentStage::Dispatched,
                PublicV1AuthorizationIntentStage::Retired
            )
        );
        if !valid_transition {
            return false;
        }
        if self.stage == PublicV1AuthorizationIntentStage::Retired {
            self.claims.is_none()
                && self.canonical_claims_base64.is_none()
                && self.envelope.is_none()
                && self.signature.is_none()
                && self.audit_digest == previous.audit_digest
        } else {
            let claims_match = self.claims == previous.claims
                && self.canonical_claims_base64 == previous.canonical_claims_base64;
            claims_match
                && (previous.stage == PublicV1AuthorizationIntentStage::Prepared
                    && self.stage == PublicV1AuthorizationIntentStage::Signed
                    || self.envelope == previous.envelope
                        && self.signature == previous.signature
                        && self.audit_digest == previous.audit_digest)
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PublicV1AuthorizationStatus {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret_binding: Option<PublicV1AuthorizationSecretBinding>,
    #[serde(default)]
    #[schemars(length(max = 16))]
    pub intents: Vec<PublicV1AuthorizationIntent>,
}

impl PublicV1AuthorizationStatus {
    pub fn validate(
        &self,
        identity: &PublicOperationPreviewIdentity,
        history: &HistoryContext,
    ) -> Result<(), &'static str> {
        if !identity.is_public_v1() {
            return Err("public-v1 authorization requires protocol 11");
        }
        history.validate()?;
        if self.intents.len() > MAX_PUBLIC_V1_AUTHORIZATION_INTENTS {
            return Err("public-v1 authorization intent retention exceeds the accepted bound");
        }
        let Some(binding) = &self.secret_binding else {
            if self.intents.is_empty() {
                return Ok(());
            }
            return Err("public-v1 authorization intents lack a Secret binding");
        };
        binding.validate()?;
        if binding.resource_uid != history.resource_uid
            || binding.generation.value() != identity.generation
            || binding.generation != history.protocol_generation
        {
            return Err("public-v1 authorization Secret binding differs from accepted history");
        }

        let mut operations = std::collections::BTreeSet::new();
        let mut build_targets = std::collections::BTreeSet::new();
        let mut current_peers = std::collections::BTreeSet::new();
        let mut previous_peers = std::collections::BTreeSet::new();
        let mut active_builds = 0;
        let mut active_current_peers = 0;
        let mut active_previous_peers = 0;
        for intent in &self.intents {
            intent.validate(Some(binding))?;
            if !operations.insert(&intent.operation_id) {
                return Err("retired public-v1 authorization intents are inconsistent");
            }
            if intent.stage == PublicV1AuthorizationIntentStage::Retired {
                continue;
            }
            let claims = intent
                .claims
                .as_ref()
                .ok_or("active public-v1 authorization lacks typed claims")?;
            if claims.history() != history {
                return Err("active public-v1 authorization history is stale");
            }
            match intent.scope {
                PublicV1AuthorizationIntentScope::Build => {
                    active_builds += 1;
                    if !build_targets.insert(
                        intent
                            .claims
                            .as_ref()
                            .ok_or("build authorization lacks claims")?
                            .build_target()
                            .ok_or("build authorization lacks a target")?,
                    ) {
                        return Err(
                            "public-v1 build authorization intents are duplicated or inconsistent",
                        );
                    }
                }
                PublicV1AuthorizationIntentScope::CurrentPeer => {
                    active_current_peers += 1;
                    if !current_peers.insert(
                        intent
                            .claims
                            .as_ref()
                            .ok_or("peer authorization lacks claims")?
                            .peer_secondary()
                            .ok_or("peer authorization lacks a secondary")?,
                    ) {
                        return Err(
                            "public-v1 peer authorization intents are duplicated or inconsistent",
                        );
                    }
                }
                PublicV1AuthorizationIntentScope::PreviousPeer => {
                    active_previous_peers += 1;
                    if !previous_peers.insert(
                        intent
                            .claims
                            .as_ref()
                            .ok_or("peer authorization lacks claims")?
                            .peer_secondary()
                            .ok_or("peer authorization lacks a secondary")?,
                    ) {
                        return Err(
                            "public-v1 peer authorization intents are duplicated or inconsistent",
                        );
                    }
                }
            }
        }
        if active_builds > MAX_PUBLIC_V1_BUILD_AUTHORIZATION_INTENTS
            || active_current_peers > MAX_PUBLIC_V1_PEER_AUTHORIZATION_INTENTS
            || active_previous_peers > MAX_PUBLIC_V1_PEER_AUTHORIZATION_INTENTS
        {
            return Err("active public-v1 authorization intents exceed their accepted bound");
        }
        Ok(())
    }

    pub fn find_intent(&self, operation_id: &OperationId) -> Option<&PublicV1AuthorizationIntent> {
        self.intents
            .iter()
            .find(|intent| &intent.operation_id == operation_id)
    }

    pub fn is_successor_of(&self, previous: &Self) -> bool {
        if self.secret_binding != previous.secret_binding {
            return previous.secret_binding.is_none()
                && self.secret_binding.is_some()
                && previous.intent_count() == 0
                && self.intent_count() == 0;
        }
        if self.intent_count() > previous.intent_count() + 1 {
            return false;
        }
        for previous_intent in previous.intents() {
            let Some(next_intent) = self.find_intent(&previous_intent.operation_id) else {
                if previous_intent.stage == PublicV1AuthorizationIntentStage::Retired {
                    continue;
                }
                return false;
            };
            if next_intent != previous_intent && !next_intent.is_successor_of(previous_intent) {
                return false;
            }
        }
        self.intents().all(|next_intent| {
            previous
                .find_intent(&next_intent.operation_id)
                .is_some_and(|previous_intent| {
                    next_intent == previous_intent || next_intent.is_successor_of(previous_intent)
                })
                || next_intent.stage == PublicV1AuthorizationIntentStage::Prepared
        })
    }

    fn intent_count(&self) -> usize {
        self.intents.len()
    }

    fn intents(&self) -> impl Iterator<Item = &PublicV1AuthorizationIntent> {
        self.intents.iter()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct PublicV1Extensions {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub history_recovery: Option<PublicV1ExtensionSlot>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signed_authorization: Option<PublicV1ExtensionSlot>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub public_build_evidence: Option<PublicV1ExtensionSlot>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PublicV1AgentReport {
    pub identity: PublicOperationPreviewIdentity,
    pub resource_uid: ResourceUid,
    pub replica: ReplicaIdentity,
    pub process_session_id: ProcessSessionId,
    pub report_sequence: u64,
    pub role: ReplicaRole,
    pub read_status: AccessStatus,
    pub write_status: AccessStatus,
    pub history: HistoryContext,
    pub storage_incarnation: ReplicaStorageIncarnationContext,
    pub progress: PublicProgress,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_loss_recovery: Option<PublicDataLossRecoveryStatus>,
    #[serde(default)]
    pub completed_operations: Vec<PublicV1OperationCompletion>,
    #[serde(default)]
    pub extensions: PublicV1Extensions,
}

impl PublicV1AgentReport {
    pub fn validate(&self) -> Result<(), &'static str> {
        if !self.identity.is_public_v1()
            || self.resource_uid.is_empty()
            || self.process_session_id.is_empty()
            || self.report_sequence == 0
        {
            return Err("invalid public-v1 report identity");
        }
        validate_exact_replica_identity(&self.replica)?;
        self.history.validate()?;
        self.storage_incarnation.validate()?;
        if self.history.resource_uid != self.resource_uid
            || self.history.protocol_generation.value() != self.identity.generation
            || self.storage_incarnation.replica != self.replica
        {
            return Err("public-v1 history or storage differs from report identity");
        }
        self.progress
            .validate(self.role, self.read_status, self.write_status)?;
        if let Some(recovery) = &self.data_loss_recovery {
            recovery.validate_for_report(
                &self.identity,
                &self.resource_uid,
                &self.replica,
                &self.storage_incarnation,
                &self.process_session_id,
                &self.history,
            )?;
            if recovery.stage != PublicDataLossRecoveryStage::Admitted
                && (self.read_status == AccessStatus::Granted
                    || self.write_status == AccessStatus::Granted)
            {
                return Err("pending data-loss recovery cannot imply serving access");
            }
        }
        if self.progress.role == PublicProgressRole::Recovery
            && !self.data_loss_recovery.as_ref().is_some_and(|recovery| {
                matches!(
                    recovery.stage,
                    PublicDataLossRecoveryStage::Authorized
                        | PublicDataLossRecoveryStage::Invalidated
                        | PublicDataLossRecoveryStage::CallbackProcessing
                        | PublicDataLossRecoveryStage::HistoryPending
                        | PublicDataLossRecoveryStage::ConfigurationPending
                )
            })
        {
            return Err("recovery progress requires nonterminal recovery authority");
        }
        let mut operations = std::collections::BTreeSet::new();
        for completion in &self.completed_operations {
            completion.validate(
                &self.identity,
                &self.resource_uid,
                &self.replica,
                &self.process_session_id,
            )?;
            if !operations.insert(&completion.operation_id) {
                return Err("public-v1 operation completion is incomplete or duplicated");
            }
        }
        for extension in [
            self.extensions.history_recovery.as_ref(),
            self.extensions.signed_authorization.as_ref(),
            self.extensions.public_build_evidence.as_ref(),
        ]
        .into_iter()
        .flatten()
        {
            extension.validate()?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum PublicV1AcceptedReplayDisposition {
    Convergent,
    RepeatableRead,
    ReevaluateCatchUp,
    RetainBuildSuccessRetireAmbiguity,
    DataLossConvergent,
    DataLossReplaceOnAmbiguity,
    ExactRemoval,
    Containment,
}

impl From<PublicOperationReplayDisposition> for PublicV1AcceptedReplayDisposition {
    fn from(value: PublicOperationReplayDisposition) -> Self {
        match value {
            PublicOperationReplayDisposition::Convergent => Self::Convergent,
            PublicOperationReplayDisposition::RepeatableRead => Self::RepeatableRead,
            PublicOperationReplayDisposition::ReevaluateCatchUp => Self::ReevaluateCatchUp,
            PublicOperationReplayDisposition::RetainBuildSuccessRetireAmbiguity => {
                Self::RetainBuildSuccessRetireAmbiguity
            }
            PublicOperationReplayDisposition::DataLoss(DataLossReplay::Convergent) => {
                Self::DataLossConvergent
            }
            PublicOperationReplayDisposition::DataLoss(DataLossReplay::ReplaceOnAmbiguity) => {
                Self::DataLossReplaceOnAmbiguity
            }
            PublicOperationReplayDisposition::ExactRemoval => Self::ExactRemoval,
            PublicOperationReplayDisposition::Containment => Self::Containment,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum PublicV1AcceptedResult {
    Succeeded,
    Failed,
    Ambiguous,
    Cancelled,
    RebuildRequired,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PublicV1AcceptedOperationCompletion {
    pub operation_id: OperationId,
    pub revision: u64,
    pub input_digest_version: u16,
    pub input_digest_sha256: String,
    pub replay_disposition: PublicV1AcceptedReplayDisposition,
    pub result: PublicV1AcceptedResult,
    pub result_digest_sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PublicV1AcceptedStatus {
    pub identity: PublicOperationPreviewIdentity,
    pub history_initialization_id: crate::protocol::types::HistoryInitializationId,
    pub history: HistoryContext,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_loss_recovery: Option<PublicDataLossRecoveryStatus>,
    #[serde(default)]
    pub completed_operations: Vec<PublicV1AcceptedOperationCompletion>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authorization: Option<PublicV1AuthorizationStatus>,
    #[serde(default)]
    pub extensions: PublicV1Extensions,
}

impl PublicV1AcceptedStatus {
    pub fn authorization(&self) -> Option<&PublicV1AuthorizationStatus> {
        self.authorization.as_ref()
    }

    pub fn authorization_mut(&mut self) -> &mut PublicV1AuthorizationStatus {
        self.authorization
            .get_or_insert_with(PublicV1AuthorizationStatus::default)
    }

    pub fn validate_authorization(&self) -> Result<(), &'static str> {
        if let Some(authorization) = self.authorization() {
            authorization.validate(&self.identity, &self.history)?;
        }
        Ok(())
    }

    pub fn is_authorization_successor_of(&self, previous: &Self) -> bool {
        if self.identity != previous.identity
            || self.history_initialization_id != previous.history_initialization_id
            || self.history != previous.history
            || self.data_loss_recovery != previous.data_loss_recovery
            || self.completed_operations != previous.completed_operations
            || self.extensions != previous.extensions
        {
            return false;
        }
        match (&previous.authorization, &self.authorization) {
            (None, None) => true,
            (None, Some(next)) => next.secret_binding.is_some() && next.intents.is_empty(),
            (Some(previous), Some(next)) => next.is_successor_of(previous),
            (Some(_), None) => false,
        }
    }
}

/// Application-owned opaque address, never a Replicator transport endpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ServiceLocation {
    pub preview: PublicOperationPreviewIdentity,
    pub resource_uid: ResourceUid,
    pub operation_id: OperationId,
    pub replica: ReplicaIdentity,
    pub process_session_id: ProcessSessionId,
    pub epoch: Epoch,
    pub revision: u64,
    pub address: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PublicLifecycleReport {
    pub preview: PublicOperationPreviewIdentity,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binding: Option<PreviewLifecycleBinding>,
    pub resource_uid: ResourceUid,
    pub replica: ReplicaIdentity,
    pub process_session_id: ProcessSessionId,
    pub process_id: u32,
    pub revision: u64,
    pub operation_id: Option<OperationId>,
    pub role: ReplicaRole,
    pub write_access: bool,
    pub service_location: Option<ServiceLocation>,
}

pub fn service_location_address_digest(location: Option<&ServiceLocation>) -> String {
    use sha2::Digest;
    let mut hasher = sha2::Sha256::new();
    match location {
        Some(location) => {
            hasher.update(b"some:");
            hasher.update(location.address.len().to_le_bytes());
            hasher.update(location.address.as_bytes());
        }
        None => hasher.update(b"none"),
    }
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

struct CanonicalEncoder {
    bytes: Vec<u8>,
}

impl CanonicalEncoder {
    fn new(domain: &str) -> Self {
        let mut encoder = Self { bytes: Vec::new() };
        encoder.string(domain);
        encoder
    }

    fn finish(self) -> Vec<u8> {
        self.bytes
    }

    fn u8(&mut self, value: u8) {
        self.bytes.push(value);
    }

    fn bool(&mut self, value: bool) {
        self.u8(u8::from(value));
    }

    fn u32(&mut self, value: u32) {
        self.bytes.extend_from_slice(&value.to_be_bytes());
    }

    fn u64(&mut self, value: u64) {
        self.bytes.extend_from_slice(&value.to_be_bytes());
    }

    fn i64(&mut self, value: i64) {
        self.bytes.extend_from_slice(&value.to_be_bytes());
    }

    fn bytes(&mut self, value: &[u8]) {
        self.u32(u32::try_from(value.len()).expect("canonical field length fits u32"));
        self.bytes.extend_from_slice(value);
    }

    fn string(&mut self, value: &str) {
        self.bytes(value.as_bytes());
    }

    fn option<T>(&mut self, value: Option<&T>, encode: impl FnOnce(&mut Self, &T)) {
        match value {
            Some(value) => {
                self.u8(1);
                encode(self, value);
            }
            None => self.u8(0),
        }
    }

    fn vec<T>(&mut self, values: &[T], mut encode: impl FnMut(&mut Self, &T)) {
        self.u32(u32::try_from(values.len()).expect("canonical collection length fits u32"));
        for value in values {
            encode(self, value);
        }
    }

    fn epoch(&mut self, epoch: Epoch) {
        self.i64(epoch.data_loss_number);
        self.i64(epoch.configuration_number);
    }

    fn replica_role(&mut self, role: ReplicaRole) {
        self.u8(match role {
            ReplicaRole::Primary => 1,
            ReplicaRole::ActiveSecondary => 2,
            ReplicaRole::IdleSecondary => 3,
            ReplicaRole::None => 4,
        });
    }

    fn catch_up_mode(&mut self, mode: PublicCatchUpMode) {
        self.u8(match mode {
            PublicCatchUpMode::WriteQuorum => 1,
            PublicCatchUpMode::All => 2,
        });
    }

    fn replica_identity(&mut self, identity: &ReplicaIdentity) {
        self.i64(identity.replica_id.value());
        self.string(identity.instance_id.as_str());
        self.string(identity.agent_generation.as_str());
    }

    fn configuration(&mut self, configuration: &ConfigurationDescriptor) {
        self.string(configuration.configuration_id.as_str());
        self.epoch(configuration.epoch);
        self.i64(configuration.primary_id.value());
        self.u32(configuration.write_quorum);
        let mut members = configuration.members.iter().collect::<Vec<_>>();
        members.sort_by(|left, right| left.identity.cmp(&right.identity));
        self.u32(u32::try_from(members.len()).expect("configuration members fit u32"));
        for member in members {
            self.replica_identity(&member.identity);
            self.replica_role(member.role);
        }
    }

    fn public_replica(&mut self, replica: &PublicReplicaDescriptor) {
        self.string(replica.build_id.as_str());
        self.replica_identity(&replica.identity);
        self.string(&replica.replication_address);
        self.string(replica.process_session_id.as_str());
        self.replica_role(replica.role);
        self.i64(replica.current_progress);
        self.i64(replica.catch_up_capability);
        self.bool(replica.must_catchup);
        self.option(
            replica.peer_authorization.as_ref(),
            |encoder, authorization| {
                encoder.bytes(
                    &authorization
                        .canonical_bytes()
                        .expect("validated peer authorization is canonical"),
                );
            },
        );
        self.option(
            replica.configuration_revision.as_ref(),
            |encoder, revision| encoder.u64(*revision),
        );
    }

    fn public_configuration(&mut self, configuration: &PublicConfiguration) {
        self.configuration(&configuration.current);
        self.option(configuration.previous.as_ref(), |encoder, previous| {
            encoder.configuration(previous);
        });
        let mut replicas = configuration.replicas.iter().collect::<Vec<_>>();
        replicas.sort_by(|left, right| left.identity.cmp(&right.identity));
        self.u32(u32::try_from(replicas.len()).expect("public replicas fit u32"));
        for replica in replicas {
            self.public_replica(replica);
        }
    }

    fn authorization(&mut self, authorization: &PublicBuildAuthorizationReference) {
        self.string(authorization.resource_uid.as_str());
        self.epoch(authorization.epoch);
        self.string(authorization.build_id.as_str());
        self.replica_identity(&authorization.source);
        self.string(authorization.source_storage.as_str());
        self.string(authorization.source_session.as_str());
        self.replica_identity(&authorization.target);
        self.string(authorization.target_storage.as_str());
        self.string(authorization.target_session.as_str());
        self.string(&authorization.audit_digest.0);
    }

    fn public_build(&mut self, build: &PublicBuildInput) {
        self.string(build.attempt.as_str());
        self.replica_identity(&build.replica);
        self.string(build.process_session_id.as_str());
        self.string(&build.replication_address);
        self.option(build.history.as_ref(), |encoder, history| {
            encoder.history(history);
        });
        self.option(build.authorization.as_ref(), |encoder, authorization| {
            encoder.authorization(authorization);
        });
        self.option(
            build.authorization_verification_key.as_ref(),
            |encoder, verification_key| encoder.bytes(verification_key.as_bytes()),
        );
        self.option(
            build.build_authorization.as_ref(),
            |encoder, authorization| {
                encoder.bytes(
                    &authorization
                        .canonical_bytes()
                        .expect("validated build authorization is canonical"),
                );
            },
        );
    }

    fn history(&mut self, history: &HistoryContext) {
        self.string(history.resource_uid.as_str());
        self.u64(history.protocol_generation.value());
        self.string(history.history_id.as_str());
        self.i64(history.data_loss_number);
    }

    fn storage_incarnation(&mut self, storage: &ReplicaStorageIncarnationContext) {
        self.replica_identity(&storage.replica);
        match &storage.storage {
            ReplicaStorageIdentity::Persistent { pod_uid, pvc_uid } => {
                self.u8(1);
                self.string(pod_uid.as_str());
                self.string(pvc_uid.as_str());
            }
            ReplicaStorageIdentity::Volatile { identity } => {
                self.u8(2);
                self.string(identity);
            }
        }
        self.string(storage.storage_generation.as_str());
    }

    fn data_loss_recovery(&mut self, recovery: &PublicDataLossRecoveryInput) {
        let authorization = &recovery.authorization;
        self.string(authorization.authorization_id.as_str());
        self.history(&authorization.predecessor);
        self.history(&authorization.proposed);
        self.storage_incarnation(&authorization.provisional_primary);
        match &authorization.reset_scope {
            ProviderProgressReset::Preserve => self.u8(1),
            ProviderProgressReset::ResetToEmpty => self.u8(2),
            ProviderProgressReset::ReplaceIncarnation { retired } => {
                self.u8(3);
                self.storage_incarnation(retired);
            }
        }
        self.u8(match authorization.replay_disposition {
            DataLossReplay::Convergent => 1,
            DataLossReplay::ReplaceOnAmbiguity => 2,
        });
        let attempt = &recovery.attempt;
        self.string(attempt.authorization_id.as_str());
        self.string(attempt.attempt_id.as_str());
        self.string(attempt.process.process_session_id.as_str());
        self.string(attempt.process.replicator_creation_id.as_str());
        self.bool(attempt.process.catch_up_specific_quorum);
        self.u8(match attempt.process.data_loss_replay {
            DataLossReplay::Convergent => 1,
            DataLossReplay::ReplaceOnAmbiguity => 2,
        });
        self.epoch(attempt.process.epoch);
        self.u64(attempt.process.attempt_revision);
    }
}
