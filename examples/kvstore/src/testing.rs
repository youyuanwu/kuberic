//! Test utilities for the kvstore example.
//! Compiled under `#[cfg(test)]` or `feature = "testing"`.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use kubelicate_core::grpc::handle::GrpcReplicaHandle;
use kubelicate_core::pod::PodRuntime;
use tokio::sync::RwLock;

use crate::state::{KvState, SharedState};

/// A running KV pod: PodRuntime + KV service event loop.
pub struct KvPod {
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
        Self::start_with_timeout(id, Duration::from_secs(5)).await
    }

    /// Start with a custom reply timeout (for tests with heavy concurrent load).
    pub async fn start_with_timeout(id: i64, reply_timeout: Duration) -> Self {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let data_dir = std::env::temp_dir().join("kv-test").join(format!(
            "pod-{}-{}-{}",
            id,
            std::process::id(),
            n
        ));
        Self::start_with_dir(id, data_dir, reply_timeout).await
    }

    /// Start with a specific data directory (for restart tests).
    /// Pass a dir from a previous pod to exercise WAL recovery.
    pub async fn start_with_dir(id: i64, data_dir: PathBuf, reply_timeout: Duration) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client_address = listener.local_addr().unwrap().to_string();
        drop(listener);

        // Pre-bind the data plane port so we know the address before Open.
        // The address is passed via OpenContext.data_bind, and WalReplicator::create()
        // binds to it inside the user's Open handler.
        let data_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let data_port = data_listener.local_addr().unwrap().port();
        let data_bind = format!("127.0.0.1:{}", data_port);
        let data_address = format!("http://{}", data_bind);
        drop(data_listener); // release — WalReplicator::create() will rebind

        let bundle = PodRuntime::builder(id)
            .reply_timeout(reply_timeout)
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
        let service_handle =
            tokio::spawn(crate::service::run_service(bundle.lifecycle_rx, st, bind));

        tokio::time::sleep(Duration::from_millis(50)).await;

        Self {
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
        GrpcReplicaHandle::connect(id, self.control_address.clone(), self.data_address.clone())
            .await
            .unwrap()
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
