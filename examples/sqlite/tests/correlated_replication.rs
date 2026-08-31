use std::time::Duration;

use kuberic_core::driver::ReplicaHandle;
use kuberic_core::grpc::handle::GrpcReplicaHandle;
use kuberic_core::types::{
    AgentControlVersion, CorrelatedControlActionRequest, DurableActionState, DurableReplicaAction,
    Epoch, OpenMode, ReplicaInfo, ReplicaSetConfig, ReplicaSetQuorumMode, ReplicaStatus, Role,
};
use serial_test::serial;
use sqlite_replicated::proto;
use sqlite_replicated::testing::{SqlitePod, connect_sqlite_client};

async fn execute(
    handle: &GrpcReplicaHandle,
    action_id: impl Into<String>,
    action: DurableReplicaAction,
) {
    let status = handle.get_status().await.unwrap();
    let action_id = action_id.into();
    let input_signature = action.signature();
    let acknowledgement = handle
        .execute_correlated_control_action(CorrelatedControlActionRequest {
            protocol_version: kuberic_core::replica_agent::CORRELATED_CONTROL_PROTOCOL_VERSION,
            action_id: action_id.clone(),
            input_signature,
            target_replica_id: handle.id(),
            target_instance_id: status.instance_id,
            expected_agent_generation: status.agent.generation,
            expected_control_version: AgentControlVersion::new(
                status.agent.control_version.value(),
            ),
            observed_runtime_epoch: status.epoch,
            action,
        })
        .await
        .unwrap();
    assert_ne!(
        acknowledgement.observation.action.state,
        DurableActionState::Failed,
        "correlated test action {action_id} failed: {:?}",
        acknowledgement.observation.action.error
    );
}

