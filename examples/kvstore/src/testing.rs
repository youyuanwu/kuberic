//! Test utilities for the kvstore example.
//! Compiled under `#[cfg(test)]` or `feature = "testing"`.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex as StdMutex, OnceLock};
use std::time::Duration;

use kuberic_core::grpc::handle::GrpcReplicaHandle;
use kuberic_core::pod::PodRuntime;
use kuberic_core::replicator::WalReplicatorOptions;
use kuberic_core::types::ReplicaInstanceId;
use tokio::sync::RwLock;

use crate::state::{KvState, SharedState};

async fn allocate_unique_address() -> String {
    static ALLOCATED_PORTS: OnceLock<StdMutex<HashSet<u16>>> = OnceLock::new();

    loop {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let is_new = ALLOCATED_PORTS
            .get_or_init(|| StdMutex::new(HashSet::new()))
            .lock()
            .unwrap()
            .insert(port);
        drop(listener);

        if is_new {
            return format!("127.0.0.1:{port}");
        }
    }
}

/// A running KV pod: PodRuntime + KV service event loop.
pub struct KvPod {
    pub instance_id: ReplicaInstanceId,
    pub control_address: String,
    pub data_address: String,
    pub client_address: String,
    pub state: SharedState,
    pub data_dir: PathBuf,
    _runtime_handle: tokio::task::JoinHandle<()>,
    _service_handle: tokio::task::JoinHandle<()>,
}

impl KvPod {
    /// Start a KV pod with a PodRuntime and the KV service event loop.
    pub async fn start(id: i64) -> Self {
        Self::start_with_timeout(id, Duration::from_secs(10)).await
    }

    /// Start with a custom reply timeout (for tests with heavy concurrent load).
    pub async fn start_with_timeout(id: i64, reply_timeout: Duration) -> Self {
        Self::start_with_options(id, reply_timeout, WalReplicatorOptions::default()).await
    }

    /// Start with independent runtime reply and quorum timeouts.
    pub async fn start_with_timeouts(
        id: i64,
        reply_timeout: Duration,
        quorum_timeout: Duration,
    ) -> Self {
        Self::start_with_options(
            id,
            reply_timeout,
            WalReplicatorOptions::new().quorum_timeout(quorum_timeout),
        )
        .await
    }

    async fn start_with_options(
        id: i64,
        reply_timeout: Duration,
        replicator_options: WalReplicatorOptions,
    ) -> Self {
        Self::start_with_options_and_data_loss(
            id,
            reply_timeout,
            replicator_options,
            crate::service::DataLossBehavior::default(),
        )
        .await
    }

    pub async fn start_with_data_loss_behavior(
        id: i64,
        behavior: crate::service::DataLossBehavior,
    ) -> Self {
        Self::start_with_options_and_data_loss(
            id,
            Duration::from_secs(10),
            WalReplicatorOptions::default(),
            behavior,
        )
        .await
    }

    async fn start_with_options_and_data_loss(
        id: i64,
        reply_timeout: Duration,
        replicator_options: WalReplicatorOptions,
        data_loss_behavior: crate::service::DataLossBehavior,
    ) -> Self {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let data_dir = std::env::temp_dir().join("kv-test").join(format!(
            "pod-{}-{}-{}",
            id,
            std::process::id(),
            n
        ));
        Self::start_with_dir_and_options(
            id,
            data_dir,
            reply_timeout,
            replicator_options,
            data_loss_behavior,
        )
        .await
    }

    /// Start with a specific data directory (for restart tests).
    /// Pass a dir from a previous pod to exercise WAL recovery.
    pub async fn start_with_dir(id: i64, data_dir: PathBuf, reply_timeout: Duration) -> Self {
        Self::start_with_dir_and_options(
            id,
            data_dir,
            reply_timeout,
            WalReplicatorOptions::default(),
            crate::service::DataLossBehavior::default(),
        )
        .await
    }

