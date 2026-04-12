use std::time::Duration;

use kuberic_core::driver::{PartitionDriver, ReplicaHandle};
use serial_test::serial;

use sqlite_replicated::proto;
use sqlite_replicated::testing::{SqlitePod, connect_sqlite_client};

/// Test 5: Write data on primary, trigger failover, verify data survives
/// on the new primary. The new primary opens the replicated DB file
/// and serves the same data via SELECT.
#[test_log::test(tokio::test)]
#[serial]
async fn test_failover_data_survives() {
    let pod1 = SqlitePod::start(1).await;
    let pod2 = SqlitePod::start(2).await;
    let pod3 = SqlitePod::start(3).await;

    let h1 = pod1.replica_handle(1).await;
    let h2 = pod2.replica_handle(2).await;
    let h3 = pod3.replica_handle(3).await;
    let handles: Vec<Box<dyn ReplicaHandle>> = vec![Box::new(h1), Box::new(h2), Box::new(h3)];

    let mut driver = PartitionDriver::new();
    driver.create_partition(handles).await.unwrap();
    assert_eq!(driver.primary_id(), Some(1));

    // Write data on primary
    let mut client = connect_sqlite_client(&pod1.client_address).await;

    client
        .execute(proto::ExecuteRequest {
            sql: "CREATE TABLE failover_test (id INTEGER PRIMARY KEY, msg TEXT)".to_string(),
            params: vec![],
        })
        .await
        .unwrap();

    for i in 1..=5 {
        client
            .execute(proto::ExecuteRequest {
                sql: format!("INSERT INTO failover_test VALUES ({}, 'message-{}')", i, i),
                params: vec![],
            })
            .await
            .unwrap();
    }

    // Verify primary has all data
    let resp = client
        .query(proto::QueryRequest {
            sql: "SELECT COUNT(*) FROM failover_test".to_string(),
            params: vec![],
        })
        .await
        .unwrap();
    assert_eq!(
        resp.get_ref().rows[0].values[0].kind,
        Some(proto::value::Kind::IntegerValue(5))
    );

    // Wait for replication to propagate fully
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Failover: primary 1 "fails", promote best secondary
    driver.failover(1).await.unwrap();
    let new_primary = driver.primary_id().unwrap();
    assert_ne!(new_primary, 1);

    // Connect to new primary
    let new_primary_pod = if new_primary == 2 { &pod2 } else { &pod3 };

    // Retry connection — new primary's gRPC server may not be registered yet
    let mut new_client = retry_connect(&new_primary_pod.client_address).await;

    // Data from before failover should be readable on new primary
    let resp = retry_query(
        &mut new_client,
        "SELECT COUNT(*) FROM failover_test".to_string(),
    )
    .await;
    assert_eq!(
        resp.rows[0].values[0].kind,
        Some(proto::value::Kind::IntegerValue(5)),
        "all 5 rows should survive failover"
    );

    // Verify specific row content
    let resp = retry_query(
        &mut new_client,
        "SELECT msg FROM failover_test WHERE id = 3".to_string(),
    )
    .await;
    assert_eq!(
        resp.rows[0].values[0].kind,
        Some(proto::value::Kind::TextValue("message-3".to_string()))
    );

    // New primary accepts writes
    let resp = new_client
        .execute(proto::ExecuteRequest {
            sql: "INSERT INTO failover_test VALUES (6, 'post-failover')".to_string(),
            params: vec![],
        })
        .await
        .unwrap();
    assert!(resp.get_ref().lsn > 0);

    // Read the new row
    let resp = retry_query(
        &mut new_client,
        "SELECT msg FROM failover_test WHERE id = 6".to_string(),
    )
    .await;
    assert_eq!(
        resp.rows[0].values[0].kind,
        Some(proto::value::Kind::TextValue("post-failover".to_string()))
    );

    driver.delete_partition().await.unwrap();
}

/// Test: Switchover preserves data — old primary demoted, new primary
/// has all pre-switchover data and accepts new writes.
#[test_log::test(tokio::test)]
#[serial]
async fn test_switchover_data_survives() {
    let pod1 = SqlitePod::start(1).await;
    let pod2 = SqlitePod::start(2).await;
    let pod3 = SqlitePod::start(3).await;

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
    let mut client1 = connect_sqlite_client(&pod1.client_address).await;

    client1
        .execute(proto::ExecuteRequest {
            sql: "CREATE TABLE sw_test (key TEXT PRIMARY KEY, val TEXT)".to_string(),
            params: vec![],
        })
        .await
        .unwrap();

    client1
        .execute(proto::ExecuteRequest {
            sql: "INSERT INTO sw_test VALUES ('before', 'switchover')".to_string(),
            params: vec![],
        })
        .await
        .unwrap();

    // Switchover to pod 2
    driver.switchover(2).await.unwrap();
    assert_eq!(driver.primary_id(), Some(2));

    // New primary (pod 2) accepts writes
    let mut client2 = retry_connect(&pod2.client_address).await;

    let resp = client2
        .execute(proto::ExecuteRequest {
            sql: "INSERT INTO sw_test VALUES ('after', 'switchover')".to_string(),
            params: vec![],
        })
        .await
        .unwrap();
    assert!(resp.get_ref().lsn > 0);

    // Pre-switchover data readable on new primary
    let resp = retry_query(
        &mut client2,
        "SELECT val FROM sw_test WHERE key = 'before'".to_string(),
    )
    .await;
    assert_eq!(
        resp.rows[0].values[0].kind,
        Some(proto::value::Kind::TextValue("switchover".to_string()))
    );

    // Old primary (pod 1) rejects writes
    let result = client1
        .execute(proto::ExecuteRequest {
            sql: "INSERT INTO sw_test VALUES ('should-fail', 'x')".to_string(),
            params: vec![],
        })
        .await;
    assert!(
        result.is_err(),
        "old primary should reject writes after switchover"
    );

    driver.delete_partition().await.unwrap();
}

/// Helper: connect with retries (new primary may not have gRPC server ready yet).
async fn retry_connect(
    addr: &str,
) -> proto::sqlite_store_client::SqliteStoreClient<tonic::transport::Channel> {
    connect_sqlite_client(addr).await
}

/// Helper: retry a query (handles Unimplemented errors right after promotion).
async fn retry_query(
    client: &mut proto::sqlite_store_client::SqliteStoreClient<tonic::transport::Channel>,
    sql: String,
) -> proto::QueryResponse {
    for attempt in 0..30 {
        match client
            .query(proto::QueryRequest {
                sql: sql.clone(),
                params: vec![],
            })
            .await
        {
            Ok(resp) => return resp.into_inner(),
            Err(e) if attempt < 29 => {
                tracing::debug!(attempt, error = %e, "query retry");
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            Err(e) => panic!("query failed after retries: {}", e),
        }
    }
    unreachable!()
}
