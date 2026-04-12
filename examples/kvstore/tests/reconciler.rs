use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use k8s_openapi::api::core::v1::{PersistentVolumeClaim, Pod, PodCondition, PodStatus, Service};
use kube::api::ObjectMeta;
use serial_test::serial;
use tokio::sync::RwLock;

use kubelicate_core::driver::ReplicaHandle;
use kubelicate_core::grpc::handle::GrpcReplicaHandle;
use kubelicate_core::pod::PodRuntime;
use kubelicate_core::types::ReplicaId;

use kubelicate_operator::cluster_api::ClusterApi;
use kubelicate_operator::crd::{
    KubelicateSet, KubelicateSetSpec, KubelicateSetStatus, Phase, PvcRetentionPolicy,
};
use kubelicate_operator::reconciler::{ReconcilerState, reconcile_set};

use kvstore::proto;
use kvstore::service;
use kvstore::state::{KvState, SharedState};

struct LivePod {
    control_address: String,
    data_address: String,
    client_address: String,
    #[allow(dead_code)]
    state: SharedState,
    _runtime_handle: tokio::task::JoinHandle<()>,
    _service_handle: tokio::task::JoinHandle<()>,
}

/// Mock ClusterApi that starts real PodRuntime + KV service for each pod.
struct KvClusterApi {
    pods: Mutex<Vec<Pod>>,
    live_pods: Mutex<HashMap<String, LivePod>>,
    statuses: Mutex<Vec<KubelicateSetStatus>>,
    pvcs: Mutex<HashMap<String, PersistentVolumeClaim>>,
    services: Mutex<HashMap<String, Service>>,
}

impl KvClusterApi {
    fn new() -> Self {
        Self {
            pods: Mutex::new(Vec::new()),
            live_pods: Mutex::new(HashMap::new()),
            statuses: Mutex::new(Vec::new()),
            pvcs: Mutex::new(HashMap::new()),
            services: Mutex::new(HashMap::new()),
        }
    }

    fn mark_all_pods_ready(&self) {
        let mut pods = self.pods.lock().unwrap();
        let live = self.live_pods.lock().unwrap();
        for pod in pods.iter_mut() {
            let name = pod.metadata.name.as_deref().unwrap_or("");
            let ip = live
                .get(name)
                .map(|lp| {
                    lp.control_address
                        .strip_prefix("http://")
                        .unwrap_or("")
                        .split(':')
                        .next()
                        .unwrap_or("127.0.0.1")
                        .to_string()
                })
                .unwrap_or_else(|| "127.0.0.1".to_string());
            pod.status = Some(PodStatus {
                conditions: Some(vec![PodCondition {
                    type_: "Ready".to_string(),
                    status: "True".to_string(),
                    ..Default::default()
                }]),
                pod_ip: Some(ip),
                ..Default::default()
            });
        }
    }

    /// Mark a specific pod as not ready (simulates pod failure for
    /// testing reconciler failure detection paths).
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

    fn client_address(&self, pod_name: &str) -> Option<String> {
        self.live_pods
            .lock()
            .unwrap()
            .get(pod_name)
            .map(|lp| lp.client_address.clone())
    }
}

#[async_trait]
impl ClusterApi for KvClusterApi {
    async fn list_pods(&self, _ns: &str, _sel: &str) -> Result<Vec<Pod>, String> {
        Ok(self.pods.lock().unwrap().clone())
    }

    async fn create_pod(&self, _ns: &str, pod: &Pod) -> Result<(), String> {
        let pod_name = pod.metadata.name.as_deref().unwrap().to_string();

        // Idempotent: skip if pod already exists
        {
            let pods = self.pods.lock().unwrap();
            if pods
                .iter()
                .any(|p| p.metadata.name.as_deref() == Some(pod_name.as_str()))
            {
                return Ok(());
            }
        }

        let replica_id: i64 = pod
            .metadata
            .labels
            .as_ref()
            .and_then(|l| l.get("kubelicate.io/pod-index"))
            .and_then(|v| v.parse::<i64>().ok())
            .unwrap_or(0)
            + 1;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client_address = listener.local_addr().unwrap().to_string();
        drop(listener);

        // Pre-bind data plane port
        let data_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let data_port = data_listener.local_addr().unwrap().port();
        let data_bind = format!("127.0.0.1:{}", data_port);
        let data_address = format!("http://{}", data_bind);
        drop(data_listener);

        let bundle = PodRuntime::builder(replica_id)
            .reply_timeout(Duration::from_secs(5))
            .data_bind(data_bind)
            .build()
            .await
            .unwrap();

        let control_address = bundle.control_address.clone();
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let data_dir = std::env::temp_dir().join("kv-test").join(format!(
            "reconciler-{}-{}-{}",
            replica_id,
            std::process::id(),
            n
        ));
        let state: SharedState = Arc::new(RwLock::new(KvState::open(data_dir).await.unwrap()));

        let runtime_handle = tokio::spawn(bundle.runtime.serve());
        let st = state.clone();
        let bind = client_address.clone();
        let service_handle = tokio::spawn(service::run_service(bundle.lifecycle_rx, st, bind));

        tokio::time::sleep(Duration::from_millis(50)).await;

        self.live_pods.lock().unwrap().insert(
            pod_name,
            LivePod {
                control_address,
                data_address,
                client_address,
                state,
                _runtime_handle: runtime_handle,
                _service_handle: service_handle,
            },
        );

        self.pods.lock().unwrap().push(pod.clone());
        Ok(())
    }

