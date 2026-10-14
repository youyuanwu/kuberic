use std::sync::Arc;

use crate::protocol::types::{ReplicaIdentity, ResourceUid, derive_replica_endpoint_name};

pub trait ReplicaEndpointResolver: Send + Sync {
    fn control_endpoint(&self, identity: &ReplicaIdentity) -> String;

    fn replication_endpoint(&self, identity: &ReplicaIdentity) -> String;
}

#[derive(Debug, Clone)]
pub struct KubernetesDnsResolver {
    resource_uid: ResourceUid,
    namespace: Arc<str>,
}

impl KubernetesDnsResolver {
    pub fn new(resource_uid: ResourceUid, namespace: impl Into<Arc<str>>) -> Self {
        Self {
            resource_uid,
            namespace: namespace.into(),
        }
    }

    fn host(&self, identity: &ReplicaIdentity) -> String {
        format!(
            "{}.{}.svc",
            derive_replica_endpoint_name(&self.resource_uid, identity),
            self.namespace
        )
    }
}

impl ReplicaEndpointResolver for KubernetesDnsResolver {
    fn control_endpoint(&self, identity: &ReplicaIdentity) -> String {
        format!("http://{}:50051", self.host(identity))
    }

    fn replication_endpoint(&self, identity: &ReplicaIdentity) -> String {
        format!("http://{}:50052", self.host(identity))
    }
}
