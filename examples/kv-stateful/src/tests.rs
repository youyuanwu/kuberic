use std::sync::Arc;
use std::time::Duration;

use kubelicate_core::driver::{PartitionDriver, ReplicaHandle};
use kubelicate_core::grpc::handle::GrpcReplicaHandle;
use kubelicate_core::pod::PodRuntime;
use kubelicate_core::types::AccessStatus;
use tokio::sync::RwLock;

use crate::proto;
use crate::service;
use crate::state::{KvState, SharedState};

/// A running KV pod: PodRuntime + KV service event loop.
struct KvPod {
    control_address: String,
    data_address: String,
    client_address: String,
    state: SharedState,
    _runtime_handle: tokio::task::JoinHandle<()>,
    _service_handle: tokio::task::JoinHandle<()>,
}

impl KvPod {
    /// Start a KV pod with a PodRuntime and the KV service event loop.
    async fn start(id: i64) -> Self {
        // Pre-allocate a port for the client KV gRPC server
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client_address = listener.local_addr().unwrap().to_string();
        drop(listener);

        let bundle = PodRuntime::builder(id)
            .reply_timeout(Duration::from_secs(5))
            .build()
            .await
            .unwrap();

        let control_address = bundle.control_address.clone();
        let data_address = bundle.data_address.clone();
        let state: SharedState = Arc::new(RwLock::new(KvState::new()));

        let runtime_handle = tokio::spawn(bundle.runtime.serve());
        let st = state.clone();
        let bind = client_address.clone();
        let service_handle = tokio::spawn(service::run_service(
            bundle.lifecycle_rx,
            bundle.state_provider_rx,
            st,
            bind,
        ));

        // Allow spawned tasks to start polling
        tokio::time::sleep(Duration::from_millis(50)).await;

        Self {
            control_address,
            data_address,
            client_address,
            state,
            _runtime_handle: runtime_handle,
            _service_handle: service_handle,
        }
    }

    /// Create a GrpcReplicaHandle for this pod (what the operator uses).
    async fn replica_handle(&self, id: i64) -> GrpcReplicaHandle {
        GrpcReplicaHandle::connect(id, self.control_address.clone(), self.data_address.clone())
            .await
            .unwrap()
    }
}