    async fn delete_pod(&self, _ns: &str, pod_name: &str) -> Result<(), String> {
        self.pods
            .lock()
            .unwrap()
            .retain(|p| p.metadata.name.as_deref() != Some(pod_name));
        self.live_pods.lock().unwrap().remove(pod_name);
        Ok(())
    }

    async fn patch_pod_labels(
        &self,
        _ns: &str,
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
        _ns: &str,
        _name: &str,
        status: &KubelicateSetStatus,
    ) -> Result<(), String> {
        self.statuses.lock().unwrap().push(status.clone());
        Ok(())
    }

    async fn create_replica_handle(
        &self,
        replica_id: ReplicaId,
        pod: &Pod,
        _spec: &KubelicateSetSpec,
    ) -> Result<Box<dyn ReplicaHandle>, String> {
        let pod_name = pod.metadata.name.as_deref().unwrap();
        let (control_addr, data_addr) = {
            let live = self.live_pods.lock().unwrap();
            let lp = live
                .get(pod_name)
                .ok_or_else(|| format!("no live pod for {}", pod_name))?;
            (lp.control_address.clone(), lp.data_address.clone())
        };

        let handle = GrpcReplicaHandle::connect(replica_id, control_addr, data_addr)
            .await
            .map_err(|e| e.to_string())?;

        Ok(Box::new(handle))
    }

    async fn get_pvc(&self, _ns: &str, name: &str) -> Result<PersistentVolumeClaim, String> {
        self.pvcs
            .lock()
            .unwrap()
            .get(name)
            .cloned()
            .ok_or_else(|| format!("pvc {} not found", name))
    }

    async fn create_pvc(&self, _ns: &str, pvc: &PersistentVolumeClaim) -> Result<(), String> {
        let name = pvc.metadata.name.as_deref().unwrap().to_string();
        self.pvcs
            .lock()
            .unwrap()
            .entry(name)
            .or_insert_with(|| pvc.clone());
        Ok(())
    }

    async fn list_pvcs(&self, _ns: &str, _sel: &str) -> Result<Vec<PersistentVolumeClaim>, String> {
        Ok(self.pvcs.lock().unwrap().values().cloned().collect())
    }

    async fn delete_pvc(&self, _ns: &str, name: &str) -> Result<(), String> {
        self.pvcs.lock().unwrap().remove(name);
        Ok(())
    }

    async fn get_service(&self, _ns: &str, name: &str) -> Result<Service, String> {
        self.services
            .lock()
            .unwrap()
            .get(name)
            .cloned()
            .ok_or_else(|| format!("service {} not found", name))
    }

    async fn create_service(&self, _ns: &str, svc: &Service) -> Result<(), String> {
        let name = svc.metadata.name.as_deref().unwrap().to_string();
        self.services
            .lock()
            .unwrap()
            .entry(name)
            .or_insert_with(|| svc.clone());
        Ok(())
    }

    async fn delete_service(&self, _ns: &str, name: &str) -> Result<(), String> {
        self.services.lock().unwrap().remove(name);
        Ok(())
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
            min_replicas: 1,
            image: "test:latest".to_string(),
            failover_delay: 0,
            switchover_delay: 3600,
            port: 8080,
            control_port: 9090,
            data_port: 9091,
            storage: "256Mi".to_string(),
            pvc_retention_policy: PvcRetentionPolicy::Delete,
        },
        status,
    }
}

