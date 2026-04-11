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

/// Run demo client exercising Put/Get/Delete via the KV gRPC API.
pub async fn run_demo_client(client_bind: String) {
    tokio::time::sleep(Duration::from_millis(500)).await;

    let mut client = None;
    for attempt in 0..20 {
        match proto::kv_store_client::KvStoreClient::connect(format!("http://{}", client_bind))
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
                warn!(error = %e, "failed to connect to KV client server");
                return;
            }
        }
    }
    let mut client = client.unwrap();

    info!("=== Demo: Writing KV pairs ===");
    for i in 1..=5 {
        let resp = client
            .put(proto::PutRequest {
                key: format!("key-{}", i),
                value: format!("value-{}", i),
            })
            .await
            .unwrap();
        info!(lsn = resp.get_ref().lsn, key = %format!("key-{}", i), "put OK");
    }

    info!("=== Demo: Reading KV pairs ===");
    for i in 1..=5 {
        let resp = client
            .get(proto::GetRequest {
                key: format!("key-{}", i),
            })
            .await
            .unwrap();
        let r = resp.get_ref();
        info!(key = %format!("key-{}", i), found = r.found, value = %r.value, "get OK");
    }

    let resp = client
        .get(proto::GetRequest {
            key: "nonexistent".to_string(),
        })
        .await
        .unwrap();
    info!(key = "nonexistent", found = resp.get_ref().found, "get OK");

    info!("=== Demo: Deleting a key ===");
    let resp = client
        .delete(proto::DeleteRequest {
            key: "key-3".to_string(),
        })
        .await
        .unwrap();
    info!(
        key = "key-3",
        existed = resp.get_ref().existed,
        lsn = resp.get_ref().lsn,
        "delete OK"
    );

    let resp = client
        .get(proto::GetRequest {
            key: "key-3".to_string(),
        })
        .await
        .unwrap();
    info!(
        key = "key-3",
        found = resp.get_ref().found,
        "get after delete"
    );

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
