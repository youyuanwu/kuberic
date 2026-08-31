use std::time::Duration;

use kuberic_core::driver::ReplicaHandle;
use kuberic_core::types::{
    DataLossAction, DurableActionResult, DurableActionState, DurableReplicaAction, Epoch, OpenMode,
    ReplicaConfigurationMemberStatus, ReplicaConfigurationMode, ReplicaConfigurationStatus,
    ReplicaElectionConfiguration, ReplicaInstanceId, Role,
};
use kvstore::service::DataLossBehavior;
use kvstore::testing::KvPod;
use serial_test::serial;

async fn open_primary(pod: &KvPod, epoch: Epoch) -> kuberic_core::grpc::handle::GrpcReplicaHandle {
    let handle = pod.replica_handle(1).await;
    handle.open(OpenMode::New).await.unwrap();
    handle.change_role(epoch, Role::Primary).await.unwrap();
    handle
}

async fn wait_for_terminal(
    handle: &kuberic_core::grpc::handle::GrpcReplicaHandle,
) -> kuberic_core::types::ReplicaStatusInfo {
    for _ in 0..100 {
        let status = handle.get_status().await.unwrap();
        if status.durable_action.as_ref().is_some_and(|action| {
            matches!(
                action.state,
                DurableActionState::Completed | DurableActionState::Failed
            )
        }) {
            return status;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("durable action did not reach a terminal state");
}

#[tokio::test]
#[serial]
async fn durable_data_loss_is_correlated_and_observable() {
    let epoch = Epoch::new(2, 7);
    let pod = KvPod::start_with_data_loss_behavior(1, DataLossBehavior::StateChanged).await;
    let handle = open_primary(&pod, epoch).await;
    pod.state.write().await.last_applied_lsn = 11;

    handle
        .execute_durable_action(
            "failover:data-loss",
            DurableReplicaAction::OnDataLoss { epoch },
        )
        .await
        .unwrap();
    let status = wait_for_terminal(&handle).await;
    let agent = status.agent.as_ref().expect("replica-agent status");
    assert!(
        agent
            .capabilities
            .contains(&kuberic_core::types::ReplicaAgentCapability::CorrelatedControlActionV1)
    );
    assert_eq!(agent.retained_terminal_actions.len(), 1);
    let completion = status.last_completed_action.unwrap();
    assert_eq!(completion.action_id, "failover:data-loss");
    assert_eq!(
        completion.result,
        Some(DurableActionResult::DataLoss(DataLossAction::StateChanged))
    );
    assert_eq!(status.current_progress, 11);
    assert_eq!(status.committed_lsn, 11);
    assert_eq!(status.catch_up_capability, Some(11));
    assert_eq!(status.deactivation_info.unwrap().catch_up_lsn, 11);

    handle
        .execute_durable_action(
            "failover:data-loss",
            DurableReplicaAction::OnDataLoss { epoch },
        )
        .await
        .unwrap();
    assert_eq!(
        handle
            .get_status()
            .await
            .unwrap()
            .last_completed_action
            .unwrap()
            .result,
        completion.result
    );
    assert!(
        handle
            .execute_durable_action(
                "failover:data-loss",
                DurableReplicaAction::OnDataLoss {
                    epoch: Epoch::new(2, 8),
                },
            )
            .await
            .is_err()
    );

    handle.change_role(epoch, Role::None).await.unwrap();
    let demoted = handle.get_status().await.unwrap();
    assert_eq!(demoted.deactivation_info.unwrap().catch_up_lsn, 11);
}

#[tokio::test]
#[serial]
async fn durable_data_loss_exposes_in_progress_and_failure() {
    let epoch = Epoch::new(3, 9);
    let delayed = KvPod::start_with_data_loss_behavior(
        1,
        DataLossBehavior::Delay {
            duration: Duration::from_millis(250),
            state_changed: false,
        },
    )
    .await;
    let delayed_handle = open_primary(&delayed, epoch).await;
    delayed_handle
        .execute_durable_action(
            "failover:data-loss:delayed",
            DurableReplicaAction::OnDataLoss { epoch },
        )
        .await
        .unwrap();
    assert_eq!(
        delayed_handle
            .get_status()
            .await
            .unwrap()
            .durable_action
            .unwrap()
            .state,
        DurableActionState::InProgress
    );
    assert_eq!(
        wait_for_terminal(&delayed_handle)
            .await
            .last_completed_action
            .unwrap()
            .result,
        Some(DurableActionResult::DataLoss(DataLossAction::None))
    );

    let failed =
        KvPod::start_with_data_loss_behavior(1, DataLossBehavior::Fail("injected".into())).await;
    let failed_handle = open_primary(&failed, epoch).await;
    failed_handle
        .execute_durable_action(
            "failover:data-loss:failed",
            DurableReplicaAction::OnDataLoss { epoch },
        )
        .await
        .unwrap();
    let status = wait_for_terminal(&failed_handle).await;
    let action = status.durable_action.unwrap();
    assert_eq!(action.state, DurableActionState::Failed);
    assert!(action.error.unwrap().contains("injected"));
    assert!(action.result.is_none());
}

#[tokio::test]
#[serial]
async fn election_configuration_and_deactivation_are_incarnation_local() {
    let epoch = Epoch::new(1, 4);
    let pod = KvPod::start(1).await;
    let handle = open_primary(&pod, epoch).await;
    let current = ReplicaConfigurationStatus {
        mode: ReplicaConfigurationMode::Current,
        members: vec![ReplicaConfigurationMemberStatus {
            id: 1,
            instance_id: pod.instance_id.clone(),
            role: Role::Primary,
        }],
        write_quorum: 1,
    };
    let configuration = ReplicaElectionConfiguration {
        previous: None,
        current: current.clone(),
    };
    let runtime_config = kuberic_core::types::ReplicaSetConfig {
        members: vec![kuberic_core::types::ReplicaInfo {
            id: 1,
            instance_id: pod.instance_id.clone(),
            role: Role::Primary,
            status: kuberic_core::types::ReplicaStatus::Up,
            replicator_address: pod.data_address.clone(),
            current_progress: 0,
            catch_up_capability: 0,
            must_catch_up: false,
        }],
        write_quorum: 1,
    };
    handle
        .update_current_configuration(runtime_config)
        .await
        .unwrap();
    assert!(
        handle
            .get_status()
            .await
            .unwrap()
            .election_configuration
            .is_none()
    );
    handle
        .execute_durable_action(
            "metadata:configuration",
            DurableReplicaAction::RecordElectionConfiguration {
                configuration: configuration.clone(),
            },
        )
        .await
        .unwrap();
    let mut conflicting = configuration.clone();
    conflicting.previous = Some(current);
    assert!(
        handle
            .execute_durable_action(
                "metadata:configuration",
                DurableReplicaAction::RecordElectionConfiguration {
                    configuration: conflicting,
                },
            )
            .await
            .is_err()
    );

    let status = wait_for_terminal(&handle).await;
    assert_eq!(status.election_configuration, Some(configuration));
    assert_eq!(status.deactivation_info.unwrap().epoch, epoch);
    assert_eq!(status.catch_up_capability, Some(0));
    assert_eq!(status.committed_lsn, 0);

    let replacement = KvPod::start(1).await;
    let replacement_handle = replacement.replica_handle(1).await;
    replacement_handle.open(OpenMode::Existing).await.unwrap();
    let replacement_status = replacement_handle.get_status().await.unwrap();
    assert_ne!(
        replacement_status.instance_id,
        ReplicaInstanceId::new(pod.instance_id.to_string())
    );
    assert!(replacement_status.election_configuration.is_none());
    assert!(replacement_status.deactivation_info.is_none());
}

#[tokio::test]
#[serial]
async fn direct_data_loss_api_remains_supported() {
    let epoch = Epoch::new(0, 1);
    let pod = KvPod::start(1).await;
    let handle = open_primary(&pod, epoch).await;
    assert_eq!(handle.on_data_loss().await.unwrap(), DataLossAction::None);
}

#[tokio::test]
#[serial]
async fn data_loss_epoch_mismatch_is_correlated_without_callback() {
    let runtime_epoch = Epoch::new(5, 1);
    let pod = KvPod::start_with_data_loss_behavior(1, DataLossBehavior::StateChanged).await;
    let handle = open_primary(&pod, runtime_epoch).await;
    handle
        .execute_durable_action(
            "failover:data-loss:mismatch",
            DurableReplicaAction::OnDataLoss {
                epoch: Epoch::new(5, 2),
            },
        )
        .await
        .unwrap_err();

    let status = handle.get_status().await.unwrap();
    let action = status.durable_action.unwrap();
    assert_eq!(action.state, DurableActionState::Failed);
    assert!(
        action
            .error
            .unwrap()
            .contains("does not match runtime epoch")
    );
    assert!(action.result.is_none());
    assert_eq!(pod.state.read().await.last_applied_lsn, 0);
}
