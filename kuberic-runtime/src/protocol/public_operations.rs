//! Dormant public-operation preview identities and admission inputs.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::protocol::types::{OperationId, ProcessSessionId, ReplicaId};

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
}

impl PublicOperationClass {
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Close | Self::Abort | Self::TransientFault | Self::PermanentFault
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
        Ok(())
    }
}
