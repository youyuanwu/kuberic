//! Demo mode: simulates operator + client for quick testing.

use std::time::Duration;

use tracing::{info, warn};

use crate::proto;

/// Simulate an operator: Open → Idle → Active → Primary.
pub async fn simulate_operator(control_address: String) {
    use kubelicate_core::proto::replicator_control_client::ReplicatorControlClient;
    use kubelicate_core::proto::*;

    tokio::time::sleep(Duration::from_millis(100)).await;

    let mut client = ReplicatorControlClient::connect(control_address)
        .await
        .expect("connect to control server");

    info!("--- Operator: Open ---");
    client.open(OpenRequest { mode: 0 }).await.unwrap();

    let epoch = Some(EpochProto {
        data_loss_number: 0,
        configuration_number: 1,
    });

    info!("--- Operator: Idle → Active → Primary ---");
    client
        .change_role(ChangeRoleRequest {
            epoch,
            role: RoleProto::RoleIdleSecondary as i32,
        })
        .await
        .unwrap();
    client
        .change_role(ChangeRoleRequest {
            epoch,
            role: RoleProto::RoleActiveSecondary as i32,
        })
        .await
        .unwrap();
    client
        .change_role(ChangeRoleRequest {
            epoch,
            role: RoleProto::RolePrimary as i32,
        })
        .await
        .unwrap();
}

/// Run demo client exercising Execute/Query via the SQL gRPC API.
pub async fn run_demo_client(client_bind: String) {
    tokio::time::sleep(Duration::from_millis(500)).await;

    let mut client = None;
    for attempt in 0..20 {
        match proto::sqlite_store_client::SqliteStoreClient::connect(format!(
            "http://{}",
            client_bind
        ))
        .await
        {
            Ok(c) => {
                client = Some(c);
                break;
            }
            Err(_) if attempt < 19 => {
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
            Err(e) => {
                warn!(error = %e, "failed to connect to SQL client server");
                return;
            }
        }
    }
    let mut client = client.unwrap();

    info!("=== Demo: Creating table ===");
    client
        .execute(proto::ExecuteRequest {
            sql: "CREATE TABLE IF NOT EXISTS demo (id INTEGER PRIMARY KEY, name TEXT, value REAL)"
                .to_string(),
            params: vec![],
        })
        .await
        .unwrap();

    info!("=== Demo: Inserting rows ===");
    for i in 1..=5 {
        let resp = client
            .execute(proto::ExecuteRequest {
                sql: format!(
                    "INSERT INTO demo (name, value) VALUES ('item-{}', {})",
                    i,
                    i as f64 * 1.5
                ),
                params: vec![],
            })
            .await
            .unwrap();
        info!(
            lsn = resp.get_ref().lsn,
            rowid = resp.get_ref().last_insert_rowid,
            "insert OK"
        );
    }

    info!("=== Demo: Querying rows ===");
    let resp = client
        .query(proto::QueryRequest {
            sql: "SELECT * FROM demo".to_string(),
            params: vec![],
        })
        .await
        .unwrap();
    let r = resp.get_ref();
    info!(columns = ?r.columns, rows = r.rows.len(), "query OK");

    info!("=== Demo: All operations complete ===");
}

/// Demote and close the replica.
pub async fn demo_close(control_address: String) {
    use kubelicate_core::proto::replicator_control_client::ReplicatorControlClient;
    use kubelicate_core::proto::*;

    let mut client = ReplicatorControlClient::connect(control_address)
        .await
        .unwrap();

    info!("--- Operator: Demote ---");
    client
        .change_role(ChangeRoleRequest {
            epoch: Some(EpochProto {
                data_loss_number: 0,
                configuration_number: 2,
            }),
            role: RoleProto::RoleActiveSecondary as i32,
        })
        .await
        .unwrap();

    info!("--- Operator: Close ---");
    client.close(CloseRequest {}).await.unwrap();
}