/// Helper: connect a KV gRPC client with retries.
async fn connect_kv_client(
    addr: &str,
) -> proto::kv_store_client::KvStoreClient<tonic::transport::Channel> {
    for attempt in 0..30 {
        match proto::kv_store_client::KvStoreClient::connect(format!("http://{}", addr)).await {
            Ok(c) => return c,
            Err(_) if attempt < 29 => {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            Err(e) => panic!("failed to connect to KV server: {}", e),
        }
    }
    unreachable!()
}

/// Operator-driven test: PartitionDriver creates a single-replica partition,
/// KV client writes data, verifies reads.
#[test_log::test(tokio::test)]
async fn test_operator_single_replica_kv() {
    let pod = KvPod::start(1).await;

    let h1 = pod.replica_handle(1).await;
    let handles: Vec<Box<dyn ReplicaHandle>> = vec![Box::new(h1)];

    // Operator creates partition (Open → Primary → configure)
    let mut driver = PartitionDriver::new();
    driver.create_partition(handles).await.unwrap();

    assert_eq!(driver.primary_id(), Some(1));

    // Wait for the KV client server to start (triggered by ChangeRole(Primary))
    let mut kv = connect_kv_client(&pod.client_address).await;

    // Write via KV API
    let resp = kv
        .put(proto::PutRequest {
            key: "hello".to_string(),
            value: "world".to_string(),
        })
        .await
        .unwrap();
    assert_eq!(resp.get_ref().lsn, 1);

    // Read via KV API
    let resp = kv
        .get(proto::GetRequest {
            key: "hello".to_string(),
        })
        .await
        .unwrap();
    assert!(resp.get_ref().found);
    assert_eq!(resp.get_ref().value, "world");

    // Verify in-memory state
    {
        let st = pod.state.read().await;
        assert_eq!(st.data.len(), 1);
        assert_eq!(st.last_applied_lsn, 1);
    }

    // Operator tears down partition
    driver.delete_partition().await.unwrap();
}

/// Operator-driven test: 3-replica partition, write on primary, verify via
/// KV client, then failover and verify new primary works.
#[tokio::test]
async fn test_operator_three_replica_failover() {
    let pod1 = KvPod::start(1).await;
    let pod2 = KvPod::start(2).await;
    let pod3 = KvPod::start(3).await;

    let h1 = pod1.replica_handle(1).await;
    let h2 = pod2.replica_handle(2).await;
    let h3 = pod3.replica_handle(3).await;
    let handles: Vec<Box<dyn ReplicaHandle>> = vec![Box::new(h1), Box::new(h2), Box::new(h3)];

    // Operator creates 3-replica partition (pod1 = primary)
    let mut driver = PartitionDriver::new();
    driver.create_partition(handles).await.unwrap();

    let primary_id = driver.primary_id().unwrap();
    assert_eq!(primary_id, 1);

    // Write 3 KV pairs on primary
    let mut kv = connect_kv_client(&pod1.client_address).await;
    for i in 1..=3 {
        let resp = kv
            .put(proto::PutRequest {
                key: format!("key-{}", i),
                value: format!("val-{}", i),
            })
            .await
            .unwrap();
        assert_eq!(resp.get_ref().lsn, i as i64);
    }

    // Verify reads on primary
    let resp = kv
        .get(proto::GetRequest {
            key: "key-2".to_string(),
        })
        .await
        .unwrap();
    assert_eq!(resp.get_ref().value, "val-2");

    // Verify primary in-memory state
    {
        let st = pod1.state.read().await;
        assert_eq!(st.data.len(), 3);
        assert_eq!(st.last_applied_lsn, 3);
    }

    // Operator triggers failover (simulate pod1 failure)
    driver.failover(1).await.unwrap();

    let new_primary = driver.primary_id().unwrap();
    assert_ne!(new_primary, 1);
    assert_eq!(
        driver.handle(new_primary).unwrap().write_status(),
        AccessStatus::Granted
    );

    // Determine which pod is the new primary
    let new_primary_pod = if new_primary == 2 { &pod2 } else { &pod3 };

    // Write on new primary via KV API
    let mut kv2 = connect_kv_client(&new_primary_pod.client_address).await;
    let resp = kv2
        .put(proto::PutRequest {
            key: "post-failover".to_string(),
            value: "survived".to_string(),
        })
        .await
        .unwrap();
    assert!(resp.get_ref().lsn > 0);

    // Read the new key
    let resp = kv2
        .get(proto::GetRequest {
            key: "post-failover".to_string(),
        })
        .await
        .unwrap();
    assert!(resp.get_ref().found);
    assert_eq!(resp.get_ref().value, "survived");

    driver.delete_partition().await.unwrap();
}

/// Operator-driven test: write, delete, overwrite, verify final state.
#[tokio::test]
async fn test_operator_kv_crud_operations() {
    let pod = KvPod::start(10).await;

    let h = pod.replica_handle(10).await;
    let mut driver = PartitionDriver::new();
    driver.create_partition(vec![Box::new(h)]).await.unwrap();

    let mut kv = connect_kv_client(&pod.client_address).await;

    // Put
    kv.put(proto::PutRequest {
        key: "a".to_string(),
        value: "1".to_string(),
    })
    .await
    .unwrap();

    kv.put(proto::PutRequest {
        key: "b".to_string(),
        value: "2".to_string(),
    })
    .await
    .unwrap();

    // Overwrite
    kv.put(proto::PutRequest {
        key: "a".to_string(),
        value: "10".to_string(),
    })
    .await
    .unwrap();

    // Delete
    let resp = kv
        .delete(proto::DeleteRequest {
            key: "b".to_string(),
        })
        .await
        .unwrap();
    assert!(resp.get_ref().existed);

    // Delete non-existent
    let resp = kv
        .delete(proto::DeleteRequest {
            key: "zzz".to_string(),
        })
        .await
        .unwrap();
    assert!(!resp.get_ref().existed);

    // Get missing
    let resp = kv
        .get(proto::GetRequest {
            key: "b".to_string(),
        })
        .await
        .unwrap();
    assert!(!resp.get_ref().found);

    // Get overwritten
    let resp = kv
        .get(proto::GetRequest {
            key: "a".to_string(),
        })
        .await
        .unwrap();
    assert_eq!(resp.get_ref().value, "10");

    // Verify final state
    {
        let st = pod.state.read().await;
        assert_eq!(st.data.len(), 1);
        assert_eq!(st.data.get("a").unwrap(), "10");
        assert_eq!(st.last_applied_lsn, 5); // 2 puts + 1 overwrite + 2 deletes
    }

    driver.delete_partition().await.unwrap();
}
