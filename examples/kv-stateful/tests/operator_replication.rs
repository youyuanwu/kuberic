use std::sync::Arc;
use std::time::Duration;

use kubelicate_core::driver::PartitionDriver;
use serial_test::serial;

use kv_stateful::proto;
use kv_stateful::testing::{KvPod, connect_kv_client, wait_for_state_count};

/// Operator-driven test: write on primary WHILE add_replica is copying state
/// to a new secondary. The PrimarySender should buffer these ops and replay
/// them after the copy completes. The new secondary should have both the
/// copied state AND the buffered ops.
#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
#[serial]
async fn test_operator_build_buffer_replay() {
    // Test parameters
    const INITIAL_ENTRIES: i64 = 500;
    const CONCURRENT_ENTRIES: i64 = 200;
    let total_entries = (INITIAL_ENTRIES + CONCURRENT_ENTRIES) as usize;

    // Start with 3 replicas — use longer reply timeout for concurrent load
    let timeout = Duration::from_secs(30);
    let pod1 = KvPod::start_with_timeout(1, timeout).await;
    let pod2 = KvPod::start_with_timeout(2, timeout).await;
    let pod3 = KvPod::start_with_timeout(3, timeout).await;

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
    for i in 1..=INITIAL_ENTRIES {
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
    let pod4 = KvPod::start_with_timeout(4, timeout).await;
    let h4 = pod4.replica_handle(4).await;

    // Start continuous writes in background BEFORE add_replica.
    // Use a notify to sync: background task signals after first write
    // succeeds, ensuring writes are flowing before copy starts.
    let started = Arc::new(tokio::sync::Notify::new());
    let started_clone = started.clone();
    let write_addr = pod1.client_address.clone();
    let write_handle = tokio::spawn(async move {
        let mut kv_bg = connect_kv_client(&write_addr).await;
        for i in 1..=CONCURRENT_ENTRIES {
            kv_bg
                .put(proto::PutRequest {
                    key: format!("during-copy-{}", i),
                    value: format!("buffered-{}", i),
                })
                .await
                .unwrap();
            if i == 1 {
                started_clone.notify_one();
            }
        }
    });

    // Wait until background task has completed at least one write
    started.notified().await;

    // add_replica blocks during copy — writes are happening concurrently
    driver.add_replica(Box::new(h4)).await.unwrap();

    // Wait for all entries to arrive on pod4
    write_handle.await.unwrap();
    wait_for_state_count(&pod4.state, total_entries).await;

    // Verify pod4 has all entries
    {
        let st = pod4.state.read().await;
        for i in 1..=INITIAL_ENTRIES {
            assert!(
                st.data.contains_key(&format!("initial-{}", i)),
                "pod4 missing initial-{i}"
            );
        }
        for i in 1..=CONCURRENT_ENTRIES {
            assert!(
                st.data.contains_key(&format!("during-copy-{}", i)),
                "pod4 missing during-copy-{i} (should come from build buffer replay)"
            );
        }
        assert_eq!(
            st.data.len(),
            total_entries,
            "pod4 should have {INITIAL_ENTRIES} initial + {CONCURRENT_ENTRIES} buffered = {total_entries} entries"
        );
    }

    driver.delete_partition().await.unwrap();
}
