use std::collections::BTreeMap;
use std::sync::Mutex;

use async_trait::async_trait;
use k8s_openapi::api::core::v1::{Pod, PodCondition, PodStatus};
use kube::api::ObjectMeta;

use kubelicate_core::driver::ReplicaHandle;
use kubelicate_core::driver::testing::InProcessReplicaHandle;
use kubelicate_core::types::ReplicaId;

use crate::cluster_api::ClusterApi;
use crate::crd::{KubelicateSet, KubelicateSetSpec, KubelicateSetStatus, Phase};
use crate::reconciler::{ReconcileAction, ReconcilerState, reconcile_set};

/// In-memory mock for ClusterApi. Stores pods and creates InProcessReplicaHandles.
struct MockClusterApi {
    pods: Mutex<Vec<Pod>>,
    statuses: Mutex<Vec<KubelicateSetStatus>>,
}

impl MockClusterApi {
    fn new() -> Self {
        Self {
            pods: Mutex::new(Vec::new()),
            statuses: Mutex::new(Vec::new()),
        }
    }

    /// Mark all pods as ready (simulates k8s scheduling).
    fn mark_all_pods_ready(&self) {
        let mut pods = self.pods.lock().unwrap();
        for pod in pods.iter_mut() {
            pod.status = Some(PodStatus {
                conditions: Some(vec![PodCondition {
                    type_: "Ready".to_string(),
                    status: "True".to_string(),
                    ..Default::default()
                }]),
                pod_ip: Some("127.0.0.1".to_string()),
                ..Default::default()
            });
        }
    }

    /// Mark a specific pod as not ready (simulates failure).
    fn mark_pod_not_ready(&self, pod_name: &str) {
        let mut pods = self.pods.lock().unwrap();
        if let Some(pod) = pods
            .iter_mut()
            .find(|p| p.metadata.name.as_deref() == Some(pod_name))
        {
            pod.status = Some(PodStatus {
                conditions: Some(vec![PodCondition {
                    type_: "Ready".to_string(),
                    status: "False".to_string(),
                    ..Default::default()
                }]),
                ..Default::default()
            });
        }
    }

    fn last_status(&self) -> Option<KubelicateSetStatus> {
        self.statuses.lock().unwrap().last().cloned()
    }

    fn pod_count(&self) -> usize {
        self.pods.lock().unwrap().len()
    }
}

#[async_trait]
impl ClusterApi for MockClusterApi {
    async fn list_pods(&self, _namespace: &str, _selector: &str) -> Result<Vec<Pod>, String> {
        Ok(self.pods.lock().unwrap().clone())
    }

    async fn create_pod(&self, _namespace: &str, pod: &Pod) -> Result<(), String> {
        self.pods.lock().unwrap().push(pod.clone());
        Ok(())
    }

    async fn patch_pod_labels(
        &self,
        _namespace: &str,
        pod_name: &str,
        labels: BTreeMap<String, String>,
    ) -> Result<(), String> {
        let mut pods = self.pods.lock().unwrap();
        if let Some(pod) = pods
            .iter_mut()
            .find(|p| p.metadata.name.as_deref() == Some(pod_name))
        {
            let pod_labels = pod.metadata.labels.get_or_insert_with(BTreeMap::new);
            pod_labels.extend(labels);
        }
        Ok(())
    }

    async fn patch_set_status(
        &self,
        _namespace: &str,
        _set_name: &str,
        status: &KubelicateSetStatus,
    ) -> Result<(), String> {
        self.statuses.lock().unwrap().push(status.clone());
        Ok(())
    }

    async fn create_replica_handle(
        &self,
        replica_id: ReplicaId,
        _pod: &Pod,
        _spec: &KubelicateSetSpec,
    ) -> Result<Box<dyn ReplicaHandle>, String> {
        let handle = InProcessReplicaHandle::spawn(replica_id)
            .await
            .map_err(|e| e.to_string())?;
        Ok(Box::new(handle))
    }
}

