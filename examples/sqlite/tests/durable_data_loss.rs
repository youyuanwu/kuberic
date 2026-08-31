use std::time::Duration;

use kuberic_core::driver::ReplicaHandle;
use kuberic_core::types::{
    DataLossAction, DurableActionResult, DurableActionState, DurableReplicaAction, Epoch, OpenMode,
    Role,
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
    handle.open(OpenMode::New).await.unwrap();
    handle.change_role(epoch, Role::Primary).await.unwrap();
    handle
        .execute_durable_action(
            "sqlite:data-loss",
            DurableReplicaAction::OnDataLoss { epoch },
        )
        .await
        .unwrap();

    for _ in 0..100 {
        let status = handle.get_status().await.unwrap();
        if status
            .durable_action
            .as_ref()
            .is_some_and(|action| action.state == DurableActionState::Completed)
        {
            assert_eq!(
                status.last_completed_action.unwrap().result,
                Some(DurableActionResult::DataLoss(DataLossAction::StateChanged))
            );
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    panic!("SQLite durable data-loss callback did not complete");
}
