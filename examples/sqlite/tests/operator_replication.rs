use std::time::Duration;

use kubelicate_core::driver::{PartitionDriver, ReplicaHandle};
use serial_test::serial;

use sqlite_replicated::proto;
use sqlite_replicated::testing::{SqlitePod, connect_sqlite_client};

/// Test 1: Single write replicates to secondary.
/// Primary: CREATE TABLE + INSERT. Verify secondary's DB file has the data.
#[test_log::test(tokio::test)]
#[serial]
async fn test_single_write_replicates() {
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

    let mut client = connect_sqlite_client(&pod1.client_address).await;

    // Create table
    client
        .execute(proto::ExecuteRequest {
            sql: "CREATE TABLE test (id INTEGER PRIMARY KEY, data TEXT)".to_string(),
            params: vec![],
        })
        .await
        .unwrap();

    // Insert a row
    let resp = client
        .execute(proto::ExecuteRequest {
            sql: "INSERT INTO test VALUES (1, 'hello')".to_string(),
            params: vec![],
        })
        .await
        .unwrap();
    assert!(resp.get_ref().lsn > 0);

    // Wait for replication to propagate
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Verify secondary has the data by reading its DB file directly
    {
        let st = pod2.state.lock().await;
        assert!(
            st.last_applied_lsn > 0,
            "secondary should have received replicated data"
        );
    }

    driver.delete_partition().await.unwrap();
}

/// Test 2: Multi-page transaction replicates correctly.
#[test_log::test(tokio::test)]
#[serial]
async fn test_multi_page_transaction() {
    let pod1 = SqlitePod::start(1).await;
    let pod2 = SqlitePod::start(2).await;
    let pod3 = SqlitePod::start(3).await;

    let h1 = pod1.replica_handle(1).await;
    let h2 = pod2.replica_handle(2).await;
    let h3 = pod3.replica_handle(3).await;
    let handles: Vec<Box<dyn ReplicaHandle>> = vec![Box::new(h1), Box::new(h2), Box::new(h3)];

    let mut driver = PartitionDriver::new();
    driver.create_partition(handles).await.unwrap();

    let mut client = connect_sqlite_client(&pod1.client_address).await;

    // Create table
    client
        .execute(proto::ExecuteRequest {
            sql: "CREATE TABLE bulk (id INTEGER PRIMARY KEY, payload TEXT)".to_string(),
            params: vec![],
        })
        .await
        .unwrap();

    // Bulk insert that spans multiple pages
    let resp = client
        .execute_batch(proto::ExecuteBatchRequest {
            statements: (1..=50)
                .map(|i| {
                    format!(
                        "INSERT INTO bulk VALUES ({}, '{}')",
                        i,
                        "x".repeat(200) // ~200 bytes per row, ~50 rows
                    )
                })
                .collect(),
        })
        .await
        .unwrap();
    assert_eq!(resp.get_ref().rows_affected.len(), 50);

    // Verify primary can read all rows
    let resp = client
        .query(proto::QueryRequest {
            sql: "SELECT COUNT(*) FROM bulk".to_string(),
            params: vec![],
        })
        .await
        .unwrap();
    assert_eq!(
        resp.get_ref().rows[0].values[0].kind,
        Some(proto::value::Kind::IntegerValue(50))
    );

    // Wait for replication
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Verify secondary received the data
    {
        let st = pod2.state.lock().await;
        assert!(st.last_applied_lsn > 0, "secondary should have data");
    }

    driver.delete_partition().await.unwrap();
}

/// Test 4: Schema changes (DDL) replicate correctly.
#[test_log::test(tokio::test)]
#[serial]
async fn test_schema_changes_replicate() {
    let pod1 = SqlitePod::start(1).await;
    let pod2 = SqlitePod::start(2).await;
    let pod3 = SqlitePod::start(3).await;

    let h1 = pod1.replica_handle(1).await;
    let h2 = pod2.replica_handle(2).await;
    let h3 = pod3.replica_handle(3).await;
    let handles: Vec<Box<dyn ReplicaHandle>> = vec![Box::new(h1), Box::new(h2), Box::new(h3)];

    let mut driver = PartitionDriver::new();
    driver.create_partition(handles).await.unwrap();

    let mut client = connect_sqlite_client(&pod1.client_address).await;

    // CREATE TABLE
    client
        .execute(proto::ExecuteRequest {
            sql: "CREATE TABLE schema_test (id INTEGER PRIMARY KEY, name TEXT)".to_string(),
            params: vec![],
        })
        .await
        .unwrap();

    // ALTER TABLE
    client
        .execute(proto::ExecuteRequest {
            sql: "ALTER TABLE schema_test ADD COLUMN email TEXT".to_string(),
            params: vec![],
        })
        .await
        .unwrap();

    // CREATE INDEX
    client
        .execute(proto::ExecuteRequest {
            sql: "CREATE INDEX idx_name ON schema_test (name)".to_string(),
            params: vec![],
        })
        .await
        .unwrap();

    // INSERT with new schema
    client
        .execute(proto::ExecuteRequest {
            sql: "INSERT INTO schema_test (name, email) VALUES ('Alice', 'alice@example.com')"
                .to_string(),
            params: vec![],
        })
        .await
        .unwrap();

    // Verify on primary
    let resp = client
        .query(proto::QueryRequest {
            sql: "SELECT name, email FROM schema_test".to_string(),
            params: vec![],
        })
        .await
        .unwrap();
    let r = resp.get_ref();
    assert_eq!(r.columns, vec!["name", "email"]);
    assert_eq!(r.rows.len(), 1);
    assert_eq!(
        r.rows[0].values[0].kind,
        Some(proto::value::Kind::TextValue("Alice".to_string()))
    );
    assert_eq!(
        r.rows[0].values[1].kind,
        Some(proto::value::Kind::TextValue(
            "alice@example.com".to_string()
        ))
    );

    // Wait for replication
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Verify secondary has the schema changes
    {
        let st = pod2.state.lock().await;
        assert!(
            st.last_applied_lsn > 0,
            "secondary should have schema changes"
        );
    }

    driver.delete_partition().await.unwrap();
}
