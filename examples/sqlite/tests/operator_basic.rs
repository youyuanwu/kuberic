use kubelicate_core::driver::{PartitionDriver, ReplicaHandle};
use serial_test::serial;

use sqlite_replicated::proto;
use sqlite_replicated::testing::{SqlitePod, connect_sqlite_client};

/// Test 12: Full CRUD via gRPC Execute/Query.
#[test_log::test(tokio::test)]
#[serial]
async fn test_execute_insert_update_delete() {
    let pod = SqlitePod::start(1).await;

    let h1 = pod.replica_handle(1).await;
    let handles: Vec<Box<dyn ReplicaHandle>> = vec![Box::new(h1)];

    let mut driver = PartitionDriver::new();
    driver.create_partition(handles).await.unwrap();
    assert_eq!(driver.primary_id(), Some(1));

    let mut client = connect_sqlite_client(&pod.client_address).await;

    // CREATE TABLE
    client
        .execute(proto::ExecuteRequest {
            sql: "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT, age INTEGER)".to_string(),
            params: vec![],
        })
        .await
        .unwrap();

    // INSERT
    let resp = client
        .execute(proto::ExecuteRequest {
            sql: "INSERT INTO users (name, age) VALUES ('Alice', 30)".to_string(),
            params: vec![],
        })
        .await
        .unwrap();
    assert_eq!(resp.get_ref().rows_affected, 1);
    assert_eq!(resp.get_ref().last_insert_rowid, 1);

    let resp = client
        .execute(proto::ExecuteRequest {
            sql: "INSERT INTO users (name, age) VALUES ('Bob', 25)".to_string(),
            params: vec![],
        })
        .await
        .unwrap();
    assert_eq!(resp.get_ref().rows_affected, 1);
    assert_eq!(resp.get_ref().last_insert_rowid, 2);

    // UPDATE
    let resp = client
        .execute(proto::ExecuteRequest {
            sql: "UPDATE users SET age = 31 WHERE name = 'Alice'".to_string(),
            params: vec![],
        })
        .await
        .unwrap();
    assert_eq!(resp.get_ref().rows_affected, 1);

    // DELETE
    let resp = client
        .execute(proto::ExecuteRequest {
            sql: "DELETE FROM users WHERE name = 'Bob'".to_string(),
            params: vec![],
        })
        .await
        .unwrap();
    assert_eq!(resp.get_ref().rows_affected, 1);

    // Verify final state
    let resp = client
        .query(proto::QueryRequest {
            sql: "SELECT name, age FROM users".to_string(),
            params: vec![],
        })
        .await
        .unwrap();
    let r = resp.get_ref();
    assert_eq!(r.columns, vec!["name", "age"]);
    assert_eq!(r.rows.len(), 1);
    assert_eq!(
        r.rows[0].values[0].kind,
        Some(proto::value::Kind::TextValue("Alice".to_string()))
    );
    assert_eq!(
        r.rows[0].values[1].kind,
        Some(proto::value::Kind::IntegerValue(31))
    );

    driver.delete_partition().await.unwrap();
}

/// Test 3: Basic read-after-write round-trip.
#[test_log::test(tokio::test)]
#[serial]
async fn test_read_after_write() {
    let pod = SqlitePod::start(2).await;

    let h = pod.replica_handle(2).await;
    let mut driver = PartitionDriver::new();
    driver.create_partition(vec![Box::new(h)]).await.unwrap();

    let mut client = connect_sqlite_client(&pod.client_address).await;

    client
        .execute(proto::ExecuteRequest {
            sql: "CREATE TABLE kv (key TEXT PRIMARY KEY, value TEXT)".to_string(),
            params: vec![],
        })
        .await
        .unwrap();

    client
        .execute(proto::ExecuteRequest {
            sql: "INSERT INTO kv VALUES ('greeting', 'hello world')".to_string(),
            params: vec![],
        })
        .await
        .unwrap();

    let resp = client
        .query(proto::QueryRequest {
            sql: "SELECT value FROM kv WHERE key = 'greeting'".to_string(),
            params: vec![],
        })
        .await
        .unwrap();
    let r = resp.get_ref();
    assert_eq!(r.rows.len(), 1);
    assert_eq!(
        r.rows[0].values[0].kind,
        Some(proto::value::Kind::TextValue("hello world".to_string()))
    );

    driver.delete_partition().await.unwrap();
}

/// Test 14: Query returning multiple rows.
#[test_log::test(tokio::test)]
#[serial]
async fn test_query_returns_rows() {
    let pod = SqlitePod::start(3).await;

    let h = pod.replica_handle(3).await;
    let mut driver = PartitionDriver::new();
    driver.create_partition(vec![Box::new(h)]).await.unwrap();

    let mut client = connect_sqlite_client(&pod.client_address).await;

    client
        .execute(proto::ExecuteRequest {
            sql: "CREATE TABLE items (id INTEGER PRIMARY KEY, name TEXT, price REAL)".to_string(),
            params: vec![],
        })
        .await
        .unwrap();

    for i in 1..=5 {
        client
            .execute(proto::ExecuteRequest {
                sql: format!(
                    "INSERT INTO items (name, price) VALUES ('item-{}', {:.1})",
                    i,
                    i as f64 * 9.99
                ),
                params: vec![],
            })
            .await
            .unwrap();
    }

    let resp = client
        .query(proto::QueryRequest {
            sql: "SELECT id, name, price FROM items ORDER BY id".to_string(),
            params: vec![],
        })
        .await
        .unwrap();
    let r = resp.get_ref();
    assert_eq!(r.columns, vec!["id", "name", "price"]);
    assert_eq!(r.rows.len(), 5);
    assert_eq!(
        r.rows[0].values[1].kind,
        Some(proto::value::Kind::TextValue("item-1".to_string()))
    );

    driver.delete_partition().await.unwrap();
}

/// Test 13: ExecuteBatch atomicity.
#[test_log::test(tokio::test)]
#[serial]
async fn test_execute_batch_transaction() {
    let pod = SqlitePod::start(4).await;

    let h = pod.replica_handle(4).await;
    let mut driver = PartitionDriver::new();
    driver.create_partition(vec![Box::new(h)]).await.unwrap();

    let mut client = connect_sqlite_client(&pod.client_address).await;

    // Create table + insert multiple rows in one batch
    let resp = client
        .execute_batch(proto::ExecuteBatchRequest {
            statements: vec![
                "CREATE TABLE batch_test (id INTEGER PRIMARY KEY, val TEXT)".to_string(),
                "INSERT INTO batch_test VALUES (1, 'one')".to_string(),
                "INSERT INTO batch_test VALUES (2, 'two')".to_string(),
                "INSERT INTO batch_test VALUES (3, 'three')".to_string(),
            ],
        })
        .await
        .unwrap();
    let r = resp.get_ref();
    assert_eq!(r.rows_affected.len(), 4);

    let resp = client
        .query(proto::QueryRequest {
            sql: "SELECT COUNT(*) FROM batch_test".to_string(),
            params: vec![],
        })
        .await
        .unwrap();
    assert_eq!(
        resp.get_ref().rows[0].values[0].kind,
        Some(proto::value::Kind::IntegerValue(3))
    );

    driver.delete_partition().await.unwrap();
}
