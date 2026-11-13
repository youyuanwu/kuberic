//! Serializable replacement-state fixtures.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::protocol::types::{
    EffectivePolicy, FaultType, InitializationId, LoadMetric, PodUid, PvcUid, ReplicaIdentity,
    ResourceUid,
};

pub const SCHEMA_VERSION: u32 = 6;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StorageIdentity {
    pub schema_version: u32,
    pub resource_uid: ResourceUid,
    pub pod_uid: PodUid,
    pub pvc_uid: PvcUid,
    pub initialization_id: InitializationId,
    pub local_identity: ReplicaIdentity,
    pub effective_policy: EffectivePolicy,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApplicationStorageBinding {
    pub paths: BTreeMap<String, PathBuf>,
    pub initializing: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentState {
    pub identity: StorageIdentity,
    #[serde(default)]
    pub application_storage: Option<ApplicationStorageBinding>,
    #[serde(default)]
    pub load_metrics: Vec<LoadMetric>,
    #[serde(default)]
    pub reported_fault: Option<FaultType>,
}

impl AgentState {
    pub fn new(identity: StorageIdentity) -> Self {
        Self {
            identity,
            application_storage: None,
            load_metrics: Vec::new(),
            reported_fault: None,
        }
    }
}
