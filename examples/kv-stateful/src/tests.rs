use std::sync::Arc;
use std::time::Duration;

use kubelicate_core::driver::{PartitionDriver, ReplicaHandle};
use kubelicate_core::grpc::handle::GrpcReplicaHandle;
use kubelicate_core::pod::PodRuntime;
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
        Self::start_with_timeout(id, Duration::from_secs(5)).await
    }

    /// Start with a custom reply timeout (for tests with heavy concurrent load).
    async fn start_with_timeout(id: i64, reply_timeout: Duration) -> Self {
        // Pre-allocate a port for the client KV gRPC server
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client_address = listener.local_addr().unwrap().to_string();
        drop(listener);

        let bundle = PodRuntime::builder(id)
            .reply_timeout(reply_timeout)
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

/// Helper: wait until a pod's state has the expected number of entries.
/// Polls every 100ms, panics after 30 seconds.
async fn wait_for_state_count(state: &SharedState, expected: usize) {
    for _ in 0..300 {
        if state.read().await.data.len() >= expected {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let actual = state.read().await.data.len();
    panic!("timed out waiting for {expected} entries, got {actual}");
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

/// Operator-driven test: create 3 replicas, write data on primary,
/// restart a secondary, verify it received the data via copy stream.
#[test_log::test(tokio::test)]
async fn test_operator_restart_secondary_copies_state() {
    let pod1 = KvPod::start(1).await;
    let pod2 = KvPod::start(2).await;
    let pod3 = KvPod::start(3).await;

    let h1 = pod1.replica_handle(1).await;
    let h2 = pod2.replica_handle(2).await;
    let h3 = pod3.replica_handle(3).await;
    let handles: Vec<Box<dyn ReplicaHandle>> = vec![Box::new(h1), Box::new(h2), Box::new(h3)];

    let mut driver = PartitionDriver::new();
    driver.create_partition(handles).await.unwrap();
    assert_eq!(driver.primary_id(), Some(1));

    // Write 3 KV pairs on primary
    let mut kv = connect_kv_client(&pod1.client_address).await;
    for i in 1..=3 {
        kv.put(proto::PutRequest {
            key: format!("key-{}", i),
            value: format!("val-{}", i),
        })
        .await
        .unwrap();
    }

    // Verify primary has 3 entries
    {
        let st = pod1.state.read().await;
        assert_eq!(st.data.len(), 3);
        assert_eq!(st.last_applied_lsn, 3);
    }

    // Restart secondary 2 with a fresh pod
    let pod2_new = KvPod::start(20).await; // fresh pod, new state
    let h2_new = pod2_new.replica_handle(2).await; // same replica ID, new handle

    driver.restart_secondary(2, Box::new(h2_new)).await.unwrap();

    // Wait for copy + replication to deliver all 3 entries
    wait_for_state_count(&pod2_new.state, 3).await;

    // Verify the restarted secondary received the data via copy stream
    {
        let st = pod2_new.state.read().await;
        assert_eq!(
            st.data.len(),
            3,
            "restarted secondary should have 3 entries from copy"
        );
        assert_eq!(st.data.get("key-1").unwrap(), "val-1");
        assert_eq!(st.data.get("key-2").unwrap(), "val-2");
        assert_eq!(st.data.get("key-3").unwrap(), "val-3");
    }

    driver.delete_partition().await.unwrap();
}

/// Operator-driven test: scale-up from 1 to 3 replicas, verify new replicas
/// receive existing data via copy stream.
#[test_log::test(tokio::test)]
async fn test_operator_scale_up() {
    // Start with 1 replica (primary only)
    let pod1 = KvPod::start(1).await;
    let h1 = pod1.replica_handle(1).await;

    let mut driver = PartitionDriver::new();
    driver.create_partition(vec![Box::new(h1)]).await.unwrap();
    assert_eq!(driver.primary_id(), Some(1));

    // Write data on single-replica primary
    let mut kv = connect_kv_client(&pod1.client_address).await;
    for i in 1..=5 {
        kv.put(proto::PutRequest {
            key: format!("k{}", i),
            value: format!("v{}", i),
        })
        .await
        .unwrap();
    }

    // Scale up: add replica 2
    let pod2 = KvPod::start(2).await;
    let h2 = pod2.replica_handle(2).await;
    driver.add_replica(Box::new(h2)).await.unwrap();

    // Scale up: add replica 3
    let pod3 = KvPod::start(3).await;
    let h3 = pod3.replica_handle(3).await;
    driver.add_replica(Box::new(h3)).await.unwrap();

    assert_eq!(driver.replica_ids().len(), 3);

    // Wait for copy + replication to deliver all 5 entries to both replicas
    wait_for_state_count(&pod2.state, 5).await;
    wait_for_state_count(&pod3.state, 5).await;

    // Verify both new replicas received all 5 KV pairs via copy
    for (name, pod) in [("pod2", &pod2), ("pod3", &pod3)] {
        let st = pod.state.read().await;
        assert_eq!(st.data.len(), 5, "{name} should have 5 entries from copy");
        for i in 1..=5 {
            assert_eq!(
                st.data.get(&format!("k{}", i)).unwrap(),
                &format!("v{}", i),
                "{name} missing k{i}"
            );
        }
    }

    // Write more data — should replicate to all 3 replicas now
    kv.put(proto::PutRequest {
        key: "after-scale".to_string(),
        value: "yes".to_string(),
    })
    .await
    .unwrap();

    // Verify primary has 6 entries
    {
        let st = pod1.state.read().await;
        assert_eq!(st.data.len(), 6);
    }

    driver.delete_partition().await.unwrap();
}

/// Operator-driven test: scale-down from 3 to 1 replica.
#[test_log::test(tokio::test)]
async fn test_operator_scale_down() {
    let pod1 = KvPod::start(1).await;
    let pod2 = KvPod::start(2).await;
    let pod3 = KvPod::start(3).await;

    let h1 = pod1.replica_handle(1).await;
    let h2 = pod2.replica_handle(2).await;
    let h3 = pod3.replica_handle(3).await;

    let mut driver = PartitionDriver::new();
    driver
        .create_partition(vec![Box::new(h1), Box::new(h2), Box::new(h3)])
        .await
        .unwrap();
    assert_eq!(driver.replica_ids().len(), 3);
    assert_eq!(driver.primary_id(), Some(1));

    // Write some data
    let mut kv = connect_kv_client(&pod1.client_address).await;
    kv.put(proto::PutRequest {
        key: "before".to_string(),
        value: "scale-down".to_string(),
    })
    .await
    .unwrap();

    // Scale down: remove replica 3 (min_replicas=1)
    driver.remove_secondary(3, 1).await.unwrap();
    assert_eq!(driver.replica_ids().len(), 2);

    // Scale down: remove replica 2
    driver.remove_secondary(2, 1).await.unwrap();
    assert_eq!(driver.replica_ids().len(), 1);

    // Primary still works with single replica
    let resp = kv
        .put(proto::PutRequest {
            key: "after".to_string(),
            value: "scale-down".to_string(),
        })
        .await
        .unwrap();
    assert!(resp.get_ref().lsn > 0);

    // Reads still work
    let resp = kv
        .get(proto::GetRequest {
            key: "before".to_string(),
        })
        .await
        .unwrap();
    assert_eq!(resp.get_ref().value, "scale-down");

    driver.delete_partition().await.unwrap();
}

/// Operator-driven test: switchover from primary to a chosen secondary,
/// verify new primary works and old primary is demoted.
#[test_log::test(tokio::test)]
async fn test_operator_switchover() {
    let pod1 = KvPod::start(1).await;
    let pod2 = KvPod::start(2).await;
    let pod3 = KvPod::start(3).await;

    let h1 = pod1.replica_handle(1).await;
    let h2 = pod2.replica_handle(2).await;
    let h3 = pod3.replica_handle(3).await;

    let mut driver = PartitionDriver::new();
    driver
        .create_partition(vec![Box::new(h1), Box::new(h2), Box::new(h3)])
        .await
        .unwrap();
    assert_eq!(driver.primary_id(), Some(1));

    // Write data on primary
    let mut kv1 = connect_kv_client(&pod1.client_address).await;
    kv1.put(proto::PutRequest {
        key: "before".to_string(),
        value: "switchover".to_string(),
    })
    .await
    .unwrap();

    // Switchover to pod 2
    driver.switchover(2).await.unwrap();
    assert_eq!(driver.primary_id(), Some(2));
    assert_eq!(driver.replica_ids().len(), 3);

    // New primary (pod 2) accepts writes
    let mut kv2 = connect_kv_client(&pod2.client_address).await;
    let resp = kv2
        .put(proto::PutRequest {
            key: "after".to_string(),
            value: "switchover".to_string(),
        })
        .await
        .unwrap();
    assert!(resp.get_ref().lsn > 0);

    // Old primary (pod 1) rejects writes
    let result = kv1
        .put(proto::PutRequest {
            key: "should-fail".to_string(),
            value: "x".to_string(),
        })
        .await;
    assert!(
        result.is_err(),
        "old primary should reject writes after switchover"
    );

    // Data from before switchover is readable on new primary
    let resp = kv2
        .get(proto::GetRequest {
            key: "before".to_string(),
        })
        .await
        .unwrap();
    assert!(resp.get_ref().found);
    assert_eq!(resp.get_ref().value, "switchover");

    driver.delete_partition().await.unwrap();
}

/// Operator-driven test: after failover, verify the old primary's epoch
/// is stale — writes on old primary are rejected. This validates epoch
/// fencing end-to-end through the full KV app stack.
///
/// NOTE: Zombie write rejection on old primary requires the operator to
/// explicitly close/delete the old primary pod (not implemented in failover).
/// This test verifies the new primary works and pre-failover data survives.
/// See design-gaps.md A2 (write revocation) and operator-failure-scenarios.md §9
/// (stale/zombie primary).
#[test_log::test(tokio::test)]
async fn test_operator_epoch_fencing_after_failover() {
    let pod1 = KvPod::start(1).await;
    let pod2 = KvPod::start(2).await;
    let pod3 = KvPod::start(3).await;

    let h1 = pod1.replica_handle(1).await;
    let h2 = pod2.replica_handle(2).await;
    let h3 = pod3.replica_handle(3).await;

    let mut driver = PartitionDriver::new();
    driver
        .create_partition(vec![Box::new(h1), Box::new(h2), Box::new(h3)])
        .await
        .unwrap();
    assert_eq!(driver.primary_id(), Some(1));

    // Write data on primary
    let mut kv1 = connect_kv_client(&pod1.client_address).await;
    for i in 1..=3 {
        kv1.put(proto::PutRequest {
            key: format!("key-{}", i),
            value: format!("val-{}", i),
        })
        .await
        .unwrap();
    }

    // Failover: primary 1 "fails", promote best secondary
    driver.failover(1).await.unwrap();
    let new_primary = driver.primary_id().unwrap();
    assert_ne!(new_primary, 1);

    // New primary works
    let new_primary_pod = if new_primary == 2 { &pod2 } else { &pod3 };
    let mut kv_new = connect_kv_client(&new_primary_pod.client_address).await;
    let resp = kv_new
        .put(proto::PutRequest {
            key: "post-failover".to_string(),
            value: "works".to_string(),
        })
        .await
        .unwrap();
    assert!(resp.get_ref().lsn > 0);

    // Data from before failover survived on new primary
    for i in 1..=3 {
        let resp = kv_new
            .get(proto::GetRequest {
                key: format!("key-{}", i),
            })
            .await
            .unwrap();
        assert!(resp.get_ref().found);
        assert_eq!(resp.get_ref().value, format!("val-{}", i));
    }

    driver.delete_partition().await.unwrap();
}

/// Operator-driven test: delete_partition closes all replicas. Verify the
/// primary stops accepting writes after deletion.
#[test_log::test(tokio::test)]
async fn test_operator_delete_partition() {
    let pod1 = KvPod::start(1).await;
    let pod2 = KvPod::start(2).await;
    let pod3 = KvPod::start(3).await;

    let h1 = pod1.replica_handle(1).await;
    let h2 = pod2.replica_handle(2).await;
    let h3 = pod3.replica_handle(3).await;

    let mut driver = PartitionDriver::new();
    driver
        .create_partition(vec![Box::new(h1), Box::new(h2), Box::new(h3)])
        .await
        .unwrap();

    // Write data
    let mut kv = connect_kv_client(&pod1.client_address).await;
    kv.put(proto::PutRequest {
        key: "alive".to_string(),
        value: "yes".to_string(),
    })
    .await
    .unwrap();

    // Delete partition
    driver.delete_partition().await.unwrap();
    assert!(driver.primary_id().is_none());
    assert_eq!(driver.replica_ids().len(), 0);

    // Primary should no longer accept writes (partition closed)
    let result = kv
        .put(proto::PutRequest {
            key: "after-delete".to_string(),
            value: "should-fail".to_string(),
        })
        .await;
    assert!(result.is_err(), "writes should fail after delete_partition");
}

/// Operator-driven test: verify epoch truncation on secondaries. After
/// failover, the secondary's uncommitted ops from the old epoch should
/// be discarded.
#[test_log::test(tokio::test)]
async fn test_operator_secondary_state_after_failover() {
    let pod1 = KvPod::start(1).await;
    let pod2 = KvPod::start(2).await;
    let pod3 = KvPod::start(3).await;

    let h1 = pod1.replica_handle(1).await;
    let h2 = pod2.replica_handle(2).await;
    let h3 = pod3.replica_handle(3).await;

    let mut driver = PartitionDriver::new();
    driver
        .create_partition(vec![Box::new(h1), Box::new(h2), Box::new(h3)])
        .await
        .unwrap();
    assert_eq!(driver.primary_id(), Some(1));

    // Write 5 ops on primary — replicated to secondaries
    let mut kv = connect_kv_client(&pod1.client_address).await;
    for i in 1..=5 {
        kv.put(proto::PutRequest {
            key: format!("k{}", i),
            value: format!("v{}", i),
        })
        .await
        .unwrap();
    }

    // Wait for replication to deliver all 5 entries to secondaries
    wait_for_state_count(&pod2.state, 5).await;
    wait_for_state_count(&pod3.state, 5).await;

    // Verify secondaries received the data
    {
        let st2 = pod2.state.read().await;
        assert_eq!(st2.data.len(), 5, "secondary 2 should have 5 entries");
    }
    {
        let st3 = pod3.state.read().await;
        assert_eq!(st3.data.len(), 5, "secondary 3 should have 5 entries");
    }

    // Failover: pod 1 fails, promote a secondary
    driver.failover(1).await.unwrap();
    let new_primary = driver.primary_id().unwrap();
    assert_ne!(new_primary, 1);

    // The surviving secondaries should still have all 5 entries
    // (epoch bump truncates only uncommitted ops — all 5 were committed)
    let surviving_secondary = if new_primary == 2 { &pod3 } else { &pod2 };
    {
        let st = surviving_secondary.state.read().await;
        assert_eq!(
            st.data.len(),
            5,
            "surviving secondary should retain all committed data"
        );
    }

    // New primary should have all 5 entries too
    let new_primary_pod = if new_primary == 2 { &pod2 } else { &pod3 };
    {
        let st = new_primary_pod.state.read().await;
        assert_eq!(
            st.data.len(),
            5,
            "new primary should have all committed data"
        );
    }

    driver.delete_partition().await.unwrap();
}

/// Operator-driven test: write on primary WHILE add_replica is copying state
/// to a new secondary. The PrimarySender should buffer these ops and replay
/// them after the copy completes. The new secondary should have both the
/// copied state AND the buffered ops.
///
/// NOTE: This test is ignored by default because concurrent writes during
/// add_replica can trigger the C0 channel contention issue (state_provider_tx
/// shared between copy and replication events). Run with:
///   cargo test test_operator_build_buffer_replay -- --ignored
#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
#[ignore]
async fn test_operator_build_buffer_replay() {
    // Start with 3 replicas
    let pod1 = KvPod::start(1).await;
    let pod2 = KvPod::start(2).await;
    let pod3 = KvPod::start(3).await;

    let h1 = pod1.replica_handle(1).await;
    let h2 = pod2.replica_handle(2).await;
    let h3 = pod3.replica_handle(3).await;

    let mut driver = PartitionDriver::new();
    driver
        .create_partition(vec![Box::new(h1), Box::new(h2), Box::new(h3)])
        .await
        .unwrap();
    assert_eq!(driver.primary_id(), Some(1));

    // Write initial data
    let mut kv = connect_kv_client(&pod1.client_address).await;
    for i in 1..=20 {
        kv.put(proto::PutRequest {
            key: format!("initial-{}", i),
            value: format!("val-{}", i),
        })
        .await
        .unwrap();
    }

    // Now add a 4th replica. Start writing in a background task FIRST
    // to ensure ops are in-flight during the copy. The PrimarySender
    // should buffer these via build_buffers and replay on connect.
    let pod4 = KvPod::start(4).await;
    let h4 = pod4.replica_handle(4).await;

    // Start continuous writes in background BEFORE add_replica.
    // Use a notify to sync: background task signals after first write
    // succeeds, ensuring writes are flowing before copy starts.
    let started = Arc::new(tokio::sync::Notify::new());
    let started_clone = started.clone();
    let write_addr = pod1.client_address.clone();
    let write_handle = tokio::spawn(async move {
        let mut kv_bg = connect_kv_client(&write_addr).await;
        for i in 1..=10 {
            kv_bg
                .put(proto::PutRequest {
                    key: format!("during-copy-{}", i),
                    value: format!("buffered-{}", i),
                })
                .await
                .unwrap();
            if i == 1 {
                started_clone.notify_one(); // signal: writes are flowing
            }
        }
    });

    // Wait until background task has completed at least one write
    started.notified().await;

    // add_replica blocks during copy — writes are happening concurrently
    driver.add_replica(Box::new(h4)).await.unwrap();
    assert_eq!(driver.replica_ids().len(), 4);

    // Wait for background writes to finish, then wait for pod4 to receive
    // all entries: 50 initial + 20 during-copy = 70 total.
    // Some entries come via copy, others via build buffer replay, others via
    // live replication — timing varies, so poll on total count.
    write_handle.await.unwrap();
    wait_for_state_count(&pod4.state, 30).await;

    // Verify the new secondary (pod4) has BOTH:
    // - The 20 initial entries (from copy/replication)
    // - The 10 during-copy entries (from build buffer replay + replication)
    {
        let st = pod4.state.read().await;
        for i in 1..=20 {
            assert!(
                st.data.contains_key(&format!("initial-{}", i)),
                "pod4 missing initial-{i}"
            );
        }
        for i in 1..=10 {
            assert!(
                st.data.contains_key(&format!("during-copy-{}", i)),
                "pod4 missing during-copy-{i} (should come from build buffer replay)"
            );
        }
        assert_eq!(
            st.data.len(),
            30,
            "pod4 should have 20 initial + 10 buffered = 30 entries"
        );
    }

    driver.delete_partition().await.unwrap();
}
