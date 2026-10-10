//! Dormant public-operation preview identities and admission inputs.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::protocol::types::{
    ConfigurationDescriptor, Epoch, OperationId, ProcessSessionId, ReplicaId, ReplicaIdentity,
    ReplicaRole, ResourceUid,
};

pub const PUBLIC_OPERATION_PREVIEW_PROTOCOL_VERSION: u32 = 10;

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

    pub const fn is_valid(&self) -> bool {
        self.protocol_version == PUBLIC_OPERATION_PREVIEW_PROTOCOL_VERSION && self.generation > 0
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PublicOperationIntent {
    pub preview: PublicOperationPreviewIdentity,
    pub operation_id: OperationId,
    pub revision: u64,
    pub process_session_id: ProcessSessionId,
    pub class: PublicOperationClass,
    pub input_digest: String,
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
        if self.input_digest.is_empty() {
            return Err("public-operation input digest is empty");
        }
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
        }
        if let Some(program) = &self.program {
            program.validate(&self.class, &self.operation_id)?;
            if self.input_digest != program.digest() {
                return Err("program digest does not match frozen input");
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
}

impl PublicConfiguration {
    fn validate(&self) -> Result<(), &'static str> {
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
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PublicBuildInput {
    pub attempt: OperationId,
    pub replica: ReplicaIdentity,
    pub process_session_id: ProcessSessionId,
    pub replication_address: String,
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
        mode: PublicCatchUpMode,
    },
    Build(PublicBuildInput),
    Remove(PublicBuildInput),
    Close,
    Abort,
}

impl PublicOperationProgram {
    pub fn digest(&self) -> String {
        use sha2::Digest;
        sha2::Sha256::digest(format!("{self:?}").as_bytes())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }

    fn validate(&self, class: &PublicOperationClass, id: &OperationId) -> Result<(), &'static str> {
        let expected = match self {
            Self::Swap {
                starting,
                refreshed,
                epoch,
                handoff,
                ..
            } => {
                starting.validate()?;
                refreshed.validate()?;
                if starting.current.epoch >= *epoch
                    || refreshed.current.epoch != *epoch
                    || !matches!(handoff, ReplicaRole::ActiveSecondary | ReplicaRole::None)
                {
                    return Err("invalid swap epoch or handoff");
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
}

impl PublicLifecycleInput {
    pub fn digest(&self) -> String {
        use sha2::Digest;
        sha2::Sha256::digest(format!("{self:?}").as_bytes())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
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
    pub resource_uid: ResourceUid,
    pub replica: ReplicaIdentity,
    pub process_session_id: ProcessSessionId,
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
