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
