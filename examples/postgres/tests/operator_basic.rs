use kuberic_core::driver::{PartitionDriver, ReplicaHandle};
use postgres_replicated::testing::{PgPod, cleanup_data_dir};
use serial_test::serial;
use test_log::test;

/// Driver test: single replica partition. Write data on primary,
/// verify via PG query.
#[test(tokio::test)]
#[serial]
async fn test_driver_single_replica() {
    let pod = PgPod::start(1).await;
    let h1 = pod.replica_handle(1).await;

    let mut driver = PartitionDriver::new();
    driver
        .create_partition(vec![Box::new(h1) as Box<dyn ReplicaHandle>])
        .await
        .unwrap();
    assert_eq!(driver.primary_id(), Some(1));

    // Connect to PG and write data
    let (client, _conn) = pod.connect_pg().await;
    client
        .execute(
            "CREATE TABLE driver_test (id SERIAL PRIMARY KEY, value TEXT)",
            &[],
        )
        .await
        .unwrap();
    client
        .execute(
            "INSERT INTO driver_test (value) VALUES ('from-driver')",
            &[],
        )
        .await
        .unwrap();

    // Read back
    let row = client
        .query_one("SELECT value FROM driver_test WHERE id = 1", &[])
        .await
        .unwrap();
    let v: &str = row.get("value");
    assert_eq!(v, "from-driver");

    driver.delete_partition().await.unwrap();
    cleanup_data_dir(&pod.data_dir);
}

/// Driver test: 3 replicas, write on primary, verify replication to
/// secondaries via pg_stat_replication.
#[test(tokio::test)]
#[serial]
async fn test_driver_three_replicas() {
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
    let (client, _conn) = pod1.connect_pg().await;
    client
        .execute("CREATE TABLE repl_test (id SERIAL, val TEXT)", &[])
        .await
        .unwrap();
    for i in 1..=3 {
        client
            .execute(
                "INSERT INTO repl_test (val) VALUES ($1)",
                &[&format!("row-{i}")],
            )
            .await
            .unwrap();
    }

    // Give replication a moment
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;

    // Verify secondary 2 has the data (hot_standby = on allows read queries)
    let (client2, _conn2) = pod2.connect_pg().await;
    let rows = client2
        .query("SELECT val FROM repl_test ORDER BY id", &[])
        .await
        .unwrap();
    assert_eq!(rows.len(), 3, "secondary should have 3 rows");
    assert_eq!(rows[0].get::<_, &str>("val"), "row-1");
    assert_eq!(rows[2].get::<_, &str>("val"), "row-3");

    // Verify secondary 3 also has data
    let (client3, _conn3) = pod3.connect_pg().await;
    let rows = client3
        .query("SELECT val FROM repl_test ORDER BY id", &[])
        .await
        .unwrap();
    assert_eq!(rows.len(), 3, "secondary 3 should have 3 rows");

    driver.delete_partition().await.unwrap();
    cleanup_data_dir(&pod1.data_dir);
    cleanup_data_dir(&pod2.data_dir);
    cleanup_data_dir(&pod3.data_dir);
}
