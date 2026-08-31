//! Demo mode: simulates operator + client for quick testing.

use std::time::Duration;

use tonic::transport::Channel;
use tracing::{info, warn};

use crate::proto;
use kuberic_core::proto::GetStatusRequest;
use kuberic_core::proto::replicator_control_client::ReplicatorControlClient;
use kuberic_core::types::{
    AgentControlVersion, AgentGeneration, CorrelatedControlActionRequest, DurableReplicaAction,
    Epoch, OpenMode, ReplicaInstanceId, Role,
};

async fn execute_action(
    client: &mut ReplicatorControlClient<Channel>,
    replica_id: i64,
    action_id: &str,
    action: DurableReplicaAction,
) {
    let status = client
        .get_status(GetStatusRequest {})
        .await
        .unwrap()
        .into_inner();
    let runtime_epoch = status.epoch.expect("runtime status must include epoch");
    let signature = action.signature();
    let observation = client
        .execute_correlated_control_action(
            kuberic_core::proto::ExecuteCorrelatedControlActionRequest::from(
                CorrelatedControlActionRequest {
                    protocol_version:
                        kuberic_core::replica_agent::CORRELATED_CONTROL_PROTOCOL_VERSION,
                    action_id: action_id.to_string(),
                    input_signature: signature,
                    target_replica_id: replica_id,
                    target_instance_id: ReplicaInstanceId::new(status.instance_id),
                    expected_agent_generation: AgentGeneration::parse(status.agent_generation)
                        .expect("valid agent generation"),
                    expected_control_version: AgentControlVersion::new(
                        status.agent_control_version,
                    ),
                    observed_runtime_epoch: runtime_epoch.into(),
                    action,
                },
            ),
        )
        .await
        .unwrap()
        .into_inner()
        .observation
        .expect("correlated action acknowledgement");
    let state = kuberic_core::proto::DurableActionStateProto::try_from(observation.state)
        .expect("known correlated action state");
    assert_ne!(
        state,
        kuberic_core::proto::DurableActionStateProto::DurableActionFailed,
        "correlated demo action {action_id} failed: {}",
        observation.error
    );
}

/// Simulate an operator: Open → Idle → Active → Primary.
pub async fn simulate_operator(control_address: String, replica_id: i64) {
    tokio::time::sleep(Duration::from_millis(100)).await;

    let mut client = ReplicatorControlClient::connect(control_address)
        .await
        .expect("connect to control server");

    info!("--- Operator: Open ---");
    execute_action(
        &mut client,
        replica_id,
        "demo-open",
        DurableReplicaAction::Open {
            mode: OpenMode::New,
        },
    )
    .await;

    info!("--- Operator: Idle → Active → Primary ---");
    for (action_id, role) in [
        ("demo-idle", Role::IdleSecondary),
        ("demo-active", Role::ActiveSecondary),
        ("demo-primary", Role::Primary),
    ] {
        execute_action(
            &mut client,
            replica_id,
            action_id,
            DurableReplicaAction::ChangeRole {
                epoch: Epoch::new(0, 1),
                role,
            },
        )
        .await;
    }
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
pub async fn demo_close(control_address: String, replica_id: i64) {
    let mut client = ReplicatorControlClient::connect(control_address)
        .await
        .unwrap();

    info!("--- Operator: Demote ---");
    execute_action(
        &mut client,
        replica_id,
        "demo-demote",
        DurableReplicaAction::ChangeRole {
            epoch: Epoch::new(0, 2),
            role: Role::ActiveSecondary,
        },
    )
    .await;

    info!("--- Operator: Close ---");
    execute_action(
        &mut client,
        replica_id,
        "demo-close",
        DurableReplicaAction::Close,
    )
    .await;
}
