//! Public V1 operation identities and admission inputs.

use crate::protocol::types::{
    AccessStatus, ConfigurationDescriptor, ConfigurationId, Epoch, FaultType, HistoryContext,
    OperationId, PodUid, ProcessSessionEndpoint, ProcessSessionId, ProcessSessionRenewalReason,
    PvcUid, ReplicaId, ReplicaIdentity, ReplicaRole, ReplicaStorageIdentity,
    ReplicaStorageIncarnationContext, ReplicatorCreationId, ResourceUid, derive_data_loss_history,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub const PUBLIC_V1_PROTOCOL_VERSION: u32 = 11;
pub(crate) const PUBLIC_V1_PROTOCOL_REGISTRATIONS: &[&str] = &["PUBLIC_V1_PROTOCOL_VERSION"];
pub const INVALID_LSN: i64 = -1;

#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "camelCase")]
pub struct PublicV1Identity {
    pub protocol_version: u32,
    pub generation: u64,
}

impl PublicV1Identity {
    pub const fn new(generation: u64) -> Self {
        Self {
            protocol_version: PUBLIC_V1_PROTOCOL_VERSION,
            generation,
        }
    }

    pub const fn is_public_v1(&self) -> bool {
        self.protocol_version == PUBLIC_V1_PROTOCOL_VERSION && self.generation > 0
    }

    pub const fn is_valid(&self) -> bool {
        self.is_public_v1()
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
pub struct PublicV1LifecycleBinding {
    pub identity: PublicV1Identity,
    pub resource_uid: ResourceUid,
    pub spec_generation: u64,
    pub state_persistence: StatePersistence,
}

impl PublicV1LifecycleBinding {
    pub fn validate(&self) -> Result<(), &'static str> {
        if !self.identity.is_valid() {
            return Err("invalid Public V1 identity");
        }
        if self.resource_uid.is_empty() {
            return Err("Public V1 lifecycle resource UID is empty");
        }
        if self.spec_generation == 0 {
            return Err("Public V1 lifecycle spec generation must be positive");
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
    pub binding: PublicV1LifecycleBinding,
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
                protocol_version = self.binding.identity.protocol_version,
                preview_generation = self.binding.identity.generation,
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
    pub identity: PublicV1Identity,
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
        if !self.identity.is_valid() {
            return Err("invalid Public V1 identity");
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
            if let Some(recovery) = &input.data_loss_recovery
                && (recovery.attempt.process.process_session_id != self.process_session_id
                    || recovery.attempt.attempt_id != self.operation_id
                    || recovery.attempt.process.attempt_revision != self.revision
                    || self.replay_disposition
                        != PublicOperationReplayDisposition::DataLoss(
                            recovery.authorization.replay_disposition,
                        ))
            {
                return Err("data-loss attempt session or replay differs from captured operation");
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
        self.validate_descriptors()?;
        let members = std::iter::once(&self.current)
            .chain(self.previous.iter())
            .flat_map(|configuration| configuration.members.iter())
            .collect::<Vec<_>>();
        let mut identities = std::collections::BTreeSet::new();
        let mut successor = None;
        for replica in &self.replicas {
            replica.validate()?;
            if !identities.insert(&replica.identity)
                || replica.build_id != OperationId::default()
                || !members.iter().any(|member| {
                    member.identity == replica.identity && member.role == replica.role
                })
                || !matches!(
                    replica.role,
                    ReplicaRole::Primary | ReplicaRole::ActiveSecondary
                )
            {
                return Err(
                    "public configuration contains an idle, build, non-member, or duplicate peer",
                );
            }
            if replica.must_catchup && successor.replace(&replica.identity).is_some() {
                return Err("public configuration contains multiple must-catchup successors");
            }
        }
        let member_identities = members
            .iter()
            .map(|member| &member.identity)
            .collect::<std::collections::BTreeSet<_>>();
        if member_identities.len() != self.replicas.len().saturating_add(1) {
            return Err("public configuration does not describe every exact remote peer");
        }
        Ok(())
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
        let expected_ready_secondaries = std::iter::once(&self.current)
            .chain(self.previous.iter())
            .flat_map(|configuration| configuration.members.iter())
            .filter(|member| {
                member.role == ReplicaRole::ActiveSecondary
                    && member.identity.replica_id != local_replica_id
            })
            .map(|member| &member.identity)
            .collect::<std::collections::BTreeSet<_>>();
        let mut identities = std::collections::BTreeSet::new();
        let mut successor = None;
        for replica in &self.replicas {
            replica.validate_for_configuration(&self.current, self.previous.as_ref())?;
            if !identities.insert(&replica.identity)
                || replica.identity.replica_id == local_replica_id
                || replica.role != ReplicaRole::ActiveSecondary
                || replica.build_id != OperationId::default()
                || !expected_ready_secondaries.contains(&replica.identity)
            {
                return Err(
                    "public configuration contains a local, idle, build, or duplicate peer",
                );
            }
            if replica.must_catchup && successor.replace(&replica.identity).is_some() {
                return Err("public configuration contains multiple must-catchup successors");
            }
        }
        if identities != expected_ready_secondaries {
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
    pub storage_incarnation: Option<ReplicaStorageIncarnationContext>,
    pub role: ReplicaRole,
    pub current_progress: i64,
    pub catch_up_capability: i64,
    pub must_catchup: bool,
}

impl PublicReplicaDescriptor {
    pub fn validate(&self) -> Result<(), &'static str> {
        validate_exact_replica_identity(&self.identity)?;
        if self.replication_address.is_empty() || self.process_session_id.is_empty() {
            return Err("public replica descriptor lacks an exact address or process session");
        }
        let storage = self
            .storage_incarnation
            .as_ref()
            .ok_or("public replica descriptor lacks storage incarnation")?;
        storage.validate()?;
        if storage.replica != self.identity {
            return Err("public replica descriptor storage differs from its identity");
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
        Ok(())
    }

    fn validate_for_configuration(
        &self,
        current: &ConfigurationDescriptor,
        previous: Option<&ConfigurationDescriptor>,
    ) -> Result<(), &'static str> {
        self.validate()?;
        if std::iter::once(current)
            .chain(previous)
            .any(|configuration| {
                configuration.members.iter().any(|member| {
                    member.identity == self.identity && member.role == ReplicaRole::ActiveSecondary
                })
            })
        {
            Ok(())
        } else {
            Err("public replica descriptor is not an active secondary in PC or CC")
        }
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PublicBuildAttemptReference {
    pub resource_uid: ResourceUid,
    pub epoch: Epoch,
    pub build_id: OperationId,
    pub source: ReplicaIdentity,
    pub source_storage: ReplicaStorageIncarnationContext,
    pub source_session: ProcessSessionId,
    pub target: ReplicaIdentity,
    pub target_storage: ReplicaStorageIncarnationContext,
    pub target_session: ProcessSessionId,
}

impl PublicBuildAttemptReference {
    pub(crate) fn validate(&self) -> Result<(), &'static str> {
        if self.resource_uid.is_empty()
            || self.build_id.is_empty()
            || self.source_session.is_empty()
            || self.target_session.is_empty()
            || self.epoch.data_loss_number < 0
            || self.epoch.configuration_number < 0
        {
            return Err("public build attempt reference is incomplete");
        }
        validate_exact_replica_identity(&self.source)?;
        validate_exact_replica_identity(&self.target)?;
        self.source_storage.validate()?;
        self.target_storage.validate()?;
        if self.source_storage.replica != self.source || self.target_storage.replica != self.target
        {
            return Err("public build storage differs from its replica identity");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PublicInboundBuildAuthority {
    pub operation_id: OperationId,
    pub operation_revision: u64,
    pub attempt: PublicBuildAttemptReference,
    pub history: HistoryContext,
    pub current_configuration_id: ConfigurationId,
    pub target_address: String,
}

impl PublicInboundBuildAuthority {
    pub fn validate(&self) -> Result<(), &'static str> {
        self.attempt.validate()?;
        self.history.validate()?;
        if self.operation_id.is_empty()
            || self.operation_revision == 0
            || self.current_configuration_id.is_empty()
            || self.target_address.is_empty()
        {
            return Err("inbound build authority lacks configuration or target address");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PublicInboundBuildReceipt {
    pub authority: PublicInboundBuildAuthority,
    pub final_sequence: u64,
    pub replication_boundary_lsn: i64,
    pub catch_up_boundary_lsn: i64,
    pub durable_lsn: i64,
}

impl PublicInboundBuildReceipt {
    pub fn validate(&self) -> Result<(), &'static str> {
        self.authority.validate()?;
        if self.replication_boundary_lsn < 0
            || self.catch_up_boundary_lsn < self.replication_boundary_lsn
            || self.durable_lsn < self.catch_up_boundary_lsn
        {
            return Err("inbound build receipt has inconsistent durable progress");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PublicBuildInput {
    pub attempt: OperationId,
    pub epoch: Epoch,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_configuration_id: Option<ConfigurationId>,
    pub replica: ReplicaIdentity,
    pub target_storage: ReplicaStorageIncarnationContext,
    pub process_session_id: ProcessSessionId,
    pub replication_address: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub history: Option<HistoryContext>,
}

impl PublicBuildInput {
    #[cfg_attr(not(feature = "testing"), allow(dead_code))]
    pub(crate) fn same_attempt_as(&self, other: &Self) -> bool {
        self.attempt == other.attempt
            && self.epoch == other.epoch
            && self.current_configuration_id == other.current_configuration_id
            && self.replica == other.replica
            && self.target_storage == other.target_storage
            && self.process_session_id == other.process_session_id
            && self.replication_address == other.replication_address
            && self.history == other.history
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
        controller_composed: bool,
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

    #[cfg_attr(not(feature = "testing"), allow(dead_code))]
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
            }
            Self::Open { .. } | Self::Progress { .. } | Self::Close | Self::Abort => true,
        };
        if valid {
            Ok(())
        } else {
            Err("ordinary public-v1 operation changed the history data-loss number")
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
                controller_composed: _,
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
                    || build.epoch.data_loss_number < 0
                    || build.epoch.configuration_number < 0
                    || build.process_session_id.is_empty()
                    || build.replication_address.is_empty()
                {
                    return Err("incomplete exact build identity");
                }
                build.target_storage.validate()?;
                if build.target_storage.replica != build.replica {
                    return Err("build target storage differs from target replica");
                }
                let history = build
                    .history
                    .as_ref()
                    .ok_or("public-v1 build lacks exact history")?;
                history.validate()?;
                if build.epoch.data_loss_number != history.data_loss_number {
                    return Err("public-v1 build epoch differs from exact history");
                }
                if matches!(self, Self::Build(_)) {
                    if build
                        .current_configuration_id
                        .as_ref()
                        .is_none_or(ConfigurationId::is_empty)
                    {
                        return Err("public-v1 build lacks exact current configuration");
                    }
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
                controller_composed,
            } => {
                encoder.u8(7);
                encoder.public_configuration(starting);
                encoder.public_configuration(refreshed);
                encoder.epoch(*epoch);
                encoder.replica_role(*handoff);
                encoder.bool(*catch_up_specific_quorum);
                encoder.catch_up_mode(*mode);
                encoder.bool(*controller_composed);
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

        let replacement_required = self.reason != ProcessSessionRenewalReason::PeerWithdrawal;
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
        report.validate()?;
        let replacement = self
            .replacement
            .as_ref()
            .ok_or("peer withdrawal has no replacement session")?;
        if !report.identity.is_public_v1()
            || report.resource_uid != self.resource_uid
            || report.report_sequence == 0
            || report.replica != replacement.identity
            || report.storage_incarnation != replacement.storage_incarnation
            || report.process_session_id != replacement.process_session_id
            || report.replicator_creation_id != replacement.replicator_creation_id
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
        identity: &PublicV1Identity,
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
        configuration.validate_for_local(self.replica.replica_id)?;
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
        match (self.possible_data_loss, &self.data_loss_recovery) {
            (PossibleDataLossIntent::Possible, Some(recovery)) => {
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
            (PossibleDataLossIntent::Possible, None) => {
                return Err("public-v1 data loss requires typed authorization");
            }
            (PossibleDataLossIntent::NotPossible, Some(_)) => {
                return Err("ordinary lifecycle cannot carry data-loss authorization");
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
    pub epoch: Epoch,
    pub target: ReplicaIdentity,
    pub target_session: ProcessSessionId,
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
    pub identity: PublicV1Identity,
    pub epoch: Epoch,
    pub operation_id: OperationId,
    pub revision: u64,
    pub detecting_session: ProcessSessionId,
}

impl CapturedPublicOperationIdentity {
    pub(crate) fn validate(&self) -> Result<(), &'static str> {
        if self.resource_uid.is_empty()
            || !self.identity.is_public_v1()
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
        build: Box<PublicBuildAttemptReference>,
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
                operation, build, ..
            } => {
                operation.validate()?;
                build.validate()?;
                if build.resource_uid != operation.resource_uid
                    || build.epoch != operation.epoch
                    || build.build_id != operation.operation_id
                {
                    return Err("remote rebuild attempt differs from captured operation");
                }
            }
        }
        Ok(())
    }

    fn validate_for_report(
        &self,
        completion_id: &OperationId,
        completion_revision: u64,
        report_identity: &PublicV1Identity,
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
            || &operation.identity != report_identity
            || &operation.resource_uid != report_resource_uid
            || &operation.detecting_session != report_session
        {
            return Err("rebuild evidence differs from its containing completion or report");
        }
        match self {
            Self::LocalProvider { local_replica, .. } if local_replica != report_replica => {
                Err("local rebuild evidence differs from the reporting replica")
            }
            Self::RemoteBuildTarget { build, .. }
                if &build.source != report_replica || &build.source_session != report_session =>
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
    pub result: PublicV1OperationResult,
}

impl PublicV1OperationCompletion {
    pub fn validate_for_status(
        &self,
        identity: &PublicV1Identity,
        history: &HistoryContext,
    ) -> Result<(), &'static str> {
        if self.operation_id.is_empty()
            || self.revision == 0
            || self.process_session_id.is_empty()
            || &self.history != history
            || self.history.protocol_generation.value() != identity.generation
        {
            return Err("public-v1 operation completion status identity is incomplete");
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
            PublicV1OperationResult::RebuildRequired(rebuild) => rebuild.validate()?,
            _ => {}
        }
        Ok(())
    }

    fn validate(
        &self,
        report_identity: &PublicV1Identity,
        report_resource_uid: &ResourceUid,
        report_replica: &ReplicaIdentity,
        report_session: &ProcessSessionId,
    ) -> Result<(), &'static str> {
        self.validate_for_status(report_identity, &self.history)?;
        if &self.process_session_id != report_session
            || self.history.resource_uid != *report_resource_uid
            || self.storage_incarnation.replica != *report_replica
        {
            return Err(
                "public-v1 operation completion differs from its report history, storage, or session",
            );
        }
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct PublicV1Extensions {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub history_recovery: Option<PublicV1ExtensionSlot>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub public_build_evidence: Option<PublicV1ExtensionSlot>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PublicV1AgentReport {
    pub identity: PublicV1Identity,
    pub resource_uid: ResourceUid,
    pub replica: ReplicaIdentity,
    pub process_session_id: ProcessSessionId,
    pub replicator_creation_id: ReplicatorCreationId,
    pub report_sequence: u64,
    pub role: ReplicaRole,
    pub read_status: AccessStatus,
    pub write_status: AccessStatus,
    pub history: HistoryContext,
    pub storage_incarnation: ReplicaStorageIncarnationContext,
    pub progress: PublicProgress,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_location: Option<ServiceLocation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_loss_recovery: Option<PublicDataLossRecoveryStatus>,
    #[serde(default)]
    pub completed_operations: Vec<PublicV1OperationCompletion>,
    #[serde(default)]
    pub extensions: PublicV1Extensions,
}

impl PublicV1AgentReport {
    fn is_access_closed_replacement_observation(&self) -> bool {
        self.role == ReplicaRole::None
            && self.read_status != AccessStatus::Granted
            && self.write_status != AccessStatus::Granted
            && self.data_loss_recovery.is_none()
    }

    pub fn validate(&self) -> Result<(), &'static str> {
        if !self.identity.is_public_v1()
            || self.resource_uid.is_empty()
            || self.process_session_id.is_empty()
            || self.replicator_creation_id.is_empty()
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
                        | PublicDataLossRecoveryStage::Failed
                        | PublicDataLossRecoveryStage::ReplacementRequired
                        | PublicDataLossRecoveryStage::Retired
                )
            })
            && !self.is_access_closed_replacement_observation()
        {
            return Err(
                "recovery progress requires nonterminal recovery authority or an access-closed replacement observation",
            );
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
        if let Some(location) = &self.service_location
            && (self.role != ReplicaRole::Primary
                || self.write_status != AccessStatus::Granted
                || location.identity != self.identity
                || location.resource_uid != self.resource_uid
                || location.replica != self.replica
                || location.process_session_id != self.process_session_id
                || location.operation_id.is_empty()
                || location.revision == 0
                || location.address.is_empty()
                || !self.completed_operations.iter().any(|completion| {
                    completion.operation_id == location.operation_id
                        && completion.revision == location.revision
                        && completion.process_session_id == location.process_session_id
                        && matches!(
                            completion.call_kind,
                            PublicV1CallKind::InitialPrimary
                                | PublicV1CallKind::FailoverPromotion
                                | PublicV1CallKind::Role
                                | PublicV1CallKind::Swap
                        )
                        && completion.result == PublicV1OperationResult::Succeeded
                }))
        {
            return Err("public-v1 service location lacks exact role authority");
        }
        for extension in [
            self.extensions.history_recovery.as_ref(),
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum PublicV1AcceptedRebuildKind {
    LocalProvider,
    RemoteBuildTarget,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PublicV1AcceptedRebuildRequired {
    pub kind: PublicV1AcceptedRebuildKind,
    pub reason: ReplicaRebuildReason,
    pub operation: CapturedPublicOperationIdentity,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_replica: Option<ReplicaIdentity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_replica: Option<ReplicaIdentity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_storage: Option<PvcUid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build: Option<PublicBuildAttemptReference>,
}

impl PublicV1AcceptedRebuildRequired {
    fn from_rebuild(rebuild: &CanonicalReplicaRebuildRequired) -> Self {
        match rebuild {
            CanonicalReplicaRebuildRequired::LocalProvider {
                reason,
                provider,
                operation,
                local_replica,
                local_storage,
            } => Self {
                kind: PublicV1AcceptedRebuildKind::LocalProvider,
                reason: *reason,
                operation: operation.clone(),
                provider_replica: Some(provider.replica.clone()),
                local_replica: Some(local_replica.clone()),
                local_storage: Some(local_storage.clone()),
                build: None,
            },
            CanonicalReplicaRebuildRequired::RemoteBuildTarget {
                reason,
                operation,
                build,
            } => Self {
                kind: PublicV1AcceptedRebuildKind::RemoteBuildTarget,
                reason: *reason,
                operation: operation.clone(),
                provider_replica: None,
                local_replica: None,
                local_storage: None,
                build: Some(build.as_ref().clone()),
            },
        }
    }

    fn validate(&self) -> Result<(), &'static str> {
        self.operation.validate()?;
        match self.kind {
            PublicV1AcceptedRebuildKind::LocalProvider => {
                let provider = self
                    .provider_replica
                    .as_ref()
                    .ok_or("accepted local rebuild lacks provider identity")?;
                let local = self
                    .local_replica
                    .as_ref()
                    .ok_or("accepted local rebuild lacks local replica")?;
                if provider != local
                    || self.local_storage.as_ref().is_none_or(PvcUid::is_empty)
                    || self.build.is_some()
                {
                    return Err("accepted local rebuild shape is inconsistent");
                }
            }
            PublicV1AcceptedRebuildKind::RemoteBuildTarget => {
                let build = self
                    .build
                    .as_ref()
                    .ok_or("accepted remote rebuild lacks exact build identity")?;
                build.validate()?;
                if self.provider_replica.is_some()
                    || self.local_replica.is_some()
                    || self.local_storage.is_some()
                    || build.resource_uid != self.operation.resource_uid
                    || build.epoch != self.operation.epoch
                    || build.build_id != self.operation.operation_id
                {
                    return Err("accepted remote rebuild shape is inconsistent");
                }
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PublicV1AcceptedOperationCompletion {
    pub operation_id: OperationId,
    pub revision: u64,
    pub call_kind: PublicV1CallKind,
    pub process_session_id: ProcessSessionId,
    pub input_digest_version: u16,
    pub input_digest_sha256: String,
    pub replay_disposition: PublicV1AcceptedReplayDisposition,
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
    pub result: PublicV1AcceptedResult,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_detail: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rebuild_required: Option<PublicV1AcceptedRebuildRequired>,
}

impl PublicV1AcceptedOperationCompletion {
    pub fn from_completion(completion: &PublicV1OperationCompletion) -> Self {
        let (result, failure_detail, rebuild_required) = match &completion.result {
            PublicV1OperationResult::Succeeded => (PublicV1AcceptedResult::Succeeded, None, None),
            PublicV1OperationResult::Failed(detail) => {
                (PublicV1AcceptedResult::Failed, Some(detail.clone()), None)
            }
            PublicV1OperationResult::Ambiguous(detail) => (
                PublicV1AcceptedResult::Ambiguous,
                Some(detail.clone()),
                None,
            ),
            PublicV1OperationResult::Cancelled => (PublicV1AcceptedResult::Cancelled, None, None),
            PublicV1OperationResult::RebuildRequired(rebuild) => (
                PublicV1AcceptedResult::RebuildRequired,
                None,
                Some(PublicV1AcceptedRebuildRequired::from_rebuild(rebuild)),
            ),
        };
        Self {
            operation_id: completion.operation_id.clone(),
            revision: completion.revision,
            call_kind: completion.call_kind,
            process_session_id: completion.process_session_id.clone(),
            input_digest_version: completion.input_digest.version(),
            input_digest_sha256: completion.input_digest.sha256().to_string(),
            replay_disposition: completion.replay_disposition.into(),
            history: completion.history.clone(),
            storage_incarnation: completion.storage_incarnation.clone(),
            previous_configuration_id: completion.previous_configuration_id.clone(),
            current_configuration_id: completion.current_configuration_id.clone(),
            catch_up_mode: completion.catch_up_mode,
            catch_up_specific_quorum: completion.catch_up_specific_quorum,
            result,
            failure_detail,
            rebuild_required,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PublicV1AcceptedStatus {
    pub identity: PublicV1Identity,
    pub history_initialization_id: crate::protocol::types::HistoryInitializationId,
    pub history: HistoryContext,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_loss_recovery: Option<PublicDataLossRecoveryStatus>,
    #[serde(default)]
    #[schemars(length(max = 64))]
    pub completed_operations: Vec<PublicV1AcceptedOperationCompletion>,
    #[serde(default)]
    pub extensions: PublicV1Extensions,
}

impl PublicV1AcceptedStatus {
    pub fn validate_evidence(&self) -> Result<(), &'static str> {
        if !self.identity.is_public_v1()
            || self.completed_operations.len() > 64
            || self.history.protocol_generation.value() != self.identity.generation
        {
            return Err("public-v1 accepted evidence identity or bound is invalid");
        }
        self.history.validate()?;
        let mut operations = std::collections::BTreeSet::new();
        for completion in &self.completed_operations {
            if completion.operation_id.is_empty()
                || completion.revision == 0
                || completion.process_session_id.is_empty()
                || completion.history != self.history
                || completion.input_digest_version != 1
                || completion.input_digest_sha256.len() != 64
                || completion.failure_detail.is_some()
                    != matches!(
                        completion.result,
                        PublicV1AcceptedResult::Failed | PublicV1AcceptedResult::Ambiguous
                    )
                || completion.rebuild_required.is_some()
                    != (completion.result == PublicV1AcceptedResult::RebuildRequired)
            {
                return Err("public-v1 accepted completion is incomplete");
            }
            completion.history.validate()?;
            completion.storage_incarnation.validate()?;
            if let Some(rebuild) = &completion.rebuild_required {
                rebuild.validate()?;
                if rebuild.operation.operation_id != completion.operation_id
                    || rebuild.operation.revision != completion.revision
                    || rebuild.operation.identity != self.identity
                    || rebuild.operation.resource_uid != self.history.resource_uid
                    || rebuild.operation.detecting_session != completion.process_session_id
                {
                    return Err("accepted rebuild differs from its completion");
                }
            }
            if !operations.insert(&completion.operation_id) {
                return Err("public-v1 accepted completions contain a duplicate operation");
            }
        }
        Ok(())
    }

    pub fn is_evidence_successor_of(&self, previous: &Self) -> bool {
        if self.identity != previous.identity
            || self.history_initialization_id != previous.history_initialization_id
            || self.history != previous.history
            || self.data_loss_recovery != previous.data_loss_recovery
            || self.extensions != previous.extensions
            || self.completed_operations.len() > previous.completed_operations.len() + 1
        {
            return false;
        }
        previous.completed_operations.iter().all(|completion| {
            self.completed_operations
                .iter()
                .any(|next| next == completion)
        })
    }
}

/// Application-owned opaque address, never a Replicator transport endpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ServiceLocation {
    pub identity: PublicV1Identity,
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
    pub identity: PublicV1Identity,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binding: Option<PublicV1LifecycleBinding>,
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
        self.option(replica.storage_incarnation.as_ref(), |encoder, storage| {
            encoder.storage_incarnation(storage);
        });
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

    fn public_build(&mut self, build: &PublicBuildInput) {
        self.string(build.attempt.as_str());
        self.epoch(build.epoch);
        self.option(
            build.current_configuration_id.as_ref(),
            |encoder, configuration| {
                encoder.string(configuration.as_str());
            },
        );
        self.replica_identity(&build.replica);
        self.storage_incarnation(&build.target_storage);
        self.string(build.process_session_id.as_str());
        self.string(&build.replication_address);
        self.option(build.history.as_ref(), |encoder, history| {
            encoder.history(history);
        });
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