fn make_set(name: &str, replicas: i32, status: Option<KubelicateSetStatus>) -> KubelicateSet {
    KubelicateSet {
        metadata: ObjectMeta {
            name: Some(name.to_string()),
            namespace: Some("default".to_string()),
            uid: Some("test-uid".to_string()),
            ..Default::default()
        },
        spec: KubelicateSetSpec {
            replicas,
            min_replicas: 2,
            image: "test:latest".to_string(),
            failover_delay: 0,
            switchover_delay: 3600,
            port: 8080,
            control_port: 9090,
            data_port: 9091,
        },
        status,
    }
}

#[tokio::test]
async fn test_reconcile_pending_creates_pods() {
    let api = MockClusterApi::new();
    let state = ReconcilerState::default();
    let set = make_set("myapp", 3, None);

    let result = reconcile_set(&set, &api, &state).await.unwrap();
    assert!(matches!(result, ReconcileAction::Requeue(_)));

    // Should have created 3 pods
    assert_eq!(api.pod_count(), 3);

    // Status should be updated to Creating
    let status = api.last_status().unwrap();
    assert_eq!(status.phase, Phase::Creating);
}

#[tokio::test]
async fn test_reconcile_creating_waits_for_ready() {
    let api = MockClusterApi::new();
    let state = ReconcilerState::default();

    // First reconcile: create pods (Pending phase)
    let set = make_set("myapp", 3, None);
    reconcile_set(&set, &api, &state).await.unwrap();

    // Second reconcile: Creating phase, pods not ready yet
    let set = make_set(
        "myapp",
        3,
        Some(KubelicateSetStatus {
            phase: Phase::Creating,
            ..Default::default()
        }),
    );
    let result = reconcile_set(&set, &api, &state).await.unwrap();

    // Should requeue (waiting for pods)
    assert!(matches!(result, ReconcileAction::Requeue(_)));

    // Status should still be Creating (no Healthy transition)
    // The last status was from the Pending→Creating transition
}

#[tokio::test]
async fn test_reconcile_creating_to_healthy() {
    let api = MockClusterApi::new();
    let state = ReconcilerState::default();

    // Create pods (Pending)
    let set = make_set("myapp", 3, None);
    reconcile_set(&set, &api, &state).await.unwrap();

    // Mark all pods ready
    api.mark_all_pods_ready();

    // Reconcile Creating → should initialize partition and go Healthy
    let set = make_set(
        "myapp",
        3,
        Some(KubelicateSetStatus {
            phase: Phase::Creating,
            ..Default::default()
        }),
    );
    reconcile_set(&set, &api, &state).await.unwrap();

    // Status should be Healthy with a primary
    let status = api.last_status().unwrap();
    assert_eq!(status.phase, Phase::Healthy);
    assert!(status.current_primary.is_some());
    assert!(status.epoch.configuration_number > 0);

    // Driver should be stored
    assert!(state.drivers.lock().await.contains_key("default/myapp"));
}

#[tokio::test]
async fn test_reconcile_healthy_detects_primary_failure() {
    let api = MockClusterApi::new();
    let state = ReconcilerState::default();

    // Create pods and initialize
    let set = make_set("myapp", 3, None);
    reconcile_set(&set, &api, &state).await.unwrap();
    api.mark_all_pods_ready();

    let set = make_set(
        "myapp",
        3,
        Some(KubelicateSetStatus {
            phase: Phase::Creating,
            ..Default::default()
        }),
    );
    reconcile_set(&set, &api, &state).await.unwrap();

    let status = api.last_status().unwrap();
    let primary_name = status.current_primary.clone().unwrap();

    // Mark primary as not ready
    api.mark_pod_not_ready(&primary_name);

    // Reconcile Healthy — should detect failure and transition to FailingOver
    let set = make_set("myapp", 3, Some(status));
    reconcile_set(&set, &api, &state).await.unwrap();

    let status = api.last_status().unwrap();
    assert_eq!(status.phase, Phase::FailingOver);
}
