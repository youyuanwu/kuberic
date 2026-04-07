use std::collections::BTreeMap;

use async_trait::async_trait;
use k8s_openapi::api::core::v1::Pod;

use kubelicate_core::driver::ReplicaHandle;
use kubelicate_core::types::ReplicaId;

use crate::crd::{KubelicateSetSpec, KubelicateSetStatus};

/// Abstraction over Kubernetes API and replica creation.
/// Real impl uses kube::Client; test impl uses in-memory state.
#[async_trait]
pub trait ClusterApi: Send + Sync {
    /// List pods matching the label selector.
    async fn list_pods(&self, namespace: &str, selector: &str) -> Result<Vec<Pod>, String>;

    /// Create a pod.
    async fn create_pod(&self, namespace: &str, pod: &Pod) -> Result<(), String>;

    /// Update a pod's labels.
    async fn patch_pod_labels(
        &self,
        namespace: &str,
        pod_name: &str,
        labels: BTreeMap<String, String>,
    ) -> Result<(), String>;

    /// Patch the CRD status.
    async fn patch_set_status(
        &self,
        namespace: &str,
        set_name: &str,
        status: &KubelicateSetStatus,
    ) -> Result<(), String>;

    /// Create a ReplicaHandle for a pod (gRPC or in-process).
    async fn create_replica_handle(
        &self,
        replica_id: ReplicaId,
        pod: &Pod,
        spec: &KubelicateSetSpec,
    ) -> Result<Box<dyn ReplicaHandle>, String>;
}

// ---------------------------------------------------------------------------
// Real implementation (kube::Client)
// ---------------------------------------------------------------------------

pub struct KubeClusterApi {
    pub client: kube::Client,
}

#[async_trait]
impl ClusterApi for KubeClusterApi {
    async fn list_pods(&self, namespace: &str, selector: &str) -> Result<Vec<Pod>, String> {
        let api: kube::Api<Pod> = kube::Api::namespaced(self.client.clone(), namespace);
        let params = kube::api::ListParams::default().labels(selector);
        api.list(&params)
            .await
            .map(|list| list.items)
            .map_err(|e| e.to_string())
    }

    async fn create_pod(&self, namespace: &str, pod: &Pod) -> Result<(), String> {
        let api: kube::Api<Pod> = kube::Api::namespaced(self.client.clone(), namespace);
        match api.create(&kube::api::PostParams::default(), pod).await {
            Ok(_) => Ok(()),
            Err(kube::Error::Api(ae)) if ae.code == 409 => Ok(()), // already exists
            Err(e) => Err(e.to_string()),
        }
    }

    async fn patch_pod_labels(
        &self,
        namespace: &str,
        pod_name: &str,
        labels: BTreeMap<String, String>,
    ) -> Result<(), String> {
        let api: kube::Api<Pod> = kube::Api::namespaced(self.client.clone(), namespace);
        let patch = serde_json::json!({ "metadata": { "labels": labels } });
        api.patch(
            pod_name,
            &kube::api::PatchParams::apply("kubelicate-operator"),
            &kube::api::Patch::Merge(&patch),
        )
        .await
        .map(|_| ())
        .map_err(|e| e.to_string())
    }

    async fn patch_set_status(
        &self,
        namespace: &str,
        set_name: &str,
        status: &KubelicateSetStatus,
    ) -> Result<(), String> {
        let api: kube::Api<crate::crd::KubelicateSet> =
            kube::Api::namespaced(self.client.clone(), namespace);
        let patch = serde_json::json!({ "status": status });
        api.patch_status(
            set_name,
            &kube::api::PatchParams::apply("kubelicate-operator"),
            &kube::api::Patch::Merge(&patch),
        )
        .await
        .map(|_| ())
        .map_err(|e| e.to_string())
    }

    async fn create_replica_handle(
        &self,
        replica_id: ReplicaId,
        pod: &Pod,
        spec: &KubelicateSetSpec,
    ) -> Result<Box<dyn ReplicaHandle>, String> {
        let pod_ip = pod
            .status
            .as_ref()
            .and_then(|s| s.pod_ip.as_ref())
            .cloned()
            .ok_or("pod has no IP")?;

        let control_addr = format!("http://{}:{}", pod_ip, spec.control_port);
        let data_addr = format!("http://{}:{}", pod_ip, spec.data_port);

        let handle = kubelicate_core::grpc::handle::GrpcReplicaHandle::connect(
            replica_id,
            control_addr,
            data_addr,
        )
        .await
        .map_err(|e| e.to_string())?;

        Ok(Box::new(handle))
    }
}