async fn connect_kv(
    addr: &str,
) -> proto::kv_store_client::KvStoreClient<tonic::transport::Channel> {
    for attempt in 0..30 {
        match proto::kv_store_client::KvStoreClient::connect(format!("http://{}", addr)).await {
            Ok(c) => return c,
            Err(_) if attempt < 29 => tokio::time::sleep(Duration::from_millis(100)).await,
            Err(e) => panic!("connect failed: {}", e),
        }
    }
    unreachable!()
}

/// Full reconciler test: Pending → Creating → Healthy → write KV data.
#[test_log::test(tokio::test)]
#[serial]
async fn test_reconciler_creates_partition_and_serves_kv() {
    let api = KvClusterApi::new();
    let state = ReconcilerState::default();

    // Pending → Creating (creates 3 pods with real PodRuntimes)
    let set = make_set("myapp", 3, None);
    reconcile_set(&set, &api, &state).await.unwrap();
    assert_eq!(api.pods.lock().unwrap().len(), 3);

    // Verify PVCs created alongside pods
    assert_eq!(api.pvcs.lock().unwrap().len(), 3);
    assert!(api.pvcs.lock().unwrap().contains_key("myapp-0-data"));
    assert!(api.pvcs.lock().unwrap().contains_key("myapp-1-data"));
    assert!(api.pvcs.lock().unwrap().contains_key("myapp-2-data"));

    // Verify services created (rw, ro, r)
    assert_eq!(api.services.lock().unwrap().len(), 3);
    assert!(api.services.lock().unwrap().contains_key("myapp-rw"));
    assert!(api.services.lock().unwrap().contains_key("myapp-ro"));
    assert!(api.services.lock().unwrap().contains_key("myapp-r"));

    // Mark pods ready
    api.mark_all_pods_ready();

    // Creating → Healthy (initializes partition via driver)
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
    assert_eq!(status.phase, Phase::Healthy);
    assert!(status.current_primary.is_some());

    // Write via KV API on primary
    let primary_name = status.current_primary.unwrap();
    let client_addr = api.client_address(&primary_name).unwrap();
    let mut kv = connect_kv(&client_addr).await;

    let resp = kv
        .put(proto::PutRequest {
            key: "hello".to_string(),
            value: "reconciler".to_string(),
        })
        .await
        .unwrap();
    assert_eq!(resp.get_ref().lsn, 1);

    let resp = kv
        .get(proto::GetRequest {
            key: "hello".to_string(),
        })
        .await
        .unwrap();
    assert!(resp.get_ref().found);
    assert_eq!(resp.get_ref().value, "reconciler");
}

/// Reconciler test: create partition → write data → switchover → write on new primary.
#[test_log::test(tokio::test)]
#[serial]
async fn test_reconciler_switchover() {
    let api = KvClusterApi::new();
    let state = ReconcilerState::default();

    // Pending → Creating
    let set = make_set("myapp", 3, None);
    reconcile_set(&set, &api, &state).await.unwrap();
    api.mark_all_pods_ready();

    // Creating → Healthy
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
    assert_eq!(status.phase, Phase::Healthy);
    let original_primary = status.current_primary.clone().unwrap();

    // Write data on original primary
    let client_addr = api.client_address(&original_primary).unwrap();
    let mut kv = connect_kv(&client_addr).await;
    kv.put(proto::PutRequest {
        key: "before-switch".to_string(),
        value: "original".to_string(),
    })
    .await
    .unwrap();

    // Pick a secondary as switchover target
    let target_name = {
        let pods = api.pods.lock().unwrap();
        pods.iter()
            .map(|p| p.metadata.name.clone().unwrap())
            .find(|n| n != &original_primary)
            .unwrap()
    };

    // Healthy → Switchover: set target_primary to a different pod
    let set = make_set(
        "myapp",
        3,
        Some(KubelicateSetStatus {
            phase: Phase::Healthy,
            current_primary: Some(original_primary.clone()),
            target_primary: Some(target_name.clone()),
            ..status.clone()
        }),
    );
    reconcile_set(&set, &api, &state).await.unwrap();

    let status = api.last_status().unwrap();
    assert_eq!(status.phase, Phase::Switchover);

    // Switchover → Healthy: execute the switchover
    let set = make_set("myapp", 3, Some(status.clone()));
    reconcile_set(&set, &api, &state).await.unwrap();

    let status = api.last_status().unwrap();
    assert_eq!(status.phase, Phase::Healthy);
    assert_eq!(
        status.current_primary.as_deref(),
        Some(target_name.as_str())
    );
    assert_ne!(
        status.current_primary.as_deref(),
        Some(original_primary.as_str())
    );

    // Write on new primary
    let new_client_addr = api.client_address(&target_name).unwrap();
    let mut kv2 = connect_kv(&new_client_addr).await;

    let resp = kv2
        .put(proto::PutRequest {
            key: "after-switch".to_string(),
            value: "new-primary".to_string(),
        })
        .await
        .unwrap();
    assert!(resp.get_ref().lsn > 0);

    // Read back on new primary
    let resp = kv2
        .get(proto::GetRequest {
            key: "after-switch".to_string(),
        })
        .await
        .unwrap();
    assert!(resp.get_ref().found);
    assert_eq!(resp.get_ref().value, "new-primary");

    // Write to old primary should fail (no longer primary)
    let result = kv
        .put(proto::PutRequest {
            key: "stale-write".to_string(),
            value: "should-fail".to_string(),
        })
        .await;
    assert!(result.is_err(), "write to demoted primary should fail");
}

