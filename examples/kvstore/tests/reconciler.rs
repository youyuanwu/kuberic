use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use k8s_openapi::api::core::v1::{PersistentVolumeClaim, Pod, PodCondition, PodStatus, Service};
use kube::api::ObjectMeta;
use serial_test::serial;
use tokio::sync::RwLock;

use kuberic_core::driver::ReplicaHandle;
use kuberic_core::error::Result as CoreResult;
use kuberic_core::grpc::handle::GrpcReplicaHandle;
use kuberic_core::pod::PodRuntime;
use kuberic_core::types::{
    DataLossAction, DurableReplicaAction, Epoch, Lsn, OpenMode, ReplicaId, ReplicaInfo,
    ReplicaInstanceId, ReplicaSetConfig, ReplicaSetQuorumMode, ReplicaStatusInfo, Role,
};

use kuberic_operator::cluster_api::ClusterApi;
use kuberic_operator::crd::{
    DurableActionKind, DurableAddMode, DurableOperationKind, DurableOperationPhase, KubericSet,
    KubericSetSpec, KubericSetStatus, Phase, PvcRetentionPolicy, StableReplicaRoleStatus,
};
use kuberic_operator::reconciler::{ReconcilerState, reconcile_set};

use kvstore::proto;
use kvstore::service;
use kvstore::state::{KvState, SharedState};

async fn allocate_unique_address() -> String {
    static ALLOCATED_PORTS: OnceLock<Mutex<HashSet<u16>>> = OnceLock::new();

    loop {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let is_new = ALLOCATED_PORTS
            .get_or_init(|| Mutex::new(HashSet::new()))
            .lock()
            .unwrap()
            .insert(port);
        drop(listener);

        if is_new {
            return format!("127.0.0.1:{port}");
        }
    }
}

struct LivePod {
    control_address: String,
    data_address: String,
    client_address: String,
    data_dir: PathBuf,
    #[allow(dead_code)]
    state: SharedState,
    _runtime_handle: tokio::task::JoinHandle<()>,
    _service_handle: tokio::task::JoinHandle<()>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ControlOperation {
    Open,
    Close,
    ChangeRole,
    UpdateEpoch,
    UpdateCatchUpConfiguration,
    UpdateCurrentConfiguration,
    WaitForCatchUpQuorum,
    BuildReplica,
    RemoveReplica,
    OnDataLoss,
    RevokeWriteStatus,
    GetStatus,
}

struct ObservedHandle {
    inner: Box<dyn ReplicaHandle>,
    operations: Arc<Mutex<Vec<ControlOperation>>>,
    fail_before_next_durable_action: Arc<Mutex<Option<ControlOperation>>>,
    fail_after_next_durable_action: Arc<Mutex<Option<ControlOperation>>>,
}

impl ObservedHandle {
    fn record(&self, operation: ControlOperation) {
        self.operations.lock().unwrap().push(operation);
    }
}

#[async_trait]
impl ReplicaHandle for ObservedHandle {
    fn id(&self) -> ReplicaId {
        self.inner.id()
    }

    fn instance_id(&self) -> ReplicaInstanceId {
        self.inner.instance_id()
    }

    async fn open(&self, mode: OpenMode) -> CoreResult<()> {
        self.record(ControlOperation::Open);
        self.inner.open(mode).await
    }

    async fn close(&self) -> CoreResult<()> {
        self.record(ControlOperation::Close);
        self.inner.close().await
    }

    fn abort(&self) {
        self.inner.abort();
    }

    async fn change_role(&self, epoch: Epoch, role: Role) -> CoreResult<()> {
        self.record(ControlOperation::ChangeRole);
        self.inner.change_role(epoch, role).await
    }

    async fn update_epoch(&self, epoch: Epoch) -> CoreResult<()> {
        self.record(ControlOperation::UpdateEpoch);
        self.inner.update_epoch(epoch).await
    }

    fn current_progress(&self) -> Lsn {
        self.inner.current_progress()
    }

    fn catch_up_capability(&self) -> Lsn {
        self.inner.catch_up_capability()
    }

    async fn on_data_loss(&self) -> CoreResult<DataLossAction> {
        self.record(ControlOperation::OnDataLoss);
        self.inner.on_data_loss().await
    }

    async fn update_catch_up_configuration(
        &self,
        current: ReplicaSetConfig,
        previous: ReplicaSetConfig,
    ) -> CoreResult<()> {
        self.record(ControlOperation::UpdateCatchUpConfiguration);
        self.inner
            .update_catch_up_configuration(current, previous)
            .await
    }

    async fn update_current_configuration(&self, current: ReplicaSetConfig) -> CoreResult<()> {
        self.record(ControlOperation::UpdateCurrentConfiguration);
        self.inner.update_current_configuration(current).await
    }

    async fn wait_for_catch_up_quorum(&self, mode: ReplicaSetQuorumMode) -> CoreResult<()> {
        self.record(ControlOperation::WaitForCatchUpQuorum);
        self.inner.wait_for_catch_up_quorum(mode).await
    }

    async fn build_replica(&self, replica: ReplicaInfo) -> CoreResult<()> {
        self.record(ControlOperation::BuildReplica);
        self.inner.build_replica(replica).await
    }

    async fn remove_replica(
        &self,
        replica_id: ReplicaId,
        instance_id: ReplicaInstanceId,
    ) -> CoreResult<()> {
        self.record(ControlOperation::RemoveReplica);
        self.inner.remove_replica(replica_id, instance_id).await
    }

    async fn revoke_write_status(&self) -> CoreResult<()> {
        self.record(ControlOperation::RevokeWriteStatus);
        self.inner.revoke_write_status().await
    }

    fn replicator_address(&self) -> String {
        self.inner.replicator_address()
    }

    async fn get_status(&self) -> CoreResult<ReplicaStatusInfo> {
        self.record(ControlOperation::GetStatus);
        self.inner.get_status().await
    }

