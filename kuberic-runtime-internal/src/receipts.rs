use kuberic_protocol::types::{AccessStatus, OperationId, ReplicaId};
use serde::{Deserialize, Serialize};

use crate::authority::{AdmittedAuthority, BuildSelection, DurableBuildProgress};

/// Exact native engine identity captured before a public operation begins.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativeOperationToken {
    pub authority: Option<AdmittedAuthority>,
    pub engine_session_id: String,
    pub engine_generation: u64,
}

/// Native evidence captured after the public catch-up operation succeeds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatchUpReceipt {
    pub authority: AdmittedAuthority,
    pub engine_session_id: String,
    pub engine_generation: u64,
    pub boundary_lsn: i64,
    pub current_progress: i64,
    pub committed_lsn: i64,
    pub current_configuration_quorum_progress: i64,
}

/// Native durable completion evidence for one exact selected build.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuildReceipt {
    pub selection: BuildSelection,
    pub progress: DurableBuildProgress,
    pub engine_session_id: String,
    pub engine_generation: u64,
}

/// Native proof that one ordinary idle replica has been removed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemovalReceipt {
    pub authority: AdmittedAuthority,
    pub replica_id: ReplicaId,
    pub retired_build_ids: Vec<OperationId>,
    pub engine_session_id: String,
    pub engine_generation: u64,
}

/// Native access admission/publication evidence. A preparation receipt is
/// immutable and must be supplied back to the engine for publication.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccessReceipt {
    pub authority: Option<AdmittedAuthority>,
    pub engine_session_id: String,
    pub engine_generation: u64,
    pub read: AccessStatus,
    pub write: AccessStatus,
    pub current_progress: i64,
    pub committed_lsn: i64,
    pub published: bool,
}