/// Reconciler test: Creating phase requeues when pods are not yet ready.
#[test_log::test(tokio::test)]
#[serial]
async fn test_reconciler_creating_waits_for_ready() {
    let api = KvClusterApi::new();
    let state = ReconcilerState::default();

    // Pending → Creating (creates pods but they're not ready yet)
    let set = make_set("myapp", 3, None);
    reconcile_set(&set, &api, &state).await.unwrap();
    assert_eq!(api.pods.lock().unwrap().len(), 3);

    // Don't mark pods ready — reconcile Creating phase
    let set = make_set(
        "myapp",
        3,
        Some(KubelicateSetStatus {
            phase: Phase::Creating,
            ..Default::default()
        }),
    );
    let result = reconcile_set(&set, &api, &state).await.unwrap();

    // Should requeue (waiting for pods to become ready)
    assert!(
        matches!(
            result,
            kubelicate_operator::reconciler::ReconcileAction::Requeue(_)
        ),
        "should requeue when pods not ready"
    );

    // Status should still be Creating (no transition to Healthy)
    let status = api.last_status().unwrap();
    assert_eq!(status.phase, Phase::Creating);
}

/// Reconciler test: Healthy phase detects primary pod NotReady → FailingOver,
/// then FailingOver phase completes failover → back to Healthy with new primary.
#[test_log::test(tokio::test)]
#[serial]
async fn test_reconciler_detects_primary_failure_and_fails_over() {
    let api = KvClusterApi::new();
    let state = ReconcilerState::default();

    // Create and initialize the partition (Pending → Creating → Healthy)
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
    assert_eq!(status.phase, Phase::Healthy);
    let primary_name = status.current_primary.clone().unwrap();

    // Write data on primary before failure
    let client_addr = api.client_address(&primary_name).unwrap();
    let mut kv = connect_kv(&client_addr).await;
    kv.put(proto::PutRequest {
        key: "before-crash".to_string(),
        value: "important".to_string(),
    })
    .await
    .unwrap();

    // Mark primary as not ready (simulate pod crash)
    api.mark_pod_not_ready(&primary_name);

    // Reconcile Healthy — should detect failure → FailingOver
    let set = make_set("myapp", 3, Some(status));
    reconcile_set(&set, &api, &state).await.unwrap();

    let status = api.last_status().unwrap();
    assert_eq!(status.phase, Phase::FailingOver);

    // Reconcile FailingOver — should run failover → Healthy with new primary
    let set = make_set("myapp", 3, Some(status));
    reconcile_set(&set, &api, &state).await.unwrap();

    let status = api.last_status().unwrap();
    assert_eq!(status.phase, Phase::Healthy);

    // New primary should be different from the crashed one
    let new_primary = status.current_primary.clone().unwrap();
    assert_ne!(new_primary, primary_name);

    // New primary should serve data that was written before crash
    let new_client_addr = api.client_address(&new_primary).unwrap();
    let mut kv2 = connect_kv(&new_client_addr).await;
    let resp = kv2
        .get(proto::GetRequest {
            key: "before-crash".to_string(),
        })
        .await
        .unwrap();
    assert!(resp.get_ref().found);
    assert_eq!(resp.get_ref().value, "important");

    // New primary accepts new writes
    let resp = kv2
        .put(proto::PutRequest {
            key: "after-failover".to_string(),
            value: "recovered".to_string(),
        })
        .await
        .unwrap();
    assert!(resp.get_ref().lsn > 0);

    // Driver now has 2 replicas (failed primary was removed, not demoted).
    // The old primary pod is still running but orphaned — nobody closed it
    // or replaced it. The partition is "Healthy" but degraded.
    // TODO: Healthy phase should detect spec.replicas (3) > driver count (2)
    //       and create a replacement. See operator-failure-scenarios.md §2, §9.
    {
        let drivers = state.drivers.lock().await;
        let driver = drivers.get("default/myapp").unwrap();
        assert_eq!(
            driver.replica_ids().len(),
            2,
            "failed primary removed, not replaced yet"
        );
        assert_eq!(
            api.pods.lock().unwrap().len(),
            3,
            "old pod still exists (orphaned)"
        );
    }
}