    async fn execute_durable_action(
        &self,
        action_id: &str,
        action: DurableReplicaAction,
    ) -> CoreResult<()> {
        let operation = match &action {
            DurableReplicaAction::Open { .. } => ControlOperation::Open,
            DurableReplicaAction::Close => ControlOperation::Close,
            DurableReplicaAction::RevokeWriteStatus => ControlOperation::RevokeWriteStatus,
            DurableReplicaAction::ChangeRole { .. } => ControlOperation::ChangeRole,
            DurableReplicaAction::UpdateEpoch { .. } => ControlOperation::UpdateEpoch,
            DurableReplicaAction::UpdateCatchUpConfiguration { .. } => {
                ControlOperation::UpdateCatchUpConfiguration
            }
            DurableReplicaAction::WaitForCatchUpQuorum { .. } => {
                ControlOperation::WaitForCatchUpQuorum
            }
            DurableReplicaAction::UpdateCurrentConfiguration { .. } => {
                ControlOperation::UpdateCurrentConfiguration
            }
            DurableReplicaAction::BuildReplica { .. } => ControlOperation::BuildReplica,
            DurableReplicaAction::RemoveReplica { .. } => ControlOperation::RemoveReplica,
        };
        self.record(operation);
        if self
            .fail_before_next_durable_action
            .lock()
            .unwrap()
            .as_ref()
            == Some(&operation)
        {
            self.fail_before_next_durable_action.lock().unwrap().take();
            return Err(kuberic_core::error::KubericError::Internal(
                "injected activity failure".into(),
            ));
        }
        self.inner.execute_durable_action(action_id, action).await?;
        if self.fail_after_next_durable_action.lock().unwrap().as_ref() == Some(&operation) {
            self.fail_after_next_durable_action.lock().unwrap().take();
            return Err(kuberic_core::error::KubericError::Internal(
                "injected lost activity reply".into(),
            ));
        }
        Ok(())
    }
}

/// Mock ClusterApi that starts real PodRuntime + KV service for each pod.
struct KvClusterApi {
    pods: Mutex<Vec<Pod>>,
    live_pods: Mutex<HashMap<String, LivePod>>,
    /// Preserved data dirs from crashed pods (simulates PVC survival).
    data_dirs: Mutex<HashMap<String, PathBuf>>,
    statuses: Mutex<Vec<KubericSetStatus>>,
    pvcs: Mutex<HashMap<String, PersistentVolumeClaim>>,
    services: Mutex<HashMap<String, Service>>,
    operations: Arc<Mutex<Vec<ControlOperation>>>,
    fail_next_status_patch: Mutex<bool>,
    fail_next_status_conflict: Mutex<bool>,
    fail_before_next_durable_action: Arc<Mutex<Option<ControlOperation>>>,
    fail_after_next_durable_action: Arc<Mutex<Option<ControlOperation>>>,
}

impl KvClusterApi {
    fn new() -> Self {
        Self {
            pods: Mutex::new(Vec::new()),
            live_pods: Mutex::new(HashMap::new()),
            data_dirs: Mutex::new(HashMap::new()),
            statuses: Mutex::new(Vec::new()),
            pvcs: Mutex::new(HashMap::new()),
            services: Mutex::new(HashMap::new()),
            operations: Arc::new(Mutex::new(Vec::new())),
            fail_next_status_patch: Mutex::new(false),
            fail_next_status_conflict: Mutex::new(false),
            fail_before_next_durable_action: Arc::new(Mutex::new(None)),
            fail_after_next_durable_action: Arc::new(Mutex::new(None)),
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

    fn last_status(&self) -> Option<KubericSetStatus> {
        self.statuses.lock().unwrap().last().cloned()
    }

    fn client_address(&self, pod_name: &str) -> Option<String> {
        self.live_pods
            .lock()
            .unwrap()
            .get(pod_name)
            .map(|lp| lp.client_address.clone())
    }

    fn reset_operations(&self) {
        self.operations.lock().unwrap().clear();
    }

    fn operations(&self) -> Vec<ControlOperation> {
        self.operations.lock().unwrap().clone()
    }

    fn fail_next_status_patch(&self) {
        *self.fail_next_status_patch.lock().unwrap() = true;
    }

    fn fail_next_status_conflict(&self) {
        *self.fail_next_status_conflict.lock().unwrap() = true;
    }

    fn fail_before_next_durable_action(&self, operation: ControlOperation) {
        *self.fail_before_next_durable_action.lock().unwrap() = Some(operation);
    }

    fn fail_after_next_durable_action(&self, operation: ControlOperation) {
        *self.fail_after_next_durable_action.lock().unwrap() = Some(operation);
    }

    /// Simulate a pod crash. Aborts the PodRuntime and service tasks
    /// (gRPC channels break immediately), marks the K8s Pod NotReady,
    /// and removes the LivePod entry. Preserves data_dir (simulates PVC).
    fn crash_pod(&self, pod_name: &str) {
        if let Some(live) = self.live_pods.lock().unwrap().remove(pod_name) {
            live._runtime_handle.abort();
            live._service_handle.abort();
            self.data_dirs
                .lock()
                .unwrap()
                .insert(pod_name.to_string(), live.data_dir);
        }
        self.mark_pod_not_ready(pod_name);
    }

    /// Simulate K8s restarting a crashed pod. Creates a fresh
    /// PodRuntime + KV service on new ports, reuses the same data_dir
    /// (simulates PVC re-attach), and marks the pod Ready.
    async fn restart_pod(&self, pod_name: &str) {
        let replica_id: i64 = pod_name.rsplit('-').next().unwrap().parse::<i64>().unwrap() + 1;
        static INSTANCE_COUNTER: std::sync::atomic::AtomicU64 =
            std::sync::atomic::AtomicU64::new(1);
        let generation = INSTANCE_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let instance_id = ReplicaInstanceId::new(format!("restarted-{pod_name}-{generation}"));

        let client_address = allocate_unique_address().await;
        let data_bind = allocate_unique_address().await;
        let data_address = format!("http://{}", data_bind);
        let control_bind = allocate_unique_address().await;

        let bundle = PodRuntime::builder(replica_id)
            .instance_id(instance_id.clone())
            .reply_timeout(Duration::from_secs(5))
            .control_bind(control_bind)
            .data_bind(data_bind)
            .build()
            .await
            .unwrap();

        let control_address = bundle.control_address.clone();

        // Reuse data_dir from crashed pod (PVC re-attach)
        let data_dir = self
            .data_dirs
            .lock()
            .unwrap()
            .remove(pod_name)
            .unwrap_or_else(|| {
                std::env::temp_dir().join("kv-test").join(format!(
                    "restart-{}-{}",
                    pod_name,
                    std::process::id(),
                ))
            });
        let state: SharedState =
            Arc::new(RwLock::new(KvState::open(data_dir.clone()).await.unwrap()));

        let runtime_handle = tokio::spawn(bundle.runtime.serve());
        let st = state.clone();
        let bind = client_address.clone();
        let service_handle = tokio::spawn(service::run_service(bundle.lifecycle_rx, st, bind));

        tokio::time::sleep(Duration::from_millis(50)).await;

        self.live_pods.lock().unwrap().insert(
            pod_name.to_string(),
            LivePod {
                control_address,
                data_address,
                client_address,
                data_dir,
                state,
                _runtime_handle: runtime_handle,
                _service_handle: service_handle,
            },
        );

        let mut pods = self.pods.lock().unwrap();
        let pod = pods
            .iter_mut()
            .find(|pod| pod.metadata.name.as_deref() == Some(pod_name))
            .unwrap();
        pod.metadata.uid = Some(instance_id.to_string());
        drop(pods);
        self.mark_all_pods_ready();
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
            .and_then(|l| l.get("kuberic.io/pod-index"))
            .and_then(|v| v.parse::<i64>().ok())
            .unwrap_or(0)
            + 1;
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let instance_id = ReplicaInstanceId::new(format!("reconciler-{replica_id}-{n}"));

        let client_address = allocate_unique_address().await;

        // Pre-bind data plane port
        let data_bind = allocate_unique_address().await;
        let data_address = format!("http://{}", data_bind);
        let control_bind = allocate_unique_address().await;

        let bundle = PodRuntime::builder(replica_id)
            .instance_id(instance_id.clone())
            .reply_timeout(Duration::from_secs(5))
            .control_bind(control_bind)
            .data_bind(data_bind)
            .build()
            .await
            .unwrap();

        let control_address = bundle.control_address.clone();
        let data_dir = std::env::temp_dir().join("kv-test").join(format!(
            "reconciler-{}-{}-{}",
            replica_id,
            std::process::id(),
            n
        ));
        let state: SharedState =
            Arc::new(RwLock::new(KvState::open(data_dir.clone()).await.unwrap()));

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
                data_dir,
                state,
                _runtime_handle: runtime_handle,
                _service_handle: service_handle,
            },
        );

        let mut stored_pod = pod.clone();
        stored_pod.metadata.uid = Some(instance_id.to_string());
        self.pods.lock().unwrap().push(stored_pod);
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
        status: &KubericSetStatus,
        _expected_resource_version: Option<&str>,
    ) -> Result<(), String> {
        if std::mem::take(&mut *self.fail_next_status_conflict.lock().unwrap()) {
            return Err("resource version conflict".to_string());
        }
        if std::mem::take(&mut *self.fail_next_status_patch.lock().unwrap()) {
            return Err("injected status persistence failure".to_string());
        }
        self.statuses.lock().unwrap().push(status.clone());
        Ok(())
    }

