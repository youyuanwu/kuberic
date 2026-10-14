use std::collections::BTreeMap;

use k8s_openapi::api::core::v1::{PersistentVolumeClaim, Pod, Secret, Service};
use kuberic_runtime::protocol::observation::ReplicaObservationKey;
#[cfg(feature = "runtime-test-bridge")]
use kuberic_runtime::protocol::types::{ReplicaCleanupIdentity, ReplicaIdentity};

use crate::crd::KubericSet;

#[derive(Debug, Clone)]
pub enum RawAgentObservation {
    Absent,
    Unavailable { message: String },
    Invalid { message: String },
    PublicV1Report(Box<kuberic_runtime::protocol::public_operations::PublicV1AgentReport>),
}

#[derive(Debug, Clone)]
pub struct RawObservation {
    pub set: KubericSet,
    pub pods: Vec<Pod>,
    pub pvcs: Vec<PersistentVolumeClaim>,
    pub services: Vec<Service>,
    pub secrets: Vec<Secret>,
    pub agents: BTreeMap<ReplicaObservationKey, RawAgentObservation>,
    #[cfg(feature = "runtime-test-bridge")]
    pub exact_resources: Vec<RawScaleDownResources>,
    pub failures: Vec<RawObservationFailure>,
    pub now_unix_seconds: i64,
}

#[cfg(feature = "runtime-test-bridge")]
#[derive(Debug, Clone)]
pub enum ExactLookup<T> {
    Present(T),
    NotFound,
    Failed(String),
}

#[cfg(feature = "runtime-test-bridge")]
#[derive(Debug, Clone)]
pub struct RawScaleDownResources {
    pub target: ReplicaIdentity,
    pub identity: ReplicaCleanupIdentity,
    pub pod: ExactLookup<Pod>,
    pub pvc: ExactLookup<PersistentVolumeClaim>,
    pub endpoint: ExactLookup<Service>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawObservationFailure {
    pub source: String,
    pub message: String,
}
