use kubelicate_core::driver::{PartitionDriver, ReplicaHandle};
use serial_test::serial;

use kvstore::proto;
use kvstore::testing::{KvPod, connect_kv_client, wait_for_state_count};

/// Operator-driven test: PartitionDriver creates a single-replica partition,
/// KV client writes data, verifies reads.
#[test_log::test(tokio::test)]
#[serial]
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

/// Operator-driven test: write, delete, overwrite, verify final state.
#[test_log::test(tokio::test)]
#[serial]
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
#[serial]
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
#[serial]
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
#[serial]
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

/// Operator-driven test: delete_partition closes all replicas. Verify the
/// primary stops accepting writes after deletion.
#[test_log::test(tokio::test)]
#[serial]
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
#[serial]
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

    // After failover at steady state: all 5 ops returned success to the
    // client, so all 5 are quorum-committed. However, committed_lsn
    // propagation to secondaries has a one-item lag (piggybacked on the
    // NEXT item). The last op's committed_lsn=5 was never sent because
    // there was no subsequent item. So the secondary sees committed=4
    // and may roll back op 5.
    //
    // This is correct and matches SF's behavior: the rolled-back op
    // will be re-replicated during catch-up from the new primary.
    let surviving_secondary = if new_primary == 2 { &pod3 } else { &pod2 };
    let new_primary_pod = if new_primary == 2 { &pod2 } else { &pod3 };

    let secondary_len = surviving_secondary.state.read().await.data.len();
    let primary_len = new_primary_pod.state.read().await.data.len();

    assert_eq!(primary_len, 5, "new primary should have all 5 entries");
    assert!(
        (4..=5).contains(&secondary_len),
        "surviving secondary may roll back last op due to committed_lsn lag, got {}",
        secondary_len
    );
    assert!(
        secondary_len <= primary_len,
        "secondary should not have more data than primary"
    );

    driver.delete_partition().await.unwrap();
}
