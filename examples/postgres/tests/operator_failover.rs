use kuberic_core::driver::{PartitionDriver, ReplicaHandle};
use postgres_replicated::testing::{PgPod, cleanup_data_dir};
use serial_test::serial;
use test_log::test;

/// Driver test: 3 replicas, write data, failover, verify new primary
/// has the data and accepts new writes.
#[test(tokio::test)]
#[serial]
async fn test_driver_failover() {
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

    // Write data on primary
    let (client1, _conn1) = pod1.connect_pg().await;
    client1
        .execute(
            "CREATE TABLE failover_test (id SERIAL PRIMARY KEY, value TEXT NOT NULL)",
            &[],
        )
        .await
        .unwrap();
    for i in 1..=5 {
        client1
            .execute(
                "INSERT INTO failover_test (value) VALUES ($1)",
                &[&format!("row-{i}")],
            )
            .await
            .unwrap();
    }

    // Wait for replication to secondaries
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;

    // Verify secondary has the data before failover
    {
        let (client2, _conn2) = pod2.connect_pg().await;
        let rows = client2
            .query("SELECT count(*) as c FROM failover_test", &[])
            .await
            .unwrap();
        let count: i64 = rows[0].get("c");
        assert_eq!(count, 5, "secondary should have 5 rows before failover");
    }

    // Drop primary connection before failover
    drop(client1);
    drop(_conn1);

    // Failover: primary 1 dies
    driver.failover(1).await.unwrap();
    let new_primary_id = driver.primary_id().unwrap();
    assert_ne!(new_primary_id, 1, "new primary should not be pod 1");
    tracing::info!(new_primary_id, "failover complete");

    let new_primary_pod = if new_primary_id == 2 { &pod2 } else { &pod3 };

    // Wait for promotion to complete
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;

    // Verify new primary has the data
    let (new_client, _new_conn) = new_primary_pod.connect_pg().await;

    // Should NOT be in recovery (promoted to primary)
    let row = new_client
        .query_one("SELECT pg_is_in_recovery()", &[])
        .await
        .unwrap();
    let in_recovery: bool = row.get(0);
    assert!(!in_recovery, "new primary should not be in recovery");

    // Verify all 5 rows survived
    let rows = new_client
        .query("SELECT value FROM failover_test ORDER BY id", &[])
        .await
        .unwrap();
    assert_eq!(rows.len(), 5, "new primary should have all 5 rows");
    assert_eq!(rows[0].get::<_, &str>("value"), "row-1");
    assert_eq!(rows[4].get::<_, &str>("value"), "row-5");

    // Write new data on promoted primary
    new_client
        .execute(
            "INSERT INTO failover_test (value) VALUES ('post-failover')",
            &[],
        )
        .await
        .unwrap();

    let rows = new_client
        .query("SELECT count(*) as c FROM failover_test", &[])
        .await
        .unwrap();
    let count: i64 = rows[0].get("c");
    assert_eq!(count, 6, "should have 6 rows after post-failover write");

    tracing::info!("failover test passed: data survived + new writes work ✓");

    driver.delete_partition().await.unwrap();
    cleanup_data_dir(&pod1.data_dir);
    cleanup_data_dir(&pod2.data_dir);
    cleanup_data_dir(&pod3.data_dir);
}
