use std::time::Duration;

use kuberic_core::driver::ReplicaHandle;
use kuberic_core::types::{
    AgentControlVersion, CorrelatedControlActionRequest, DataLossAction, DurableActionResult,
    DurableActionState, DurableReplicaAction, Epoch, OpenMode, Role,
};
use serial_test::serial;
use sqlite_replicated::service::DataLossBehavior;
use sqlite_replicated::testing::SqlitePod;

#[tokio::test]
#[serial]
async fn sqlite_durable_data_loss_result_uses_real_runtime_path() {
    let epoch = Epoch::new(4, 2);
    let pod = SqlitePod::start_with_data_loss_behavior(1, DataLossBehavior::StateChanged).await;
    let handle = pod.replica_handle(1).await;
    for (action_id, action) in [
        (
            "sqlite:open",
            DurableReplicaAction::Open {
                mode: OpenMode::New,
            },
        ),
        (
            "sqlite:primary",
            DurableReplicaAction::ChangeRole {
                epoch,
                role: Role::Primary,
            },
        ),
        (
            "sqlite:data-loss",
            DurableReplicaAction::OnDataLoss { epoch },
        ),
    ] {
        let status = handle.get_status().await.unwrap();
        let signature = action.signature();
        handle
            .execute_correlated_control_action(CorrelatedControlActionRequest {
                protocol_version: kuberic_core::replica_agent::CORRELATED_CONTROL_PROTOCOL_VERSION,
                action_id: action_id.to_string(),
                input_signature: signature,
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
    }

    for _ in 0..100 {
        let status = handle.get_status().await.unwrap();
        if let Some(completed) =
            status
                .agent
                .retained_terminal_actions
                .iter()
                .rev()
                .find(|observation| {
                    observation.action.action_id == "sqlite:data-loss"
                        && observation.action.state == DurableActionState::Completed
                })
        {
            assert_eq!(
                completed.action.result,
                Some(DurableActionResult::DataLoss(DataLossAction::StateChanged))
            );
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    panic!("SQLite durable data-loss callback did not complete");
}
