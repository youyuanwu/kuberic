//! Dormant public-operation preview identities and admission inputs.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::protocol::types::{
    AccessStatus, ConfigurationDescriptor, Epoch, FaultType, OperationId, PodUid, ProcessSessionId,
    PvcUid, ReplicaId, ReplicaIdentity, ReplicaRole, ResourceUid,
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
            input.validate()?;
            if self.input_digest != input.digest() {
                return Err("lifecycle digest does not match frozen input");
            }
            match input.possible_data_loss {
                PossibleDataLossIntent::NotPossible
                    if self.replay_disposition != PublicOperationReplayDisposition::Convergent =>
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
        }
        if let Some(program) = &self.program {
            program.validate(&self.class, &self.operation_id)?;
            if self.input_digest != program.digest() {
                return Err("program digest does not match frozen input");
            }
            if self.replay_disposition != program.replay_disposition() {
                return Err("program replay disposition does not match frozen input");
            }
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

    pub fn validate_for_local(&self, local_replica_id: ReplicaId) -> Result<(), &'static str> {
        self.validate_descriptors()?;
        let mut identities = std::collections::BTreeSet::new();
        let mut successor = None;
        for replica in &self.replicas {
            replica.validate()?;
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
}

impl PublicReplicaDescriptor {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.replication_address.is_empty() || self.process_session_id.is_empty() {
            return Err("public replica descriptor lacks an exact address or process session");
        }
        validate_progress_value(self.current_progress)?;
        validate_progress_value(self.catch_up_capability)?;
        if self.role == ReplicaRole::ActiveSecondary
            && (self.current_progress == INVALID_LSN || self.catch_up_capability == INVALID_LSN)
        {
            return Err("ready public peer cannot use invalid progress");
        }
        if self.role == ReplicaRole::IdleSecondary
            && (self.current_progress != INVALID_LSN
                || self.catch_up_capability != INVALID_LSN
                || self.must_catchup)
        {
            return Err("idle public build descriptor must use invalid progress");
        }
        Ok(())
    }
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
    fn validate(&self) -> Result<(), &'static str> {
        if self.resource_uid.is_empty()
            || self.build_id.is_empty()
            || self.source_storage.is_empty()
            || self.target_storage.is_empty()
            || self.source_session.is_empty()
            || self.target_session.is_empty()
        {
            return Err("public build authorization reference is incomplete");
        }
        self.audit_digest.validate()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PublicBuildInput {
    pub attempt: OperationId,
    pub replica: ReplicaIdentity,
    pub process_session_id: ProcessSessionId,
    pub replication_address: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authorization: Option<PublicBuildAuthorizationReference>,
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

    fn validate(&self, class: &PublicOperationClass, id: &OperationId) -> Result<(), &'static str> {
        let expected = match self {
            Self::Swap {
                starting,
                refreshed,
                epoch,
                handoff,
                catch_up_specific_quorum,
                mode,
            } => {
                starting.validate()?;
                refreshed.validate()?;
                if starting.current.epoch >= *epoch
                    || refreshed.current.epoch != *epoch
                    || !matches!(handoff, ReplicaRole::ActiveSecondary | ReplicaRole::None)
                {
                    return Err("invalid swap epoch or handoff");
                }
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
                PublicOperationClass::PlannedSwap
            }
            Self::Configuration(configuration) | Self::CatchUp { configuration, .. } => {
                configuration.validate()?;
                PublicOperationClass::Authority
            }
            Self::Build(build) | Self::Remove(build) => {
                if build.attempt.is_empty()
                    || build.process_session_id.is_empty()
                    || build.replication_address.is_empty()
                {
                    return Err("incomplete exact build identity");
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
        CanonicalOperationInputDigest::V1(CanonicalOperationInputDigestV1::from_bytes(
            &encoder.finish(),
        ))
    }

    pub fn validate(&self) -> Result<(), &'static str> {
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
        PublicConfiguration {
            current: self.current.clone(),
            previous: self.previous.clone(),
            replicas: self.replicas.clone(),
        }
        .validate_for_local(self.replica.replica_id)?;
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
    pub fn validate(&self, read: AccessStatus, write: AccessStatus) -> Result<(), &'static str> {
        validate_progress_value(self.first)?;
        validate_progress_value(self.last)?;
        if self.first == -1 || self.last == -1 || self.first > self.last {
            return Err("completed public progress must be a valid ordered range");
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PublicV1OperationCompletion {
    pub operation_id: OperationId,
    pub revision: u64,
    pub input_digest: CanonicalOperationInputDigest,
    pub replay_disposition: PublicOperationReplayDisposition,
    pub result: PublicV1OperationResult,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PublicV1ExtensionSlot {
    pub version: u16,
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
    pub progress: PublicProgress,
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
        self.progress
            .validate(self.read_status, self.write_status)?;
        let mut operations = std::collections::BTreeSet::new();
        for completion in &self.completed_operations {
            completion.input_digest.validate()?;
            if completion.operation_id.is_empty()
                || completion.revision == 0
                || !operations.insert(&completion.operation_id)
            {
                return Err("public-v1 operation completion is incomplete or duplicated");
            }
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
    #[serde(default)]
    pub completed_operations: Vec<PublicV1AcceptedOperationCompletion>,
    #[serde(default)]
    pub extensions: PublicV1Extensions,
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
        self.option(build.authorization.as_ref(), |encoder, authorization| {
            encoder.authorization(authorization);
        });
    }
}