async fn wait_for_terminal(handle: &GrpcReplicaHandle, action_id: &str) {
    for _ in 0..200 {
        let status = handle.get_status().await.unwrap();
        if status
            .agent
            .retained_terminal_actions
            .iter()
            .any(|observation| {
                observation.action.action_id == action_id
                    && observation.action.state == DurableActionState::Completed
            })
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("correlated action {action_id} did not complete");
}

async fn replica_info(
    pod: &SqlitePod,
    handle: &GrpcReplicaHandle,
    role: Role,
    must_catch_up: bool,
) -> ReplicaInfo {
    let status = handle.get_status().await.unwrap();
    ReplicaInfo {
        id: handle.id(),
        instance_id: pod.instance_id.clone(),
        role,
        status: ReplicaStatus::Up,
        replicator_address: pod.data_address.clone(),
        current_progress: status.current_progress,
        catch_up_capability: status.catch_up_capability.unwrap_or_default(),
        must_catch_up,
    }
}

struct ThreePodPartition {
    pod1: SqlitePod,
    pod2: SqlitePod,
    pod3: SqlitePod,
    h1: GrpcReplicaHandle,
    h2: GrpcReplicaHandle,
    h3: GrpcReplicaHandle,
    epoch: Epoch,
}

async fn start_three_pod_partition() -> ThreePodPartition {
    let pod1 = SqlitePod::start(1).await;
    let pod2 = SqlitePod::start(2).await;
    let pod3 = SqlitePod::start(3).await;
    let h1 = pod1.replica_handle(1).await;
    let h2 = pod2.replica_handle(2).await;
    let h3 = pod3.replica_handle(3).await;
    let epoch = Epoch::new(0, 1);

    for (handle, id) in [(&h1, 1), (&h2, 2), (&h3, 3)] {
        execute(
            handle,
            format!("setup:{id}:open"),
            DurableReplicaAction::Open {
                mode: OpenMode::New,
            },
        )
        .await;
    }
    execute(
        &h1,
        "setup:primary",
        DurableReplicaAction::ChangeRole {
            epoch,
            role: Role::Primary,
        },
    )
    .await;

    for (pod, handle, id) in [(&pod2, &h2, 2), (&pod3, &h3, 3)] {
        execute(
            handle,
            format!("setup:{id}:idle"),
            DurableReplicaAction::ChangeRole {
                epoch,
                role: Role::IdleSecondary,
            },
        )
        .await;
        let build_id = format!("setup:{id}:build");
        execute(
            &h1,
            build_id.clone(),
            DurableReplicaAction::BuildReplica {
                replica: replica_info(pod, handle, Role::IdleSecondary, false).await,
            },
        )
        .await;
        wait_for_terminal(&h1, &build_id).await;
        execute(
            handle,
            format!("setup:{id}:active"),
            DurableReplicaAction::ChangeRole {
                epoch,
                role: Role::ActiveSecondary,
            },
        )
        .await;
    }

    let members = vec![
        replica_info(&pod2, &h2, Role::ActiveSecondary, true).await,
        replica_info(&pod3, &h3, Role::ActiveSecondary, true).await,
    ];
    let configuration = ReplicaSetConfig {
        members,
        write_quorum: 2,
    };
    execute(
        &h1,
        "setup:catch-up-configuration",
        DurableReplicaAction::UpdateCatchUpConfiguration {
            current: configuration.clone(),
            previous: ReplicaSetConfig {
                members: Vec::new(),
                write_quorum: 0,
            },
        },
    )
    .await;
    execute(
        &h1,
        "setup:wait-quorum",
        DurableReplicaAction::WaitForCatchUpQuorum {
            mode: ReplicaSetQuorumMode::Write,
        },
    )
    .await;
    execute(
        &h1,
        "setup:current-configuration",
        DurableReplicaAction::UpdateCurrentConfiguration {
            current: configuration,
        },
    )
    .await;

    ThreePodPartition {
        pod1,
        pod2,
        pod3,
        h1,
        h2,
        h3,
        epoch,
    }
}

async fn write_sqlite_fixture(pod: &SqlitePod) {
    let mut client = connect_sqlite_client(&pod.client_address).await;
    client
        .execute(proto::ExecuteRequest {
            sql: "CREATE TABLE replicated (id INTEGER PRIMARY KEY, payload TEXT)".to_string(),
            params: Vec::new(),
        })
        .await
        .unwrap();
    client
        .execute_batch(proto::ExecuteBatchRequest {
            statements: (1..=50)
                .map(|id| {
                    format!(
                        "INSERT INTO replicated VALUES ({id}, '{}')",
                        "x".repeat(200)
                    )
                })
                .collect(),
        })
        .await
        .unwrap();
    client
        .execute(proto::ExecuteRequest {
            sql: "ALTER TABLE replicated ADD COLUMN note TEXT".to_string(),
            params: Vec::new(),
        })
        .await
        .unwrap();
}

async fn assert_replicated_rows(pod: &SqlitePod) {
    let mut client = connect_sqlite_client(&pod.client_address).await;
    let response = client
        .query(proto::QueryRequest {
            sql: "SELECT COUNT(*) FROM replicated".to_string(),
            params: Vec::new(),
        })
        .await
        .unwrap();
    assert_eq!(
        response.get_ref().rows[0].values[0].kind,
        Some(proto::value::Kind::IntegerValue(50))
    );
}

#[test_log::test(tokio::test)]
#[serial]
async fn correlated_sqlite_replication_covers_multi_page_and_schema_changes() {
    let partition = start_three_pod_partition().await;
    write_sqlite_fixture(&partition.pod1).await;
    tokio::time::sleep(Duration::from_millis(500)).await;

    assert!(partition.pod2.state.lock().await.last_applied_lsn > 0);
    assert!(partition.pod3.state.lock().await.last_applied_lsn > 0);
}

#[test_log::test(tokio::test)]
#[serial]
async fn correlated_sqlite_switchover_preserves_data() {
    let partition = start_three_pod_partition().await;
    write_sqlite_fixture(&partition.pod1).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let next_epoch = Epoch::new(0, partition.epoch.configuration_number + 1);

    execute(
        &partition.h1,
        "switchover:revoke",
        DurableReplicaAction::RevokeWriteStatus,
    )
    .await;
    execute(
        &partition.h1,
        "switchover:demote",
        DurableReplicaAction::ChangeRole {
            epoch: next_epoch,
            role: Role::ActiveSecondary,
        },
    )
    .await;
    execute(
        &partition.h2,
        "switchover:promote",
        DurableReplicaAction::ChangeRole {
            epoch: next_epoch,
            role: Role::Primary,
        },
    )
    .await;
    execute(
        &partition.h3,
        "switchover:update-third-epoch",
        DurableReplicaAction::UpdateEpoch { epoch: next_epoch },
    )
    .await;
    assert_replicated_rows(&partition.pod2).await;
}

#[test_log::test(tokio::test)]
#[serial]
async fn correlated_sqlite_failover_preserves_data() {
    let partition = start_three_pod_partition().await;
    write_sqlite_fixture(&partition.pod1).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let next_epoch = Epoch::new(0, partition.epoch.configuration_number + 1);
    let pod2_address = partition.pod2.client_address.clone();

    partition.pod1.crash().await;
    execute(
        &partition.h2,
        "failover:update-candidate-epoch",
        DurableReplicaAction::UpdateEpoch { epoch: next_epoch },
    )
    .await;
    execute(
        &partition.h2,
        "failover:promote",
        DurableReplicaAction::ChangeRole {
            epoch: next_epoch,
            role: Role::Primary,
        },
    )
    .await;

    let mut client = connect_sqlite_client(&pod2_address).await;
    let response = client
        .query(proto::QueryRequest {
            sql: "SELECT COUNT(*) FROM replicated".to_string(),
            params: Vec::new(),
        })
        .await
        .unwrap();
    assert_eq!(
        response.get_ref().rows[0].values[0].kind,
        Some(proto::value::Kind::IntegerValue(50))
    );
}