    async fn start_with_dir_and_options(
        id: i64,
        data_dir: PathBuf,
        reply_timeout: Duration,
        replicator_options: WalReplicatorOptions,
        data_loss_behavior: crate::service::DataLossBehavior,
    ) -> Self {
        static INSTANCE_COUNTER: std::sync::atomic::AtomicU64 =
            std::sync::atomic::AtomicU64::new(1);
        let generation = INSTANCE_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let instance_id = ReplicaInstanceId::new(format!("kv-pod-{id}-{generation}"));
        let client_address = allocate_unique_address().await;

        // Pre-bind the data plane port so we know the address before Open.
        // The address is passed via OpenContext.data_bind, and WalReplicator::create()
        // binds to it inside the user's Open handler.
        let data_bind = allocate_unique_address().await;
        let data_address = format!("http://{}", data_bind);
        let control_bind = allocate_unique_address().await;

        let bundle = PodRuntime::builder(id)
            .instance_id(instance_id.clone())
            .reply_timeout(reply_timeout)
            .control_bind(control_bind)
            .data_bind(data_bind)
            .build()
            .await
            .unwrap();

        let control_address = bundle.control_address.clone();
        let state: SharedState =
            Arc::new(RwLock::new(KvState::open(data_dir.clone()).await.unwrap()));

        let runtime_handle = tokio::spawn(bundle.runtime.serve());
        let st = state.clone();
        let bind = client_address.clone();
        let service_handle = tokio::spawn(crate::service::run_service_with_options_and_data_loss(
            bundle.lifecycle_rx,
            st,
            bind,
            replicator_options,
            data_loss_behavior,
        ));

        tokio::time::sleep(Duration::from_millis(50)).await;

        Self {
            instance_id,
            control_address,
            data_address,
            client_address,
            state,
            data_dir,
            _runtime_handle: runtime_handle,
            _service_handle: service_handle,
        }
    }

    /// Create a GrpcReplicaHandle for this pod (what the operator uses).
    pub async fn replica_handle(&self, id: i64) -> GrpcReplicaHandle {
        GrpcReplicaHandle::connect(
            id,
            self.instance_id.clone(),
            self.control_address.clone(),
            self.data_address.clone(),
        )
        .await
        .unwrap()
    }

    /// Simulate a pod crash. Aborts the PodRuntime and service without
    /// graceful shutdown. The KvPod instance becomes unusable, but tasks
    /// independently spawned by the aborted owners can survive; use
    /// a correlated `Close` action when a test needs graceful shutdown.
    pub async fn crash(self) {
        self._runtime_handle.abort();
        self._service_handle.abort();
        // Drop the owned fields after aborting the two owner tasks.
    }

    /// Crash this pod and start a fresh one with the same replica ID
    /// and the same data directory. The new pod recovers from checkpoint.
    /// Returns a new KvPod with fresh gRPC addresses.
    pub async fn restart(self, id: i64) -> KvPod {
        let dir = self.data_dir.clone();
        self.crash().await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        KvPod::start_with_dir(id, dir, Duration::from_secs(5)).await
    }
}

/// Helper: connect a KV gRPC client with retries.
pub async fn connect_kv_client(
    addr: &str,
) -> crate::proto::kv_store_client::KvStoreClient<tonic::transport::Channel> {
    for attempt in 0..30 {
        match crate::proto::kv_store_client::KvStoreClient::connect(format!("http://{}", addr))
            .await
        {
            Ok(c) => return c,
            Err(_) if attempt < 29 => {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            Err(e) => panic!("failed to connect to KV server: {}", e),
        }
    }
    unreachable!()
}

/// Helper: wait until a pod's state has the expected number of entries.
pub async fn wait_for_state_count(state: &SharedState, expected: usize) {
    let mut last_count = 0;
    for _tick in 0..300 {
        let count = state.read().await.data.len();
        if count >= expected {
            return;
        }
        if count != last_count {
            eprintln!("[wait] count={count} expected={expected}");
            last_count = count;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let actual = state.read().await.data.len();
    panic!("timed out waiting for {expected} entries, got {actual}");
}