/// Reconciler test: Healthy phase detects spec.replicas > actual → scale-up.
#[test_log::test(tokio::test)]
#[serial]
async fn test_reconciler_scale_up() {
    let api = KvClusterApi::new();
    let state = ReconcilerState::default();

    // Create partition with 1 replica
    let set = make_set("myapp", 1, None);
    reconcile_set(&set, &api, &state).await.unwrap();
    api.mark_all_pods_ready();

    let set = make_set(
        "myapp",
        1,
        Some(KubelicateSetStatus {
            phase: Phase::Creating,
            ..Default::default()
        }),
    );
    reconcile_set(&set, &api, &state).await.unwrap();

    let status = api.last_status().unwrap();
    assert_eq!(status.phase, Phase::Healthy);
    let primary_name = status.current_primary.clone().unwrap();

    // Write data on primary
    let client_addr = api.client_address(&primary_name).unwrap();
    let mut kv = connect_kv(&client_addr).await;
    kv.put(proto::PutRequest {
        key: "before-scale".to_string(),
        value: "original".to_string(),
    })
    .await
    .unwrap();

    // Scale up: change spec to 3 replicas
    // First reconcile creates new pods
    let set = make_set("myapp", 3, Some(status.clone()));
    reconcile_set(&set, &api, &state).await.unwrap();
    assert_eq!(api.pods.lock().unwrap().len(), 3);

    // PVCs should also scale up
    assert_eq!(api.pvcs.lock().unwrap().len(), 3);

    // Mark new pods ready
    api.mark_all_pods_ready();

    // Second reconcile adds new replicas to driver via add_replica
    let status = api.last_status().unwrap();
    let set = make_set("myapp", 3, Some(status));
    reconcile_set(&set, &api, &state).await.unwrap();

    // Driver should have 3 replicas
    {
        let drivers = state.drivers.lock().await;
        let driver = drivers.get("default/myapp").unwrap();
        assert_eq!(driver.replica_ids().len(), 3);
    }

    // Primary still works
    let resp = kv
        .get(proto::GetRequest {
            key: "before-scale".to_string(),
        })
        .await
        .unwrap();
    assert!(resp.get_ref().found);
}

/// Reconciler test: Healthy phase detects spec.replicas < actual → scale-down.
#[test_log::test(tokio::test)]
#[serial]
async fn test_reconciler_scale_down() {
    let api = KvClusterApi::new();
    let state = ReconcilerState::default();

    // Create partition with 3 replicas
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
    assert_eq!(status.phase, Phase::Healthy);

    // Write data
    let primary_name = status.current_primary.clone().unwrap();
    let client_addr = api.client_address(&primary_name).unwrap();
    let mut kv = connect_kv(&client_addr).await;
    kv.put(proto::PutRequest {
        key: "before-scale-down".to_string(),
        value: "yes".to_string(),
    })
    .await
    .unwrap();

    // Scale down: change spec to 1 replica
    let set = make_set("myapp", 1, Some(status.clone()));
    reconcile_set(&set, &api, &state).await.unwrap();

    // PVCs retained after scale-down (pod deleted, PVC kept)
    assert_eq!(api.pvcs.lock().unwrap().len(), 3);

    // Driver should have fewer replicas
    {
        let drivers = state.drivers.lock().await;
        let driver = drivers.get("default/myapp").unwrap();
        // Scale-down removes one at a time, may need multiple reconcile loops
        assert!(
            driver.replica_ids().len() < 3,
            "should have removed at least one replica"
        );
    }

    // Primary still works
    let resp = kv
        .put(proto::PutRequest {
            key: "after-scale-down".to_string(),
            value: "still-works".to_string(),
        })
        .await
        .unwrap();
    assert!(resp.get_ref().lsn > 0);
}