    async fn create_replica_handle(
        &self,
        replica_id: ReplicaId,
        pod: &Pod,
        _spec: &KubericSetSpec,
    ) -> Result<Box<dyn ReplicaHandle>, String> {
        let pod_name = pod.metadata.name.as_deref().unwrap();
        let (control_addr, data_addr) = {
            let live = self.live_pods.lock().unwrap();
            let lp = live
                .get(pod_name)
                .ok_or_else(|| format!("no live pod for {}", pod_name))?;
            (lp.control_address.clone(), lp.data_address.clone())
        };

        let instance_id = pod
            .metadata
            .uid
            .clone()
            .map(ReplicaInstanceId::new)
            .ok_or_else(|| format!("pod {pod_name} has no UID"))?;
        let handle = GrpcReplicaHandle::connect(replica_id, instance_id, control_addr, data_addr)
            .await
            .map_err(|e| e.to_string())?;

        Ok(Box::new(ObservedHandle {
            inner: Box::new(handle),
            operations: self.operations.clone(),
            fail_before_next_durable_action: self.fail_before_next_durable_action.clone(),
            fail_after_next_durable_action: self.fail_after_next_durable_action.clone(),
        }))
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

fn make_set(name: &str, replicas: i32, status: Option<KubericSetStatus>) -> KubericSet {
    KubericSet {
        metadata: ObjectMeta {
            name: Some(name.to_string()),
            namespace: Some("default".to_string()),
            uid: Some("test-uid".to_string()),
            ..Default::default()
        },
        spec: KubericSetSpec {
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

/// Retry a Get call — the client gRPC service may not be registered yet
/// after a role transition (returns Unimplemented until ready).
async fn retry_get(
    client: &mut proto::kv_store_client::KvStoreClient<tonic::transport::Channel>,
    key: &str,
) -> tonic::Response<proto::GetResponse> {
    for attempt in 0..30 {
        match client.get(proto::GetRequest { key: key.into() }).await {
            Ok(resp) => return resp,
            Err(e) if attempt < 29 => {
                tracing::debug!(attempt, error = %e, "Get retry");
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            Err(e) => panic!("Get({}) failed after retries: {}", key, e),
        }
    }
    unreachable!()
}

async fn create_healthy_set(
    api: &KvClusterApi,
    state: &ReconcilerState,
    name: &str,
    replicas: i32,
) -> KubericSetStatus {
    reconcile_set(&make_set(name, replicas, None), api, state)
        .await
        .unwrap();
    api.mark_all_pods_ready();
    reconcile_set(
        &make_set(
            name,
            replicas,
            Some(KubericSetStatus {
                phase: Phase::Creating,
                ..Default::default()
            }),
        ),
        api,
        state,
    )
    .await
    .unwrap();
    let status = api.last_status().unwrap();
    assert_eq!(status.phase, Phase::Healthy);
    assert_eq!(
        status
            .stable_snapshot
            .as_ref()
            .expect("creation must persist a stable snapshot")
            .members
            .len(),
        replicas as usize
    );
    status
}

async fn drive_switchover(
    api: &KvClusterApi,
    state: &ReconcilerState,
    name: &str,
    replicas: i32,
    mut status: KubericSetStatus,
) -> KubericSetStatus {
    for _ in 0..80 {
        reconcile_set(&make_set(name, replicas, Some(status.clone())), api, state)
            .await
            .unwrap();
        status = api.last_status().unwrap();
        if status.phase == Phase::Healthy {
            return status;
        }
    }
    panic!("durable switchover did not reach a terminal healthy status");
}

async fn drive_add_replica(
    api: &KvClusterApi,
    state: &ReconcilerState,
    name: &str,
    replicas: i32,
    mut status: KubericSetStatus,
) -> KubericSetStatus {
    for _ in 0..120 {
        reconcile_set(&make_set(name, replicas, Some(status.clone())), api, state)
            .await
            .unwrap();
        status = api.last_status().unwrap();
        if status.phase == Phase::Healthy
            && status
                .stable_snapshot
                .as_ref()
                .is_some_and(|snapshot| snapshot.members.len() == replicas as usize)
        {
            reconcile_set(&make_set(name, replicas, Some(status.clone())), api, state)
                .await
                .unwrap();
            return status;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("durable replica add did not reach a terminal healthy status");
}

async fn advance_until_pending_action(
    api: &KvClusterApi,
    name: &str,
    replicas: i32,
    mut status: KubericSetStatus,
    kind: DurableActionKind,
) -> KubericSetStatus {
    for _ in 0..60 {
        if status
            .operation
            .as_ref()
            .and_then(|operation| operation.pending_action.as_ref())
            .is_some_and(|action| action.kind == kind)
        {
            return status;
        }

        reconcile_set(
            &make_set(name, replicas, Some(status.clone())),
            api,
            &ReconcilerState::default(),
        )
        .await
        .unwrap();
        status = api.last_status().unwrap();
    }
    panic!("durable switchover did not reach pending action {kind:?}");
}

async fn advance_add_until_phase(
    api: &KvClusterApi,
    state: &ReconcilerState,
    name: &str,
    replicas: i32,
    mut status: KubericSetStatus,
    phase: DurableOperationPhase,
) -> KubericSetStatus {
    for _ in 0..100 {
        if status
            .operation
            .as_ref()
            .is_some_and(|operation| operation.phase == phase && operation.pending_action.is_none())
        {
            return status;
        }
        reconcile_set(&make_set(name, replicas, Some(status.clone())), api, state)
            .await
            .unwrap();
        status = api.last_status().unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("durable add did not reach phase {phase:?}");
}

async fn drive_operation_to_healthy(
    api: &KvClusterApi,
    name: &str,
    replicas: i32,
    mut status: KubericSetStatus,
) -> KubericSetStatus {
    for _ in 0..120 {
        reconcile_set(
            &make_set(name, replicas, Some(status.clone())),
            api,
            &ReconcilerState::default(),
        )
        .await
        .unwrap();
        status = api.last_status().unwrap();
        if status.phase == Phase::Healthy {
            return status;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("durable operation did not return to Healthy: {status:?}");
}

fn control_for_add_action(kind: DurableActionKind) -> Option<ControlOperation> {
    match kind {
        DurableActionKind::RetireOldReplica | DurableActionKind::CompensateRemoveCandidate => {
            Some(ControlOperation::RemoveReplica)
        }
        DurableActionKind::OpenCandidate => Some(ControlOperation::Open),
        DurableActionKind::UpdateCandidateEpoch => Some(ControlOperation::UpdateEpoch),
        DurableActionKind::AssignCandidateIdle
        | DurableActionKind::AssignCandidateActive
        | DurableActionKind::CompensateDemoteCandidate => Some(ControlOperation::ChangeRole),
        DurableActionKind::BuildCandidate => Some(ControlOperation::BuildReplica),
        DurableActionKind::AddCatchUpConfiguration => {
            Some(ControlOperation::UpdateCatchUpConfiguration)
        }
        DurableActionKind::AddWaitForCatchUpQuorum => Some(ControlOperation::WaitForCatchUpQuorum),
        DurableActionKind::AddCurrentConfiguration
        | DurableActionKind::CompensateRestoreConfiguration => {
            Some(ControlOperation::UpdateCurrentConfiguration)
        }
        DurableActionKind::CompensateCloseCandidate => Some(ControlOperation::Close),
        _ => None,
    }
}

async fn drive_operation_with_lost_replies(
    api: &KvClusterApi,
    name: &str,
    replicas: i32,
    mut status: KubericSetStatus,
    expected: &[DurableActionKind],
) -> (KubericSetStatus, Vec<DurableActionKind>) {
    let mut injected = Vec::new();
    for _ in 0..120 {
        if let Some(pending) = status
            .operation
            .as_ref()
            .and_then(|operation| operation.pending_action.as_ref())
            && pending.attempts == 0
            && expected.contains(&pending.kind)
            && !injected.contains(&pending.kind)
            && let Some(control) = control_for_add_action(pending.kind)
        {
            api.fail_after_next_durable_action(control);
            injected.push(pending.kind);
        }
        reconcile_set(
            &make_set(name, replicas, Some(status.clone())),
            api,
            &ReconcilerState::default(),
        )
        .await
        .unwrap();
        status = api.last_status().unwrap();
        if status.phase == Phase::Healthy {
            return (status, injected);
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("durable operation with lost replies did not return to Healthy");
}

async fn live_replica_statuses(
    api: &KvClusterApi,
    set_name: &str,
    replicas: i32,
) -> Vec<ReplicaStatusInfo> {
    let set = make_set(set_name, replicas, api.last_status());
    let pods = api.pods.lock().unwrap().clone();
    let mut statuses = Vec::new();
    for pod in pods {
        let replica_id = pod
            .metadata
            .labels
            .as_ref()
            .and_then(|labels| labels.get("kuberic.io/pod-index"))
            .unwrap()
            .parse::<i64>()
            .unwrap()
            + 1;
        let handle = api
            .create_replica_handle(replica_id, &pod, &set.spec)
            .await
            .unwrap();
        statuses.push(handle.get_status().await.unwrap());
    }
    statuses
}

fn assert_status_reads_only(operations: &[ControlOperation]) {
    assert!(
        !operations.is_empty(),
        "recovery should query at least one runtime status"
    );
    for forbidden in [
        ControlOperation::Open,
        ControlOperation::Close,
        ControlOperation::ChangeRole,
        ControlOperation::UpdateEpoch,
        ControlOperation::UpdateCatchUpConfiguration,
        ControlOperation::UpdateCurrentConfiguration,
        ControlOperation::WaitForCatchUpQuorum,
        ControlOperation::BuildReplica,
        ControlOperation::RemoveReplica,
        ControlOperation::OnDataLoss,
        ControlOperation::RevokeWriteStatus,
    ] {
        assert!(
            !operations.contains(&forbidden),
            "recovery invoked mutating RPC {forbidden:?}: {operations:?}"
        );
    }

    assert!(
        operations
            .iter()
            .all(|operation| *operation == ControlOperation::GetStatus)
    );
}

fn assert_stable_snapshot(api: &KvClusterApi, status: &KubericSetStatus, member_count: usize) {
    let snapshot = status
        .stable_snapshot
        .as_ref()
        .expect("stable operation must persist a snapshot");
    assert_eq!(snapshot.epoch, status.epoch);
    assert_eq!(snapshot.members.len(), member_count);
    assert_eq!(snapshot.write_quorum, member_count as u32 / 2 + 1);
    assert_eq!(
        snapshot
            .members
            .iter()
            .filter(|member| member.role == StableReplicaRoleStatus::Primary)
            .count(),
        1
    );
    assert_eq!(
        snapshot
            .members
            .iter()
            .find(|member| member.role == StableReplicaRoleStatus::Primary)
            .unwrap()
            .id,
        snapshot.primary_id
    );

    let pods = api.pods.lock().unwrap();
    let mut member_ids = HashSet::new();
    let mut member_instances = HashSet::new();
    for member in &snapshot.members {
        assert!(member_ids.insert(member.id), "duplicate snapshot member ID");
        assert!(
            member_instances.insert(member.instance_id.as_str()),
            "duplicate snapshot member incarnation"
        );
        let pod = pods
            .iter()
            .find(|pod| {
                pod.metadata
                    .labels
                    .as_ref()
                    .and_then(|labels| labels.get("kuberic.io/pod-index"))
                    .and_then(|index| index.parse::<i64>().ok())
                    .map(|index| index + 1)
                    == Some(member.id)
            })
            .expect("every snapshot member must have a current pod");
        assert_eq!(
            member.instance_id,
            pod.metadata.uid.as_deref().unwrap(),
            "snapshot incarnation must exactly match current pod UID"
        );
        let expected_role = if member.id == snapshot.primary_id {
            StableReplicaRoleStatus::Primary
        } else {
            StableReplicaRoleStatus::ActiveSecondary
        };
        assert_eq!(member.role, expected_role);
    }
}

#[test_log::test(tokio::test)]
#[serial]
async fn test_operator_restart_recovers_read_only_then_switches_and_scales() {
    let api = KvClusterApi::new();
    let initial_state = ReconcilerState::default();
    let status = create_healthy_set(&api, &initial_state, "recover", 3).await;
    let original_primary = status.current_primary.clone().unwrap();

    let mut kv = connect_kv(&api.client_address(&original_primary).unwrap()).await;
    kv.put(proto::PutRequest {
        key: "before-restart".into(),
        value: "durable".into(),
    })
    .await
    .unwrap();

    // Lose only operator process memory. Pods and their real runtimes remain.
    let recovered_state = ReconcilerState::default();
    api.reset_operations();
    reconcile_set(
        &make_set("recover", 3, Some(status.clone())),
        &api,
        &recovered_state,
    )
    .await
    .unwrap();
    assert_status_reads_only(&api.operations());

    // Recovery neither reopens nor changes the existing replicas.
    kv.put(proto::PutRequest {
        key: "after-restart".into(),
        value: "still-open".into(),
    })
    .await
    .unwrap();

    let target = api
        .pods
        .lock()
        .unwrap()
        .iter()
        .map(|pod| pod.metadata.name.clone().unwrap())
        .find(|pod_name| pod_name != &original_primary)
        .unwrap();
    reconcile_set(
        &make_set(
            "recover",
            3,
            Some(KubericSetStatus {
                target_primary: Some(target.clone()),
                ..status
            }),
        ),
        &api,
        &recovered_state,
    )
    .await
    .unwrap();
    let switching = api.last_status().unwrap();
    assert_eq!(switching.phase, Phase::Switchover);
    let switched = drive_switchover(&api, &recovered_state, "recover", 3, switching).await;
    assert_eq!(switched.current_primary.as_deref(), Some(target.as_str()));
    assert_eq!(
        switched.stable_snapshot.as_ref().unwrap().primary_id,
        switched
            .stable_snapshot
            .as_ref()
            .unwrap()
            .members
            .iter()
            .find(|member| member.instance_id
                == api
                    .pods
                    .lock()
                    .unwrap()
                    .iter()
                    .find(|pod| pod.metadata.name.as_deref() == Some(target.as_str()))
                    .unwrap()
                    .metadata
                    .uid
                    .clone()
                    .unwrap())
            .unwrap()
            .id
    );

    // First loop creates the fourth pod; the next commits and persists it.
    let scale_request = make_set("recover", 4, Some(switched.clone()));
    reconcile_set(&scale_request, &api, &recovered_state)
        .await
        .unwrap();
    let uncommitted_target = api
        .pods
        .lock()
        .unwrap()
        .iter()
        .find(|pod| {
            pod.metadata
                .labels
                .as_ref()
                .and_then(|labels| labels.get("kuberic.io/pod-index"))
                .map(String::as_str)
                == Some("3")
        })
        .unwrap()
        .metadata
        .name
        .clone()
        .unwrap();
    let error = reconcile_set(
        &make_set(
            "recover",
            4,
            Some(KubericSetStatus {
                target_primary: Some(uncommitted_target),
                ..switched.clone()
            }),
        ),
        &api,
        &recovered_state,
    )
    .await
    .err()
    .expect("switchover to an uncommitted scale-up pod must fail");
    assert!(error.contains("not in the committed driver topology"));
    api.mark_all_pods_ready();
    let scaled = drive_add_replica(
        &api,
        &recovered_state,
        "recover",
        4,
        api.last_status().unwrap(),
    )
    .await;
    assert_eq!(scaled.stable_snapshot.as_ref().unwrap().members.len(), 4);
    assert_eq!(scaled.stable_snapshot.as_ref().unwrap().write_quorum, 3);

    let mut new_primary = connect_kv(&api.client_address(&target).unwrap()).await;
    new_primary
        .put(proto::PutRequest {
            key: "after-scale".into(),
            value: "works".into(),
        })
        .await
        .unwrap();
}

#[test_log::test(tokio::test)]
#[serial]
async fn test_operator_restart_recovers_unhealthy_primary_then_fails_over() {
    let api = KvClusterApi::new();
    let initial_state = ReconcilerState::default();
    let status = create_healthy_set(&api, &initial_state, "recover-failover", 3).await;
    let failed_primary = status.current_primary.clone().unwrap();
    api.mark_pod_not_ready(&failed_primary);

    let recovered_state = ReconcilerState::default();
    api.reset_operations();
    reconcile_set(
        &make_set("recover-failover", 3, Some(status)),
        &api,
        &recovered_state,
    )
    .await
    .unwrap();
    assert_status_reads_only(&api.operations());
    let failing = api.last_status().unwrap();
    assert_eq!(failing.phase, Phase::FailingOver);

    reconcile_set(
        &make_set("recover-failover", 3, Some(failing)),
        &api,
        &recovered_state,
    )
    .await
    .unwrap();
    let recovered = api.last_status().unwrap();
    assert_eq!(recovered.phase, Phase::Healthy);
    assert_ne!(
        recovered.current_primary.as_deref(),
        Some(failed_primary.as_str())
    );
    assert_eq!(recovered.stable_snapshot.as_ref().unwrap().members.len(), 2);

    let mut kv = connect_kv(
        &api.client_address(recovered.current_primary.as_deref().unwrap())
            .unwrap(),
    )
    .await;
    kv.put(proto::PutRequest {
        key: "recovered-failover".into(),
        value: "works".into(),
    })
    .await
    .unwrap();
}

#[test_log::test(tokio::test)]
#[serial]
async fn test_recovery_rejects_legacy_and_persisted_live_mismatch_without_mutation() {
    let api = KvClusterApi::new();
    let initial_state = ReconcilerState::default();
    let status = create_healthy_set(&api, &initial_state, "reject-recovery", 3).await;
    let status_count = api.statuses.lock().unwrap().len();

    let mut legacy = status.clone();
    legacy.stable_snapshot = None;
    api.reset_operations();
    let error = reconcile_set(
        &make_set("reject-recovery", 3, Some(legacy)),
        &api,
        &ReconcilerState::default(),
    )
    .await
    .err()
    .expect("legacy recovery should fail");
    assert!(error.contains("stable snapshot is absent"));
    assert!(api.operations().is_empty());
    assert_eq!(api.statuses.lock().unwrap().len(), status_count);

    let mut mismatched = status;
    mismatched.stable_snapshot.as_mut().unwrap().members[0].instance_id =
        "persisted-stale-incarnation".into();
    api.reset_operations();
    let error = reconcile_set(
        &make_set("reject-recovery", 3, Some(mismatched)),
        &api,
        &ReconcilerState::default(),
    )
    .await
    .err()
    .expect("mismatched recovery should fail");
    assert!(error.contains("handle incarnation mismatch"));
    assert!(api.operations().is_empty());
    assert_eq!(api.statuses.lock().unwrap().len(), status_count);
}

#[test_log::test(tokio::test)]
#[serial]
async fn test_post_recovery_pod_identity_drift_is_rejected_before_rpc() {
    let api = KvClusterApi::new();
    let initial_state = ReconcilerState::default();
    let status = create_healthy_set(&api, &initial_state, "identity-drift", 3).await;
    let recovered_state = ReconcilerState::default();
    reconcile_set(
        &make_set("identity-drift", 3, Some(status.clone())),
        &api,
        &recovered_state,
    )
    .await
    .unwrap();

    {
        let mut pods = api.pods.lock().unwrap();
        pods[0]
            .metadata
            .labels
            .as_mut()
            .unwrap()
            .insert("kuberic.io/pod-index".into(), "9".into());
    }
    api.reset_operations();
    let error = reconcile_set(
        &make_set("identity-drift", 3, Some(status.clone())),
        &api,
        &recovered_state,
    )
    .await
    .err()
    .expect("logical identity drift should fail");
    assert!(error.contains("current pod for driver replica"));
    assert!(api.operations().is_empty());

    {
        let mut pods = api.pods.lock().unwrap();
        pods[0]
            .metadata
            .labels
            .as_mut()
            .unwrap()
            .insert("kuberic.io/pod-index".into(), "0".into());
        pods[0].metadata.uid = Some("replacement-incarnation".into());
    }
    api.reset_operations();
    let error = reconcile_set(
        &make_set("identity-drift", 3, Some(status)),
        &api,
        &recovered_state,
    )
    .await
    .err()
    .expect("incarnation drift should fail");
    assert!(error.contains("current pod incarnation drift"));
    assert!(api.operations().is_empty());
}

#[test_log::test(tokio::test)]
#[serial]
async fn test_creation_uses_pod_labels_when_list_is_unordered() {
    let api = KvClusterApi::new();
    let state = ReconcilerState::default();
    reconcile_set(&make_set("unordered", 3, None), &api, &state)
        .await
        .unwrap();
    api.pods.lock().unwrap().reverse();
    api.mark_all_pods_ready();

    reconcile_set(
        &make_set(
            "unordered",
            3,
            Some(KubericSetStatus {
                phase: Phase::Creating,
                ..Default::default()
            }),
        ),
        &api,
        &state,
    )
    .await
    .unwrap();
    let status = api.last_status().unwrap();
    assert_eq!(status.current_primary.as_deref(), Some("unordered-0"));
    assert_eq!(status.stable_snapshot.as_ref().unwrap().primary_id, 1);
}

#[test_log::test(tokio::test)]
#[serial]
async fn test_committed_topology_retries_status_before_another_mutation() {
    let api = KvClusterApi::new();
    let state = ReconcilerState::default();
    let status = create_healthy_set(&api, &state, "persist-retry", 1).await;

    // Create two scale-up pods, then fail persistence after the first one's
    // current configuration is committed.
    let scale_request = make_set("persist-retry", 3, Some(status.clone()));
    reconcile_set(&scale_request, &api, &state).await.unwrap();
    api.mark_all_pods_ready();
    reconcile_set(&scale_request, &api, &state).await.unwrap();
    let finalizing = advance_add_until_phase(
        &api,
        &state,
        "persist-retry",
        3,
        api.last_status().unwrap(),
        DurableOperationPhase::AddFinalize,
    )
    .await;
    api.fail_next_status_patch();
    let error = reconcile_set(
        &make_set("persist-retry", 3, Some(finalizing.clone())),
        &api,
        &state,
    )
    .await
    .err()
    .expect("injected status persistence failure must be returned");
    assert_eq!(error, "injected status persistence failure");

    // The next reconcile retries only the pending status. It must not add the
    // third replica until the first committed add is durable.
    api.reset_operations();
    reconcile_set(
        &make_set("persist-retry", 3, Some(finalizing)),
        &api,
        &state,
    )
    .await
    .unwrap();
    assert!(api.operations().is_empty());
    let persisted_first_add = api.last_status().unwrap();
    assert_stable_snapshot(&api, &persisted_first_add, 2);

    let completed = drive_add_replica(&api, &state, "persist-retry", 3, persisted_first_add).await;
    assert_stable_snapshot(&api, &completed, 3);
}

#[test_log::test(tokio::test)]
#[serial]
async fn test_creation_status_retry_does_not_reopen_replicas() {
    let api = KvClusterApi::new();
    let state = ReconcilerState::default();
    reconcile_set(&make_set("create-persist-retry", 1, None), &api, &state)
        .await
        .unwrap();
    api.mark_all_pods_ready();
    let creating = make_set(
        "create-persist-retry",
        1,
        Some(KubericSetStatus {
            phase: Phase::Creating,
            ..Default::default()
        }),
    );

    api.fail_next_status_patch();
    let error = reconcile_set(&creating, &api, &state)
        .await
        .err()
        .expect("injected creation status failure must be returned");
    assert_eq!(error, "injected status persistence failure");

    // If the operator process is lost too, the new state has no pending
    // status. Creating-phase preflight observes the already-open runtime and
    // fails closed using GetStatus only.
    api.reset_operations();
    let error = reconcile_set(&creating, &api, &ReconcilerState::default())
        .await
        .err()
        .expect("creation replay after process loss must fail closed");
    assert!(error.contains("refusing to replay creation"));
    assert_status_reads_only(&api.operations());

    // With the original process state intact, retry only the durable status.
    api.reset_operations();
    reconcile_set(&creating, &api, &state).await.unwrap();
    assert!(
        api.operations().is_empty(),
        "status retry must not replay Open or any creation RPC"
    );
    let recovered_status = api.last_status().unwrap();
    assert_eq!(recovered_status.phase, Phase::Healthy);
    assert_stable_snapshot(&api, &recovered_status, 1);
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
        Some(KubericSetStatus {
            phase: Phase::Creating,
            ..Default::default()
        }),
    );
    reconcile_set(&set, &api, &state).await.unwrap();

    let status = api.last_status().unwrap();
    assert_eq!(status.phase, Phase::Healthy);
    assert!(status.current_primary.is_some());
    assert_stable_snapshot(&api, &status, 3);

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
        Some(KubericSetStatus {
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
        Some(KubericSetStatus {
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
    let status = drive_switchover(&api, &state, "myapp", 3, status).await;
    assert_eq!(status.phase, Phase::Healthy);
    assert_eq!(
        status.current_primary.as_deref(),
        Some(target_name.as_str())
    );
    assert_stable_snapshot(&api, &status, 3);
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

#[test_log::test(tokio::test)]
#[serial]
async fn test_durable_switchover_survives_state_loss_at_every_boundary() {
    let api = KvClusterApi::new();
    let initial_state = ReconcilerState::default();
    let status = create_healthy_set(&api, &initial_state, "durable-boundaries", 3).await;
    let original_primary = status.current_primary.clone().unwrap();
    let target = api
        .pods
        .lock()
        .unwrap()
        .iter()
        .map(|pod| pod.metadata.name.clone().unwrap())
        .find(|name| name != &original_primary)
        .unwrap();

    reconcile_set(
        &make_set(
            "durable-boundaries",
            3,
            Some(KubericSetStatus {
                target_primary: Some(target.clone()),
                ..status
            }),
        ),
        &api,
        &initial_state,
    )
    .await
    .unwrap();
    let mut status = api.last_status().unwrap();
    let operation_id = status.operation.as_ref().unwrap().operation_id.clone();
    api.reset_operations();

    for _ in 0..80 {
        reconcile_set(
            &make_set("durable-boundaries", 3, Some(status.clone())),
            &api,
            &ReconcilerState::default(),
        )
        .await
        .unwrap();
        status = api.last_status().unwrap();
        if status.phase == Phase::Healthy {
            break;
        }
    }

    assert_eq!(status.phase, Phase::Healthy);
    assert_eq!(status.current_primary.as_deref(), Some(target.as_str()));
    assert_eq!(
        status.operation.as_ref().unwrap().phase,
        DurableOperationPhase::Completed
    );
    assert_eq!(
        status.operation.as_ref().unwrap().operation_id,
        operation_id
    );
    assert_stable_snapshot(&api, &status, 3);
    let live = live_replica_statuses(&api, "durable-boundaries", 3).await;
    assert_eq!(
        live.iter()
            .filter(|status| status.role == Role::Primary)
            .count(),
        1
    );
    assert_eq!(
        live.iter()
            .filter(|status| status.write_status == kuberic_core::types::AccessStatus::Granted)
            .count(),
        1
    );

    let operations = api.operations();
    assert_eq!(
        operations
            .iter()
            .filter(|operation| **operation == ControlOperation::RevokeWriteStatus)
            .count(),
        1
    );
    assert_eq!(
        operations
            .iter()
            .filter(|operation| **operation == ControlOperation::ChangeRole)
            .count(),
        2
    );
}

#[test_log::test(tokio::test)]
#[serial]
async fn test_durable_switchover_observes_lost_promotion_reply_without_duplicate() {
    let api = KvClusterApi::new();
    let state = ReconcilerState::default();
    let status = create_healthy_set(&api, &state, "durable-ambiguous", 3).await;
    let original_primary = status.current_primary.clone().unwrap();
    let target = api
        .pods
        .lock()
        .unwrap()
        .iter()
        .map(|pod| pod.metadata.name.clone().unwrap())
        .find(|name| name != &original_primary)
        .unwrap();
    reconcile_set(
        &make_set(
            "durable-ambiguous",
            3,
            Some(KubericSetStatus {
                target_primary: Some(target.clone()),
                ..status
            }),
        ),
        &api,
        &state,
    )
    .await
    .unwrap();
    let switching = advance_until_pending_action(
        &api,
        "durable-ambiguous",
        3,
        api.last_status().unwrap(),
        DurableActionKind::PromoteTarget,
    )
    .await;

    api.reset_operations();
    api.fail_after_next_durable_action(ControlOperation::ChangeRole);
    reconcile_set(
        &make_set("durable-ambiguous", 3, Some(switching)),
        &api,
        &ReconcilerState::default(),
    )
    .await
    .unwrap();
    let after_lost_reply = api.last_status().unwrap();
    assert_eq!(
        after_lost_reply
            .operation
            .as_ref()
            .unwrap()
            .pending_action
            .as_ref()
            .unwrap()
            .attempts,
        1
    );

    let completed = drive_switchover(
        &api,
        &ReconcilerState::default(),
        "durable-ambiguous",
        3,
        after_lost_reply,
    )
    .await;
    assert_eq!(completed.current_primary.as_deref(), Some(target.as_str()));
    assert_eq!(
        api.operations()
            .iter()
            .filter(|operation| **operation == ControlOperation::ChangeRole)
            .count(),
        1,
        "the ambiguous target promotion must not be dispatched twice"
    );
}

#[test_log::test(tokio::test)]
#[serial]
async fn test_durable_switchover_observes_every_runtime_lost_reply_window() {
    let api = KvClusterApi::new();
    let state = ReconcilerState::default();
    let status = create_healthy_set(&api, &state, "durable-all-ambiguous", 3).await;
    let original_primary = status.current_primary.clone().unwrap();
    let target = api
        .pods
        .lock()
        .unwrap()
        .iter()
        .map(|pod| pod.metadata.name.clone().unwrap())
        .find(|name| name != &original_primary)
        .unwrap();
    reconcile_set(
        &make_set(
            "durable-all-ambiguous",
            3,
            Some(KubericSetStatus {
                target_primary: Some(target.clone()),
                ..status
            }),
        ),
        &api,
        &state,
    )
    .await
    .unwrap();
    api.reset_operations();
    let mut status = api.last_status().unwrap();
    let mut injected = Vec::new();

    for _ in 0..100 {
        if let Some(pending) = status
            .operation
            .as_ref()
            .and_then(|operation| operation.pending_action.as_ref())
            && pending.attempts == 0
            && !injected.contains(&pending.kind)
        {
            let control = match pending.kind {
                DurableActionKind::RevokeWrite => Some(ControlOperation::RevokeWriteStatus),
                DurableActionKind::DemoteOldPrimary | DurableActionKind::PromoteTarget => {
                    Some(ControlOperation::ChangeRole)
                }
                DurableActionKind::UpdateSecondaryEpoch => Some(ControlOperation::UpdateEpoch),
                DurableActionKind::UpdateCatchUpConfiguration => {
                    Some(ControlOperation::UpdateCatchUpConfiguration)
                }
                DurableActionKind::WaitForCatchUpQuorum => {
                    Some(ControlOperation::WaitForCatchUpQuorum)
                }
                DurableActionKind::UpdateCurrentConfiguration => {
                    Some(ControlOperation::UpdateCurrentConfiguration)
                }
                _ => None,
            };
            if let Some(control) = control {
                api.fail_after_next_durable_action(control);
                injected.push(pending.kind);
            }
        }

        reconcile_set(
            &make_set("durable-all-ambiguous", 3, Some(status.clone())),
            &api,
            &ReconcilerState::default(),
        )
        .await
        .unwrap();
        status = api.last_status().unwrap();
        if status.phase == Phase::Healthy {
            break;
        }
    }

    assert_eq!(status.phase, Phase::Healthy);
    assert_eq!(status.current_primary.as_deref(), Some(target.as_str()));
    for kind in [
        DurableActionKind::RevokeWrite,
        DurableActionKind::DemoteOldPrimary,
        DurableActionKind::PromoteTarget,
        DurableActionKind::UpdateSecondaryEpoch,
        DurableActionKind::UpdateCatchUpConfiguration,
        DurableActionKind::WaitForCatchUpQuorum,
        DurableActionKind::UpdateCurrentConfiguration,
    ] {
        assert!(
            injected.contains(&kind),
            "missing lost-reply window {kind:?}"
        );
    }
    let operations = api.operations();
    assert_eq!(
        operations
            .iter()
            .filter(|operation| **operation == ControlOperation::RevokeWriteStatus)
            .count(),
        1
    );
    assert_eq!(
        operations
            .iter()
            .filter(|operation| **operation == ControlOperation::ChangeRole)
            .count(),
        2
    );
    assert_eq!(
        operations
            .iter()
            .filter(|operation| **operation == ControlOperation::UpdateEpoch)
            .count(),
        1
    );
    assert_eq!(
        operations
            .iter()
            .filter(|operation| **operation == ControlOperation::UpdateCatchUpConfiguration)
            .count(),
        1
    );
    assert_eq!(
        operations
            .iter()
            .filter(|operation| **operation == ControlOperation::WaitForCatchUpQuorum)
            .count(),
        1
    );
    assert_eq!(
        operations
            .iter()
            .filter(|operation| **operation == ControlOperation::UpdateCurrentConfiguration)
            .count(),
        1
    );
}

#[test_log::test(tokio::test)]
#[serial]
async fn test_durable_switchover_compensates_failed_target_promotion() {
    let api = KvClusterApi::new();
    let state = ReconcilerState::default();
    let status = create_healthy_set(&api, &state, "durable-compensate", 3).await;
    let original_primary = status.current_primary.clone().unwrap();
    let target = api
        .pods
        .lock()
        .unwrap()
        .iter()
        .map(|pod| pod.metadata.name.clone().unwrap())
        .find(|name| name != &original_primary)
        .unwrap();
    reconcile_set(
        &make_set(
            "durable-compensate",
            3,
            Some(KubericSetStatus {
                target_primary: Some(target),
                ..status
            }),
        ),
        &api,
        &state,
    )
    .await
    .unwrap();
    let switching = advance_until_pending_action(
        &api,
        "durable-compensate",
        3,
        api.last_status().unwrap(),
        DurableActionKind::PromoteTarget,
    )
    .await;

    api.fail_before_next_durable_action(ControlOperation::ChangeRole);
    reconcile_set(
        &make_set("durable-compensate", 3, Some(switching)),
        &api,
        &ReconcilerState::default(),
    )
    .await
    .unwrap();
    let mut failed = api.last_status().unwrap();
    failed
        .operation
        .as_mut()
        .unwrap()
        .pending_action
        .as_mut()
        .unwrap()
        .deadline_unix_seconds = 0;
    reconcile_set(
        &make_set("durable-compensate", 3, Some(failed)),
        &api,
        &ReconcilerState::default(),
    )
    .await
    .unwrap();

    let completed = drive_switchover(
        &api,
        &ReconcilerState::default(),
        "durable-compensate",
        3,
        api.last_status().unwrap(),
    )
    .await;
    assert_eq!(
        completed.current_primary.as_deref(),
        Some(original_primary.as_str())
    );
    assert_eq!(
        completed.operation.as_ref().unwrap().phase,
        DurableOperationPhase::Failed
    );
    assert_stable_snapshot(&api, &completed, 3);

    api.reset_operations();
    reconcile_set(
        &make_set("durable-compensate", 3, Some(completed)),
        &api,
        &ReconcilerState::default(),
    )
    .await
    .unwrap();
    assert_status_reads_only(&api.operations());
}

#[test_log::test(tokio::test)]
#[serial]
async fn test_durable_switchover_restores_writes_when_demotion_never_runs() {
    let api = KvClusterApi::new();
    let state = ReconcilerState::default();
    let status = create_healthy_set(&api, &state, "durable-safe-abort", 3).await;
    let original_primary = status.current_primary.clone().unwrap();
    let original_epoch = status.epoch.clone();
    let target = api
        .pods
        .lock()
        .unwrap()
        .iter()
        .map(|pod| pod.metadata.name.clone().unwrap())
        .find(|name| name != &original_primary)
        .unwrap();
    reconcile_set(
        &make_set(
            "durable-safe-abort",
            3,
            Some(KubericSetStatus {
                target_primary: Some(target),
                ..status
            }),
        ),
        &api,
        &state,
    )
    .await
    .unwrap();
    let switching = advance_until_pending_action(
        &api,
        "durable-safe-abort",
        3,
        api.last_status().unwrap(),
        DurableActionKind::DemoteOldPrimary,
    )
    .await;

    api.fail_before_next_durable_action(ControlOperation::ChangeRole);
    reconcile_set(
        &make_set("durable-safe-abort", 3, Some(switching)),
        &api,
        &ReconcilerState::default(),
    )
    .await
    .unwrap();
    let mut failed = api.last_status().unwrap();
    failed
        .operation
        .as_mut()
        .unwrap()
        .pending_action
        .as_mut()
        .unwrap()
        .deadline_unix_seconds = 0;
    reconcile_set(
        &make_set("durable-safe-abort", 3, Some(failed)),
        &api,
        &ReconcilerState::default(),
    )
    .await
    .unwrap();
    let completed = drive_switchover(
        &api,
        &ReconcilerState::default(),
        "durable-safe-abort",
        3,
        api.last_status().unwrap(),
    )
    .await;
    assert_eq!(
        completed.current_primary.as_deref(),
        Some(original_primary.as_str())
    );
    assert_eq!(completed.epoch, original_epoch);
    assert_eq!(
        completed.operation.as_ref().unwrap().phase,
        DurableOperationPhase::Failed
    );
    let live = live_replica_statuses(&api, "durable-safe-abort", 3).await;
    assert_eq!(
        live.iter()
            .filter(|status| status.write_status == kuberic_core::types::AccessStatus::Granted)
            .count(),
        1
    );
}

#[test_log::test(tokio::test)]
#[serial]
async fn test_durable_switchover_conflict_and_invalid_checkpoint_do_not_mutate() {
    let api = KvClusterApi::new();
    let state = ReconcilerState::default();
    let status = create_healthy_set(&api, &state, "durable-conflict", 3).await;
    let original_primary = status.current_primary.clone().unwrap();
    let target = api
        .pods
        .lock()
        .unwrap()
        .iter()
        .map(|pod| pod.metadata.name.clone().unwrap())
        .find(|name| name != &original_primary)
        .unwrap();

    api.reset_operations();
    api.fail_next_status_conflict();
    let result = reconcile_set(
        &make_set(
            "durable-conflict",
            3,
            Some(KubericSetStatus {
                target_primary: Some(target.clone()),
                ..status.clone()
            }),
        ),
        &api,
        &state,
    )
    .await;
    assert_eq!(result.err().as_deref(), Some("resource version conflict"));
    assert!(
        api.operations()
            .iter()
            .all(|operation| *operation == ControlOperation::GetStatus)
    );
    assert!(
        state
            .drivers
            .lock()
            .await
            .contains_key("default/durable-conflict")
    );

    reconcile_set(
        &make_set(
            "durable-conflict",
            3,
            Some(KubericSetStatus {
                target_primary: Some(target),
                ..status
            }),
        ),
        &api,
        &state,
    )
    .await
    .unwrap();
    let mut invalid = api.last_status().unwrap();
    invalid.operation.as_mut().unwrap().version += 1;
    api.reset_operations();
    reconcile_set(
        &make_set("durable-conflict", 3, Some(invalid)),
        &api,
        &ReconcilerState::default(),
    )
    .await
    .unwrap();
    assert!(
        api.operations()
            .iter()
            .all(|operation| *operation == ControlOperation::GetStatus)
    );
}

#[test_log::test(tokio::test)]
#[serial]
async fn test_durable_switchover_rejects_incarnation_drift_before_mutation() {
    let api = KvClusterApi::new();
    let state = ReconcilerState::default();
    let status = create_healthy_set(&api, &state, "durable-stale", 3).await;
    let original_primary = status.current_primary.clone().unwrap();
    let target = api
        .pods
        .lock()
        .unwrap()
        .iter()
        .map(|pod| pod.metadata.name.clone().unwrap())
        .find(|name| name != &original_primary)
        .unwrap();
    reconcile_set(
        &make_set(
            "durable-stale",
            3,
            Some(KubericSetStatus {
                target_primary: Some(target.clone()),
                ..status
            }),
        ),
        &api,
        &state,
    )
    .await
    .unwrap();
    let switching = api.last_status().unwrap();
    api.pods
        .lock()
        .unwrap()
        .iter_mut()
        .find(|pod| pod.metadata.name.as_deref() == Some(target.as_str()))
        .unwrap()
        .metadata
        .uid = Some("replacement-incarnation".to_string());

    api.reset_operations();
    reconcile_set(
        &make_set("durable-stale", 3, Some(switching)),
        &api,
        &ReconcilerState::default(),
    )
    .await
    .unwrap();
    assert!(api.operations().is_empty());
    let poisoned = api.last_status().unwrap();
    assert_eq!(
        poisoned.operation.as_ref().unwrap().phase,
        DurableOperationPhase::Poisoned
    );
    assert!(
        poisoned
            .operation
            .as_ref()
            .unwrap()
            .last_error
            .as_deref()
            .unwrap()
            .contains("incarnation changed")
    );
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
        Some(KubericSetStatus {
            phase: Phase::Creating,
            ..Default::default()
        }),
    );
    let result = reconcile_set(&set, &api, &state).await.unwrap();

    // Should requeue (waiting for pods to become ready)
    assert!(
        matches!(
            result,
            kuberic_operator::reconciler::ReconcileAction::Requeue(_)
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
        Some(KubericSetStatus {
            phase: Phase::Creating,
            ..Default::default()
        }),
    );
    reconcile_set(&set, &api, &state).await.unwrap();

    let status = api.last_status().unwrap();
    assert_eq!(status.phase, Phase::Healthy);
    assert!(
        status
            .members
            .iter()
            .all(|member| !member.instance_id.is_empty())
    );
    {
        let drivers = state.drivers.lock().await;
        let driver = drivers.get("default/myapp").unwrap();
        for member in &status.members {
            let handle = driver.handle(member.id).unwrap();
            assert_eq!(member.instance_id, handle.instance_id().as_str());
            assert_eq!(
                handle.get_status().await.unwrap().instance_id,
                handle.instance_id()
            );
        }
    }
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

    // Crash primary (abrupt — abort tasks, gRPC breaks)
    api.crash_pod(&primary_name);

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
    assert_stable_snapshot(&api, &status, 2);

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

#[test_log::test(tokio::test)]
#[serial]
async fn test_durable_add_survives_state_loss_and_every_lost_runtime_reply() {
    let api = KvClusterApi::new();
    let initial_state = ReconcilerState::default();
    let previous = create_healthy_set(&api, &initial_state, "durable-add", 1).await;
    let previous_snapshot = previous.stable_snapshot.clone().unwrap();

    reconcile_set(
        &make_set("durable-add", 2, Some(previous.clone())),
        &api,
        &initial_state,
    )
    .await
    .unwrap();
    api.mark_all_pods_ready();
    reconcile_set(
        &make_set("durable-add", 2, Some(previous.clone())),
        &api,
        &initial_state,
    )
    .await
    .unwrap();
    let mut status = api.last_status().unwrap();
    assert_eq!(status.phase, Phase::AddingReplica);
    assert_eq!(
        status.operation.as_ref().unwrap().kind,
        DurableOperationKind::AddReplica
    );
    assert_eq!(
        status.operation.as_ref().unwrap().add_mode,
        Some(DurableAddMode::ScaleUp)
    );

    api.reset_operations();
    let mut injected = Vec::new();
    for _ in 0..120 {
        if let Some(pending) = status
            .operation
            .as_ref()
            .and_then(|operation| operation.pending_action.as_ref())
            && pending.attempts == 0
            && !injected.contains(&pending.kind)
        {
            let control = control_for_add_action(pending.kind);
            if let Some(control) = control {
                api.fail_after_next_durable_action(control);
                injected.push(pending.kind);
            }
        }

        reconcile_set(
            &make_set("durable-add", 2, Some(status.clone())),
            &api,
            &ReconcilerState::default(),
        )
        .await
        .unwrap();
        status = api.last_status().unwrap();
        if status.phase == Phase::Healthy {
            break;
        }
        assert_eq!(
            status.stable_snapshot.as_ref(),
            Some(&previous_snapshot),
            "stable snapshot changed before durable add completion"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    assert_eq!(status.phase, Phase::Healthy);
    assert_eq!(
        status.operation.as_ref().unwrap().phase,
        DurableOperationPhase::Completed
    );
    assert_stable_snapshot(&api, &status, 2);
    for kind in [
        DurableActionKind::OpenCandidate,
        DurableActionKind::UpdateCandidateEpoch,
        DurableActionKind::AssignCandidateIdle,
        DurableActionKind::BuildCandidate,
        DurableActionKind::AssignCandidateActive,
        DurableActionKind::AddCatchUpConfiguration,
        DurableActionKind::AddWaitForCatchUpQuorum,
        DurableActionKind::AddCurrentConfiguration,
    ] {
        assert!(
            injected.contains(&kind),
            "missing lost-reply window {kind:?}"
        );
    }
    let operations = api.operations();
    assert_eq!(
        operations
            .iter()
            .filter(|operation| **operation == ControlOperation::Open)
            .count(),
        1
    );
    assert_eq!(
        operations
            .iter()
            .filter(|operation| **operation == ControlOperation::BuildReplica)
            .count(),
        1,
        "ambiguous build scheduling must not start a second copy"
    );
    assert_eq!(
        operations
            .iter()
            .filter(|operation| **operation == ControlOperation::ChangeRole)
            .count(),
        2
    );
}

#[test_log::test(tokio::test)]
#[serial]
async fn test_durable_rejoin_retires_old_incarnation_once() {
    let api = KvClusterApi::new();
    let state = ReconcilerState::default();
    let previous = create_healthy_set(&api, &state, "durable-rejoin", 3).await;
    let primary = previous.current_primary.clone().unwrap();
    let target = api
        .pods
        .lock()
        .unwrap()
        .iter()
        .map(|pod| pod.metadata.name.clone().unwrap())
        .find(|name| name != &primary)
        .unwrap();
    let old_instance = previous
        .stable_snapshot
        .as_ref()
        .unwrap()
        .members
        .iter()
        .find(|member| member.id == 2)
        .map(|member| member.instance_id.clone())
        .unwrap_or_else(|| {
            previous
                .stable_snapshot
                .as_ref()
                .unwrap()
                .members
                .iter()
                .find(|member| member.id != previous.stable_snapshot.as_ref().unwrap().primary_id)
                .unwrap()
                .instance_id
                .clone()
        });

    api.crash_pod(&target);
    api.restart_pod(&target).await;
    let replacement_instance = api
        .pods
        .lock()
        .unwrap()
        .iter()
        .find(|pod| pod.metadata.name.as_deref() == Some(target.as_str()))
        .unwrap()
        .metadata
        .uid
        .clone()
        .unwrap();
    assert_ne!(old_instance, replacement_instance);

    reconcile_set(
        &make_set("durable-rejoin", 3, Some(previous.clone())),
        &api,
        &state,
    )
    .await
    .unwrap();
    let mut status = api.last_status().unwrap();
    assert_eq!(status.phase, Phase::AddingReplica);
    assert_eq!(
        status.operation.as_ref().unwrap().add_mode,
        Some(DurableAddMode::Rebuild)
    );
    assert_eq!(
        status
            .operation
            .as_ref()
            .unwrap()
            .retired_instance_id
            .as_deref(),
        Some(old_instance.as_str())
    );

    status = advance_until_pending_action(
        &api,
        "durable-rejoin",
        3,
        status,
        DurableActionKind::RetireOldReplica,
    )
    .await;
    let retire_action_id = status
        .operation
        .as_ref()
        .unwrap()
        .pending_action
        .as_ref()
        .unwrap()
        .action_id
        .clone();
    let target_replica_id = status
        .operation
        .as_ref()
        .unwrap()
        .target_replica_id
        .unwrap();
    api.reset_operations();
    api.fail_after_next_durable_action(ControlOperation::RemoveReplica);
    reconcile_set(
        &make_set("durable-rejoin", 3, Some(status)),
        &api,
        &ReconcilerState::default(),
    )
    .await
    .unwrap();
    let completed =
        drive_operation_to_healthy(&api, "durable-rejoin", 3, api.last_status().unwrap()).await;
    assert_eq!(
        api.operations()
            .iter()
            .filter(|operation| **operation == ControlOperation::RemoveReplica)
            .count(),
        1
    );
    assert!(
        completed
            .stable_snapshot
            .as_ref()
            .unwrap()
            .members
            .iter()
            .any(|member| member.instance_id == replacement_instance)
    );

    let primary_id = completed.stable_snapshot.as_ref().unwrap().primary_id;
    let primary_pod = api
        .pods
        .lock()
        .unwrap()
        .iter()
        .find(|pod| {
            pod.metadata
                .labels
                .as_ref()
                .and_then(|labels| labels.get("kuberic.io/pod-index"))
                .and_then(|index| index.parse::<i64>().ok())
                .map(|index| index + 1)
                == Some(primary_id)
        })
        .unwrap()
        .clone();
    let primary_handle = api
        .create_replica_handle(
            primary_id,
            &primary_pod,
            &make_set("durable-rejoin", 3, Some(completed.clone())).spec,
        )
        .await
        .unwrap();
    primary_handle
        .execute_durable_action(
            &retire_action_id,
            DurableReplicaAction::RemoveReplica {
                replica_id: target_replica_id,
                instance_id: ReplicaInstanceId::new(old_instance),
            },
        )
        .await
        .unwrap();
    let target_status = live_replica_statuses(&api, "durable-rejoin", 3)
        .await
        .into_iter()
        .find(|status| status.instance_id.as_str() == replacement_instance)
        .unwrap();
    assert_eq!(target_status.role, Role::ActiveSecondary);
}

#[test_log::test(tokio::test)]
#[serial]
async fn test_durable_rejoin_compensation_recreates_and_retries() {
    let api = KvClusterApi::new();
    let state = ReconcilerState::default();
    let previous = create_healthy_set(&api, &state, "rejoin-compensate", 3).await;
    let previous_snapshot = previous.stable_snapshot.clone().unwrap();
    let primary = previous.current_primary.clone().unwrap();
    let target = api
        .pods
        .lock()
        .unwrap()
        .iter()
        .map(|pod| pod.metadata.name.clone().unwrap())
        .find(|name| name != &primary)
        .unwrap();

    api.crash_pod(&target);
    api.restart_pod(&target).await;
    reconcile_set(
        &make_set("rejoin-compensate", 3, Some(previous)),
        &api,
        &state,
    )
    .await
    .unwrap();
    let mut pending = advance_until_pending_action(
        &api,
        "rejoin-compensate",
        3,
        api.last_status().unwrap(),
        DurableActionKind::OpenCandidate,
    )
    .await;
    pending
        .operation
        .as_mut()
        .unwrap()
        .pending_action
        .as_mut()
        .unwrap()
        .deadline_unix_seconds = 0;
    reconcile_set(
        &make_set("rejoin-compensate", 3, Some(pending)),
        &api,
        &ReconcilerState::default(),
    )
    .await
    .unwrap();
    let compensated =
        drive_operation_to_healthy(&api, "rejoin-compensate", 3, api.last_status().unwrap()).await;
    assert_eq!(
        compensated.stable_snapshot.as_ref(),
        Some(&previous_snapshot)
    );
    assert_eq!(api.pods.lock().unwrap().len(), 2);

    reconcile_set(
        &make_set("rejoin-compensate", 3, Some(compensated.clone())),
        &api,
        &ReconcilerState::default(),
    )
    .await
    .unwrap();
    assert_eq!(api.pods.lock().unwrap().len(), 3);
    api.mark_all_pods_ready();
    let completed = drive_add_replica(
        &api,
        &ReconcilerState::default(),
        "rejoin-compensate",
        3,
        compensated,
    )
    .await;
    assert_eq!(
        completed.operation.as_ref().unwrap().phase,
        DurableOperationPhase::Completed
    );
    assert_stable_snapshot(&api, &completed, 3);
}

#[test_log::test(tokio::test)]
#[serial]
async fn test_durable_add_status_conflict_prevents_open() {
    let api = KvClusterApi::new();
    let state = ReconcilerState::default();
    let previous = create_healthy_set(&api, &state, "durable-add-conflict", 1).await;
    reconcile_set(
        &make_set("durable-add-conflict", 2, Some(previous.clone())),
        &api,
        &state,
    )
    .await
    .unwrap();
    api.mark_all_pods_ready();
    reconcile_set(
        &make_set("durable-add-conflict", 2, Some(previous)),
        &api,
        &state,
    )
    .await
    .unwrap();
    let operation_started = api.last_status().unwrap();
    assert_eq!(operation_started.phase, Phase::AddingReplica);
    api.reset_operations();
    api.fail_next_status_conflict();
    let error = match reconcile_set(
        &make_set("durable-add-conflict", 2, Some(operation_started)),
        &api,
        &ReconcilerState::default(),
    )
    .await
    {
        Ok(_) => panic!("conflicting intent status must fail"),
        Err(error) => error,
    };
    assert_eq!(error, "resource version conflict");
    assert!(
        !api.operations().contains(&ControlOperation::Open),
        "candidate must not be opened before durable intent persists"
    );
}

#[test_log::test(tokio::test)]
#[serial]
async fn test_durable_add_compensates_before_and_during_configuration() {
    async fn prepare(api: &KvClusterApi, state: &ReconcilerState, name: &str) -> KubericSetStatus {
        let previous = create_healthy_set(api, state, name, 1).await;
        reconcile_set(&make_set(name, 2, Some(previous.clone())), api, state)
            .await
            .unwrap();
        api.mark_all_pods_ready();
        reconcile_set(&make_set(name, 2, Some(previous)), api, state)
            .await
            .unwrap();
        api.last_status().unwrap()
    }

    let api = KvClusterApi::new();
    let state = ReconcilerState::default();
    let status = prepare(&api, &state, "add-preconfig-compensate").await;
    let mut pending = advance_until_pending_action(
        &api,
        "add-preconfig-compensate",
        2,
        status,
        DurableActionKind::BuildCandidate,
    )
    .await;
    pending
        .operation
        .as_mut()
        .unwrap()
        .pending_action
        .as_mut()
        .unwrap()
        .deadline_unix_seconds = 0;
    reconcile_set(
        &make_set("add-preconfig-compensate", 2, Some(pending)),
        &api,
        &ReconcilerState::default(),
    )
    .await
    .unwrap();
    let expected = [
        DurableActionKind::CompensateRemoveCandidate,
        DurableActionKind::CompensateDemoteCandidate,
        DurableActionKind::CompensateCloseCandidate,
    ];
    let (compensated, injected) = drive_operation_with_lost_replies(
        &api,
        "add-preconfig-compensate",
        2,
        api.last_status().unwrap(),
        &expected,
    )
    .await;
    assert_eq!(injected, expected);
    assert_eq!(
        compensated.stable_snapshot.as_ref().unwrap().members.len(),
        1
    );
    assert_eq!(
        compensated.operation.as_ref().unwrap().phase,
        DurableOperationPhase::Failed
    );

    let api = KvClusterApi::new();
    let state = ReconcilerState::default();
    let status = prepare(&api, &state, "add-config-compensate").await;
    let mut pending = advance_until_pending_action(
        &api,
        "add-config-compensate",
        2,
        status,
        DurableActionKind::AddWaitForCatchUpQuorum,
    )
    .await;
    pending
        .operation
        .as_mut()
        .unwrap()
        .pending_action
        .as_mut()
        .unwrap()
        .deadline_unix_seconds = 0;
    reconcile_set(
        &make_set("add-config-compensate", 2, Some(pending)),
        &api,
        &ReconcilerState::default(),
    )
    .await
    .unwrap();
    let expected = [
        DurableActionKind::CompensateRestoreConfiguration,
        DurableActionKind::CompensateRemoveCandidate,
        DurableActionKind::CompensateDemoteCandidate,
        DurableActionKind::CompensateCloseCandidate,
    ];
    let (compensated, injected) = drive_operation_with_lost_replies(
        &api,
        "add-config-compensate",
        2,
        api.last_status().unwrap(),
        &expected,
    )
    .await;
    assert_eq!(injected, expected);
    assert_eq!(
        compensated.stable_snapshot.as_ref().unwrap().members.len(),
        1
    );
    assert_eq!(
        compensated.operation.as_ref().unwrap().phase,
        DurableOperationPhase::Failed
    );
}

#[test_log::test(tokio::test)]
#[serial]
async fn test_durable_add_rolls_forward_after_current_configuration_commit() {
    let api = KvClusterApi::new();
    let state = ReconcilerState::default();
    let previous = create_healthy_set(&api, &state, "add-roll-forward", 1).await;
    reconcile_set(
        &make_set("add-roll-forward", 2, Some(previous.clone())),
        &api,
        &state,
    )
    .await
    .unwrap();
    api.mark_all_pods_ready();
    reconcile_set(
        &make_set("add-roll-forward", 2, Some(previous)),
        &api,
        &state,
    )
    .await
    .unwrap();
    let pending = advance_until_pending_action(
        &api,
        "add-roll-forward",
        2,
        api.last_status().unwrap(),
        DurableActionKind::AddCurrentConfiguration,
    )
    .await;
    api.fail_after_next_durable_action(ControlOperation::UpdateCurrentConfiguration);
    reconcile_set(
        &make_set("add-roll-forward", 2, Some(pending)),
        &api,
        &ReconcilerState::default(),
    )
    .await
    .unwrap();
    let completed =
        drive_operation_to_healthy(&api, "add-roll-forward", 2, api.last_status().unwrap()).await;
    assert_eq!(
        completed.operation.as_ref().unwrap().phase,
        DurableOperationPhase::Completed
    );
    assert_stable_snapshot(&api, &completed, 2);
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
        Some(KubericSetStatus {
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

    // Subsequent reconciles durably add one replica at a time.
    let status = api.last_status().unwrap();
    let status = drive_add_replica(&api, &state, "myapp", 3, status).await;
    assert_stable_snapshot(&api, &status, 3);

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
        Some(KubericSetStatus {
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
    assert_stable_snapshot(&api, &api.last_status().unwrap(), 1);

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
        Some(KubericSetStatus {
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
    api.crash_pod(&first_primary);

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
    api.crash_pod(&second_primary);

    // Reconcile Healthy: health check detects both dead replicas,
    // removes stale secondaries, then transitions to FailingOver.
    let set = make_set("myapp", 3, Some(status));
    reconcile_set(&set, &api, &state).await.unwrap();
    let status = api.last_status().unwrap();
    assert_eq!(status.phase, Phase::FailingOver);

    let set = make_set("myapp", 3, Some(status));
    reconcile_set(&set, &api, &state).await.unwrap();
    let status = api.last_status().unwrap();
    assert_eq!(status.phase, Phase::Healthy);

    let third_primary = status.current_primary.clone().unwrap();
    assert_ne!(third_primary, first_primary);
    assert_ne!(third_primary, second_primary);

    // Third primary should have data from both epochs.
    // Retry Get — the client gRPC server may not be ready immediately after promotion.
    let addr3 = api.client_address(&third_primary).unwrap();
    let mut kv3 = connect_kv(&addr3).await;

    let resp = retry_get(&mut kv3, "epoch-1").await;
    assert!(resp.get_ref().found, "data from first epoch should survive");

    let resp = retry_get(&mut kv3, "epoch-2").await;
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

/// Reconciler test: secondary crash_pod → detected by epoch-based health
/// check → stale handle removed → restart_pod → re-integrated via scale-up.
#[test_log::test(tokio::test)]
#[serial]
async fn test_reconciler_secondary_crash_and_rejoin() {
    let api = KvClusterApi::new();
    let state = ReconcilerState::default();

    // Create and initialize the partition (Pending → Creating → Healthy)
    let set = make_set("myapp", 3, None);
    reconcile_set(&set, &api, &state).await.unwrap();
    api.mark_all_pods_ready();

    let set = make_set(
        "myapp",
        3,
        Some(KubericSetStatus {
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
        key: "before-crash".into(),
        value: "important".into(),
    })
    .await
    .unwrap();

    // Pick a secondary to crash (any non-primary pod)
    let secondary_name = {
        let pods = api.pods.lock().unwrap();
        pods.iter()
            .map(|p| p.metadata.name.clone().unwrap())
            .find(|n| n != &primary_name)
            .unwrap()
    };

    // Crash the secondary (high fidelity — abort tasks, gRPC breaks)
    api.crash_pod(&secondary_name);

    // Reconcile Healthy: epoch-based health check detects stale secondary
    let status = api.last_status().unwrap();
    let set = make_set("myapp", 3, Some(status));
    reconcile_set(&set, &api, &state).await.unwrap();
    assert_stable_snapshot(&api, &api.last_status().unwrap(), 3);

    // The durable path preserves the previous stable driver/topology until a
    // replacement incarnation is ready.
    {
        let drivers = state.drivers.lock().await;
        let driver = drivers.get("default/myapp").unwrap();
        assert_eq!(
            driver.replica_ids().len(),
            3,
            "stale secondary remains represented until durable replacement starts"
        );
    }

    // Restart the pod (fresh PodRuntime, new ports)
    api.restart_pod(&secondary_name).await;

    // Reconcile Healthy: driver_count=2 < desired=3, re-adds via scale-up
    let status = api.last_status().unwrap();
    let rejoined_status = drive_add_replica(&api, &state, "myapp", 3, status).await;
    assert_stable_snapshot(&api, &rejoined_status, 3);
    let replacement_uid = api
        .pods
        .lock()
        .unwrap()
        .iter()
        .find(|pod| pod.metadata.name.as_deref() == Some(secondary_name.as_str()))
        .unwrap()
        .metadata
        .uid
        .clone()
        .unwrap();
    assert!(
        rejoined_status
            .stable_snapshot
            .as_ref()
            .unwrap()
            .members
            .iter()
            .any(|member| member.instance_id == replacement_uid)
    );

    // Driver should be back to 3 replicas
    {
        let drivers = state.drivers.lock().await;
        let driver = drivers.get("default/myapp").unwrap();
        assert_eq!(
            driver.replica_ids().len(),
            3,
            "restarted secondary should be re-added"
        );
    }

    // Primary still works — write new data
    kv.put(proto::PutRequest {
        key: "after-rejoin".into(),
        value: "recovered".into(),
    })
    .await
    .unwrap();

    // Verify data readable on primary
    let resp = kv
        .get(proto::GetRequest {
            key: "before-crash".into(),
        })
        .await
        .unwrap();
    assert!(resp.get_ref().found);
    assert_eq!(resp.get_ref().value, "important");

    // Promote the replacement. Reading the post-rejoin write from it proves
    // the primary replaced the old same-ID data-plane connection.
    let status = api.last_status().unwrap();
    let set = make_set(
        "myapp",
        3,
        Some(KubericSetStatus {
            phase: Phase::Healthy,
            current_primary: Some(primary_name),
            target_primary: Some(secondary_name.clone()),
            ..status
        }),
    );
    reconcile_set(&set, &api, &state).await.unwrap();
    let status = api.last_status().unwrap();
    assert_eq!(status.phase, Phase::Switchover);
    drive_switchover(&api, &state, "myapp", 3, status).await;

    let replacement_addr = api.client_address(&secondary_name).unwrap();
    let mut replacement = connect_kv(&replacement_addr).await;
    let resp = replacement
        .get(proto::GetRequest {
            key: "after-rejoin".into(),
        })
        .await
        .unwrap();
    assert!(resp.get_ref().found);
    assert_eq!(resp.get_ref().value, "recovered");
}
