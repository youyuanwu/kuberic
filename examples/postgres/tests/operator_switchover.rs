use kuberic_core::driver::{PartitionDriver, ReplicaHandle};
use postgres_replicated::testing::{PgPod, cleanup_data_dir};
use serial_test::serial;
use test_log::test;

/// Switchover test: write data, switchover to pod 2, verify:
/// 1. Pre-switchover data survives on the new primary
/// 2. Old primary becomes a standby (in recovery, read-only)
/// 3. New primary accepts new writes
/// 4. Writing on old primary after switchover fails
#[test(tokio::test)]
#[serial]
async fn test_switchover_during_writes() {
    let pod1 = PgPod::start(1).await;
    let pod2 = PgPod::start(2).await;
    let pod3 = PgPod::start(3).await;

    let h1 = pod1.replica_handle(1).await;
    let h2 = pod2.replica_handle(2).await;
    let h3 = pod3.replica_handle(3).await;

    let mut driver = PartitionDriver::new();
    driver
        .create_partition(vec![
            Box::new(h1) as Box<dyn ReplicaHandle>,
            Box::new(h2),
            Box::new(h3),
        ])
        .await
        .unwrap();
    assert_eq!(driver.primary_id(), Some(1));

    // Write data on primary continuously
    let (client1, _conn1) = pod1.connect_pg().await;
    client1
        .execute(
            "CREATE TABLE switchover_test (id SERIAL PRIMARY KEY, value TEXT NOT NULL)",
            &[],
        )
        .await
        .unwrap();

    for i in 1..=20 {
        client1
            .execute(
                "INSERT INTO switchover_test (value) VALUES ($1)",
                &[&format!("row-{i}")],
            )
            .await
            .unwrap();
    }

    // Wait for replication to secondaries
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;

    // Verify secondary has all data before switchover
    {
        let (client2, _c2) = pod2.connect_pg().await;
        let rows = client2
            .query("SELECT count(*) as c FROM switchover_test", &[])
            .await
            .unwrap();
        let count: i64 = rows[0].get("c");
        assert_eq!(count, 20, "secondary should have 20 rows before switchover");
    }

    // Drop primary connection before switchover
    drop(client1);
    drop(_conn1);

    // === Switchover to pod 2 ===
    tracing::info!("=== starting switchover to pod 2 ===");
    driver.switchover(2).await.unwrap();
    assert_eq!(driver.primary_id(), Some(2));
    tracing::info!("=== switchover complete ===");

    // Wait for things to stabilize (ReconfigureStandby restarts PG on old primary)
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;

    // Verify new primary (pod 2) is writable and has all data
    let (client2, _conn2) = pod2.connect_pg().await;

    let row = client2
        .query_one("SELECT pg_is_in_recovery()", &[])
        .await
        .unwrap();
    let in_recovery: bool = row.get(0);
    assert!(!in_recovery, "pod 2 should be primary after switchover");

    let rows = client2
        .query("SELECT count(*) as c FROM switchover_test", &[])
        .await
        .unwrap();
    let count: i64 = rows[0].get("c");
    assert_eq!(count, 20, "new primary should have all 20 rows");

    // New primary accepts writes
    client2
        .execute(
            "INSERT INTO switchover_test (value) VALUES ('post-switchover')",
            &[],
        )
        .await
        .unwrap();
    tracing::info!("new primary accepts writes ✓");

    // === Key test: old primary (pod 1) should now be in recovery (standby) ===
    // This validates KP-3 fix (standby.signal created by ReconfigureStandby)
    let (client1_post, _conn1_post) = pod1.connect_pg().await;

    let row = client1_post
        .query_one("SELECT pg_is_in_recovery()", &[])
        .await
        .unwrap();
    let in_recovery: bool = row.get(0);
    assert!(
        in_recovery,
        "old primary (pod 1) should be in recovery after switchover (KP-3)"
    );

    // Writing on old primary should fail (it's a standby now)
    let write_result = client1_post
        .execute(
            "INSERT INTO switchover_test (value) VALUES ('should-fail')",
            &[],
        )
        .await;
    assert!(
        write_result.is_err(),
        "writes on old primary should fail — it is a standby"
    );
    tracing::info!("old primary rejects writes ✓");

    // Verify old primary can still read (hot_standby = on)
    let rows = client1_post
        .query("SELECT count(*) as c FROM switchover_test", &[])
        .await
        .unwrap();
    let count: i64 = rows[0].get("c");
    assert!(count >= 20, "old primary (standby) should still have data");
    tracing::info!(count, "old primary reads data as standby ✓");

    tracing::info!("switchover test passed: roles swapped, writes fenced, data survived ✓");

    driver.delete_partition().await.unwrap();
    cleanup_data_dir(&pod1.data_dir);
    cleanup_data_dir(&pod2.data_dir);
    cleanup_data_dir(&pod3.data_dir);
}