/// Reconciler test: double failover — after the first failover, the new
/// primary also fails. The reconciler handles a second failover cycle
/// (FailingOver → Healthy again) with the last surviving replica.
/// Exercises: reconciler retry loop, driver failover, A1 best-effort epoch.
#[test_log::test(tokio::test)]
#[serial]
async fn test_reconciler_double_failover() {
    let api = KvClusterApi::new();
    let state = ReconcilerState::default();

    // Create 3-replica partition → Healthy
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
    assert_eq!(status.phase, Phase::Healthy);
    let first_primary = status.current_primary.clone().unwrap();

    // Write data
    let addr = api.client_address(&first_primary).unwrap();
    let mut kv = connect_kv(&addr).await;
    kv.put(proto::PutRequest {
        key: "epoch-1".into(),
        value: "data".into(),
    })
    .await
    .unwrap();

    // --- First failover: primary fails ---
    api.mark_pod_not_ready(&first_primary);

    let set = make_set("myapp", 3, Some(status));
    reconcile_set(&set, &api, &state).await.unwrap();
    let status = api.last_status().unwrap();
    assert_eq!(status.phase, Phase::FailingOver);

    let set = make_set("myapp", 3, Some(status));
    reconcile_set(&set, &api, &state).await.unwrap();
    let status = api.last_status().unwrap();
    assert_eq!(status.phase, Phase::Healthy);

    let second_primary = status.current_primary.clone().unwrap();
    assert_ne!(second_primary, first_primary);

    // Write on second primary
    let addr2 = api.client_address(&second_primary).unwrap();
    let mut kv2 = connect_kv(&addr2).await;
    kv2.put(proto::PutRequest {
        key: "epoch-2".into(),
        value: "survived".into(),
    })
    .await
    .unwrap();

    // --- Second failover: new primary also fails ---
    api.mark_pod_not_ready(&second_primary);

    let set = make_set("myapp", 3, Some(status));
    reconcile_set(&set, &api, &state).await.unwrap();
    let status = api.last_status().unwrap();
    assert_eq!(status.phase, Phase::FailingOver);

    // Remove the two dead replicas from the driver so failover
    // doesn't select a dead candidate (A3 gap for failover).
    {
        let mut drivers = state.drivers.lock().await;
        let driver = drivers.get_mut("default/myapp").unwrap();
        let first_id: i64 = first_primary
            .strip_prefix("myapp-")
            .unwrap()
            .parse::<i64>()
            .unwrap()
            + 1;
        let second_id: i64 = second_primary
            .strip_prefix("myapp-")
            .unwrap()
            .parse::<i64>()
            .unwrap()
            + 1;
        driver.remove_replica_from_driver(first_id);
        driver.remove_replica_from_driver(second_id);
    }

    let set = make_set("myapp", 3, Some(status));
    reconcile_set(&set, &api, &state).await.unwrap();
    let status = api.last_status().unwrap();
    assert_eq!(status.phase, Phase::Healthy);

    let third_primary = status.current_primary.clone().unwrap();
    assert_ne!(third_primary, first_primary);
    assert_ne!(third_primary, second_primary);

    // Third primary should have data from both epochs
    let addr3 = api.client_address(&third_primary).unwrap();
    let mut kv3 = connect_kv(&addr3).await;

    let resp = kv3
        .get(proto::GetRequest {
            key: "epoch-1".into(),
        })
        .await
        .unwrap();
    assert!(resp.get_ref().found, "data from first epoch should survive");

    let resp = kv3
        .get(proto::GetRequest {
            key: "epoch-2".into(),
        })
        .await
        .unwrap();
    assert!(
        resp.get_ref().found,
        "data from second epoch should survive"
    );
}

/// Verify idempotent creation: reconciling Pending twice doesn't create duplicate PVCs.
#[test_log::test(tokio::test)]
#[serial]
async fn test_reconciler_idempotent_creation() {
    let api = KvClusterApi::new();
    let state = ReconcilerState::default();

    let set = make_set("idempotent", 3, None);

    // First reconcile: Pending → Creating
    reconcile_set(&set, &api, &state).await.unwrap();
    assert_eq!(api.pods.lock().unwrap().len(), 3);
    assert_eq!(api.pvcs.lock().unwrap().len(), 3);

    // Second reconcile at Pending — should not duplicate
    reconcile_set(&set, &api, &state).await.unwrap();
    assert_eq!(api.pods.lock().unwrap().len(), 3);
    assert_eq!(api.pvcs.lock().unwrap().len(), 3);
    assert_eq!(api.services.lock().unwrap().len(), 3);
}
