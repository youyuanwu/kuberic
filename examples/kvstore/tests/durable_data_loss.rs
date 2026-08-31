use std::time::Duration;

use kuberic_core::driver::ReplicaHandle;
use kuberic_core::types::{
    AgentControlVersion, CorrelatedActionObservation, CorrelatedControlActionAcknowledgement,
    CorrelatedControlActionRequest, DataLossAction, DurableActionResult, DurableActionState,
    DurableReplicaAction, Epoch, OpenMode, ReplicaConfigurationMemberStatus,
    ReplicaConfigurationMode, ReplicaConfigurationStatus, ReplicaElectionConfiguration,
    ReplicaInstanceId, Role,
};
use kvstore::service::DataLossBehavior;
use kvstore::testing::KvPod;
use serial_test::serial;

type GrpcHandle = kuberic_core::grpc::handle::GrpcReplicaHandle;

async fn execute(
    handle: &GrpcHandle,
    action_id: &str,
    action: DurableReplicaAction,
) -> kuberic_core::Result<CorrelatedControlActionAcknowledgement> {
    let status = handle.get_status().await?;
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
}

async fn open_primary(pod: &KvPod, epoch: Epoch) -> GrpcHandle {
    let handle = pod.replica_handle(1).await;
    execute(
        &handle,
        "test:open",
        DurableReplicaAction::Open {
            mode: OpenMode::New,
        },
    )
    .await
    .unwrap();
    execute(
        &handle,
        "test:primary",
        DurableReplicaAction::ChangeRole {
            epoch,
            role: Role::Primary,
        },
    )
    .await
    .unwrap();
    handle
}

fn retained<'a>(
    status: &'a kuberic_core::types::ReplicaStatusInfo,
    action_id: &str,
) -> Option<&'a CorrelatedActionObservation> {
    status
        .agent
        .retained_terminal_actions
        .iter()
        .rev()
        .find(|observation| observation.action.action_id == action_id)
}

async fn wait_for_terminal(
    handle: &GrpcHandle,
    action_id: &str,
) -> kuberic_core::types::ReplicaStatusInfo {
    for _ in 0..100 {
        let status = handle.get_status().await.unwrap();
        if retained(&status, action_id).is_some() {
            return status;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("correlated action did not reach a terminal state");
}

#[tokio::test]
#[serial]
async fn durable_data_loss_is_correlated_observable_and_replayable() {
    let epoch = Epoch::new(2, 7);
    let pod = KvPod::start_with_data_loss_behavior(1, DataLossBehavior::StateChanged).await;
    let handle = open_primary(&pod, epoch).await;
    pod.state.write().await.last_applied_lsn = 11;

    execute(
        &handle,
        "failover:data-loss",
        DurableReplicaAction::OnDataLoss { epoch },
    )
    .await
    .unwrap();
    let status = wait_for_terminal(&handle, "failover:data-loss").await;
    assert_eq!(
        status.agent.protocol_version,
        kuberic_core::replica_agent::CORRELATED_CONTROL_PROTOCOL_VERSION
    );
    let completion = retained(&status, "failover:data-loss").unwrap().clone();
    assert_eq!(
        completion.action.result,
        Some(DurableActionResult::DataLoss(DataLossAction::StateChanged))
    );
    assert_eq!(status.current_progress, 11);
    assert_eq!(status.committed_lsn, 11);
    assert_eq!(status.catch_up_capability, Some(11));
    assert_eq!(status.deactivation_info.unwrap().catch_up_lsn, 11);

    let replay = execute(
        &handle,
        "failover:data-loss",
        DurableReplicaAction::OnDataLoss { epoch },
    )
    .await
    .unwrap();
    assert_eq!(replay.observation, completion);
    assert!(
        execute(
            &handle,
            "failover:data-loss",
            DurableReplicaAction::OnDataLoss {
                epoch: Epoch::new(2, 8),
            },
        )
        .await
        .is_err()
    );

    execute(
        &handle,
        "test:demote",
        DurableReplicaAction::ChangeRole {
            epoch,
            role: Role::None,
        },
    )
    .await
    .unwrap();
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
    let acknowledgement = execute(
        &delayed_handle,
        "failover:data-loss:delayed",
        DurableReplicaAction::OnDataLoss { epoch },
    )
    .await
    .unwrap();
    assert_eq!(
        acknowledgement.observation.action.state,
        DurableActionState::InProgress
    );
    let status = delayed_handle.get_status().await.unwrap();
    assert_eq!(
        status.agent.current_action.unwrap().action.state,
        DurableActionState::InProgress
    );
    assert_eq!(
        retained(
            &wait_for_terminal(&delayed_handle, "failover:data-loss:delayed").await,
            "failover:data-loss:delayed"
        )
        .unwrap()
        .action
        .result,
        Some(DurableActionResult::DataLoss(DataLossAction::None))
    );

    let failed =
        KvPod::start_with_data_loss_behavior(1, DataLossBehavior::Fail("injected".into())).await;
    let failed_handle = open_primary(&failed, epoch).await;
    execute(
        &failed_handle,
        "failover:data-loss:failed",
        DurableReplicaAction::OnDataLoss { epoch },
    )
    .await
    .unwrap();
    let status = wait_for_terminal(&failed_handle, "failover:data-loss:failed").await;
    let action = &retained(&status, "failover:data-loss:failed")
        .unwrap()
        .action;
    assert_eq!(action.state, DurableActionState::Failed);
    assert!(action.error.as_deref().unwrap().contains("injected"));
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
    execute(
        &handle,
        "metadata:runtime-configuration",
        DurableReplicaAction::UpdateCurrentConfiguration {
            current: runtime_config,
        },
    )
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
    execute(
        &handle,
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
        execute(
            &handle,
            "metadata:configuration",
            DurableReplicaAction::RecordElectionConfiguration {
                configuration: conflicting,
            },
        )
        .await
        .is_err()
    );

    let status = wait_for_terminal(&handle, "metadata:configuration").await;
    assert_eq!(status.election_configuration, Some(configuration));
    assert_eq!(status.deactivation_info.unwrap().epoch, epoch);

    let replacement = KvPod::start(1).await;
    let replacement_handle = replacement.replica_handle(1).await;
    execute(
        &replacement_handle,
        "replacement:open",
        DurableReplicaAction::Open {
            mode: OpenMode::Existing,
        },
    )
    .await
    .unwrap();
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
async fn data_loss_epoch_mismatch_is_terminal_without_callback() {
    let runtime_epoch = Epoch::new(5, 1);
    let pod = KvPod::start_with_data_loss_behavior(1, DataLossBehavior::StateChanged).await;
    let handle = open_primary(&pod, runtime_epoch).await;
    let acknowledgement = execute(
        &handle,
        "failover:data-loss:mismatch",
        DurableReplicaAction::OnDataLoss {
            epoch: Epoch::new(5, 2),
        },
    )
    .await
    .unwrap();

    assert_eq!(
        acknowledgement.observation.action.state,
        DurableActionState::Failed
    );
    assert!(
        acknowledgement
            .observation
            .action
            .error
            .as_deref()
            .unwrap()
            .contains("does not match runtime epoch")
    );
    assert!(acknowledgement.observation.action.result.is_none());
    assert_eq!(pod.state.read().await.last_applied_lsn, 0);
}
