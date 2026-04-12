use std::collections::BTreeMap;

use async_trait::async_trait;
use k8s_openapi::api::core::v1::{PersistentVolumeClaim, Pod, Service};

use kuberic_core::driver::ReplicaHandle;
use kuberic_core::types::ReplicaId;

use crate::crd::{KubericSetSpec, KubericSetStatus};

/// Abstraction over Kubernetes API and replica creation.
/// Real impl uses kube::Client; test impl uses in-memory state.
#[async_trait]
pub trait ClusterApi: Send + Sync {
    /// List pods matching the label selector.
    async fn list_pods(&self, namespace: &str, selector: &str) -> Result<Vec<Pod>, String>;

    /// Create a pod.
    async fn create_pod(&self, namespace: &str, pod: &Pod) -> Result<(), String>;

    /// Delete a pod by name.
    async fn delete_pod(&self, namespace: &str, pod_name: &str) -> Result<(), String>;

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
        status: &KubericSetStatus,
    ) -> Result<(), String>;

    /// Create a ReplicaHandle for a pod (gRPC or in-process).
    async fn create_replica_handle(
        &self,
        replica_id: ReplicaId,
        pod: &Pod,
        spec: &KubericSetSpec,
    ) -> Result<Box<dyn ReplicaHandle>, String>;

    // -- PVC management --

    /// Get a PVC by name.
    async fn get_pvc(&self, namespace: &str, name: &str) -> Result<PersistentVolumeClaim, String>;

    /// Create a PVC.
    async fn create_pvc(&self, namespace: &str, pvc: &PersistentVolumeClaim) -> Result<(), String>;

    /// List PVCs matching the label selector.
    async fn list_pvcs(
        &self,
        namespace: &str,
        selector: &str,
    ) -> Result<Vec<PersistentVolumeClaim>, String>;

    /// Delete a PVC by name.
    async fn delete_pvc(&self, namespace: &str, name: &str) -> Result<(), String>;

    // -- Service management --

    /// Get a Service by name.
    async fn get_service(&self, namespace: &str, name: &str) -> Result<Service, String>;

    /// Create a Service.
    async fn create_service(&self, namespace: &str, svc: &Service) -> Result<(), String>;

    /// Delete a Service by name.
    async fn delete_service(&self, namespace: &str, name: &str) -> Result<(), String>;
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

    async fn delete_pod(&self, namespace: &str, pod_name: &str) -> Result<(), String> {
        let api: kube::Api<Pod> = kube::Api::namespaced(self.client.clone(), namespace);
        match api
            .delete(pod_name, &kube::api::DeleteParams::default())
            .await
        {
            Ok(_) => Ok(()),
            Err(kube::Error::Api(ae)) if ae.code == 404 => Ok(()), // already gone
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
            &kube::api::PatchParams::apply("kuberic-operator"),
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
        status: &KubericSetStatus,
    ) -> Result<(), String> {
        let api: kube::Api<crate::crd::KubericSet> =
            kube::Api::namespaced(self.client.clone(), namespace);
        let patch = serde_json::json!({ "status": status });
        api.patch_status(
            set_name,
            &kube::api::PatchParams::apply("kuberic-operator"),
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
        spec: &KubericSetSpec,
    ) -> Result<Box<dyn ReplicaHandle>, String> {
        let pod_ip = pod
            .status
            .as_ref()
            .and_then(|s| s.pod_ip.as_ref())
            .cloned()
            .ok_or("pod has no IP")?;

        let control_addr = format!("http://{}:{}", pod_ip, spec.control_port);
        let data_addr = format!("http://{}:{}", pod_ip, spec.data_port);

        let handle = kuberic_core::grpc::handle::GrpcReplicaHandle::connect(
            replica_id,
            control_addr,
            data_addr,
        )
        .await
        .map_err(|e| e.to_string())?;

        Ok(Box::new(handle))
    }

    async fn get_pvc(&self, namespace: &str, name: &str) -> Result<PersistentVolumeClaim, String> {
        let api: kube::Api<PersistentVolumeClaim> =
            kube::Api::namespaced(self.client.clone(), namespace);
        api.get(name).await.map_err(|e| e.to_string())
    }

    async fn create_pvc(&self, namespace: &str, pvc: &PersistentVolumeClaim) -> Result<(), String> {
        let api: kube::Api<PersistentVolumeClaim> =
            kube::Api::namespaced(self.client.clone(), namespace);
        match api.create(&kube::api::PostParams::default(), pvc).await {
            Ok(_) => Ok(()),
            Err(kube::Error::Api(ae)) if ae.code == 409 => Ok(()), // already exists
            Err(e) => Err(e.to_string()),
        }
    }

    async fn list_pvcs(
        &self,
        namespace: &str,
        selector: &str,
    ) -> Result<Vec<PersistentVolumeClaim>, String> {
        let api: kube::Api<PersistentVolumeClaim> =
            kube::Api::namespaced(self.client.clone(), namespace);
        let params = kube::api::ListParams::default().labels(selector);
        api.list(&params)
            .await
            .map(|list| list.items)
            .map_err(|e| e.to_string())
    }

    async fn delete_pvc(&self, namespace: &str, name: &str) -> Result<(), String> {
        let api: kube::Api<PersistentVolumeClaim> =
            kube::Api::namespaced(self.client.clone(), namespace);
        match api.delete(name, &kube::api::DeleteParams::default()).await {
            Ok(_) => Ok(()),
            Err(kube::Error::Api(ae)) if ae.code == 404 => Ok(()),
            Err(e) => Err(e.to_string()),
        }
    }

    async fn get_service(&self, namespace: &str, name: &str) -> Result<Service, String> {
        let api: kube::Api<Service> = kube::Api::namespaced(self.client.clone(), namespace);
        api.get(name).await.map_err(|e| e.to_string())
    }

    async fn create_service(&self, namespace: &str, svc: &Service) -> Result<(), String> {
        let api: kube::Api<Service> = kube::Api::namespaced(self.client.clone(), namespace);
        match api.create(&kube::api::PostParams::default(), svc).await {
            Ok(_) => Ok(()),
            Err(kube::Error::Api(ae)) if ae.code == 409 => Ok(()), // already exists
            Err(e) => Err(e.to_string()),
        }
    }

    async fn delete_service(&self, namespace: &str, name: &str) -> Result<(), String> {
        let api: kube::Api<Service> = kube::Api::namespaced(self.client.clone(), namespace);
        match api.delete(name, &kube::api::DeleteParams::default()).await {
            Ok(_) => Ok(()),
            Err(kube::Error::Api(ae)) if ae.code == 404 => Ok(()),
            Err(e) => Err(e.to_string()),
        }
    }
}
