use super::*;
use kuberic_runtime::testing::{PublicCutPosition, PublicOperationCut};

/// Real adapter/query evidence; controller dispatch has separate positive
/// journeys. This fixture deliberately holds the host's native/receipt seam.
#[tokio::test]
async fn builtin_native_success_before_receipt_never_labels_new_idle_state_as_old_configuration() {
    if m1_legacy_production_unavailable().await {
        return;
    }
    tokio::time::timeout(Duration::from_secs(60), async {
        let root = tempfile::tempdir().unwrap();
        let epoch = Epoch::new(0, 1);
        let mut source = Node::<KvService>::start(&root.path().join("source"), 1, 0).await;
        let mut target = Node::<KvService>::start(&root.path().join("target"), 2, 0).await;
        source.open(ReplicaRole::Primary, epoch).await;
        let single = source.single(epoch).await;
        target.open(ReplicaRole::IdleSecondary, epoch).await;
        source.build_idle(&mut target, epoch, &single).await;
        let old = source.report().await;
        assert_eq!(old.configuration.as_ref().unwrap().voting.current, single);
        assert_eq!(
            old.engine_idle.unwrap().idle_replicas,
            vec![target.identity.replica_id]
        );
        target
            .command(PublicOperationProgram::Role {
                epoch,
                role: ReplicaRole::ActiveSecondary,
            })
            .await;
        target
            .command(PublicOperationProgram::Epoch { epoch })
            .await;
        let full = ConfigurationDescriptor::new(
            epoch,
            source.identity.replica_id,
            vec![
                ConfigurationMember {
                    identity: source.identity.clone(),
                    role: ReplicaRole::Primary,
                },
                ConfigurationMember {
                    identity: target.identity.clone(),
                    role: ReplicaRole::ActiveSecondary,
                },
            ],
            2,
        );
        target
            .command(PublicOperationProgram::Configuration(PublicConfiguration {
                current: full.clone(),
                previous: None,
                replicas: vec![source.descriptor(ReplicaRole::Primary).await],
            }))
            .await;
        let configuration = PublicConfiguration {
            current: full,
            previous: None,
            replicas: vec![target.descriptor(ReplicaRole::ActiveSecondary).await],
        };
        let cut = Arc::new(PublicOperationCut {
            position: PublicCutPosition::CallbackSuccess,
            index: 0,
            entered: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
        });
        source
            .running
            .handle()
            .set_public_operation_cut(Some(cut.clone()));
        source.revision += 1;
        let program = PublicOperationProgram::Configuration(configuration.clone());
        let intent = PublicOperationIntent {
            identity: source.preview.clone(),
            operation_id: OperationId::new("native-query-before-receipt"),
            revision: source.revision,
            process_session_id: source.session.clone(),
            class: PublicOperationClass::Authority,
            input_digest: program.digest(),
            replay_disposition: program.replay_disposition(),
            lifecycle: None,
            program: Some(program),
        };
        let command = PublicV1OperationCommand {
            target: source.identity.clone(),
            intent: intent.clone(),
        };
        let address = source.control_address.clone();
        let request = proto::ExecuteCommandRequest {
            protocol_version: kuberic_runtime::protocol::PROTOCOL_VERSION,
            resource_uid: source.history.resource_uid.to_string(),
            target: Some(source.identity.clone().into()),
            expected_process_session_id: source.session.to_string(),
            command: Some(proto::execute_command_request::Command::PublicV1Operation(
                convert::public_v1_operation_command_to_proto(&command).unwrap(),
            )),
        };
        let execution = tokio::spawn(async move {
            let mut client = proto::agent_control_client::AgentControlClient::connect(address)
                .await
                .unwrap();
            client.execute(authorized(request)).await
        });
        cut.entered.notified().await;
        let during = source.report().await;
        assert_eq!(
            during.configuration.as_ref().unwrap().voting.current,
            single,
            "native callback success is not an achieved host installation receipt"
        );
        assert!(
            during.engine_idle.is_none(),
            "B's native empty idle set must never be labeled as a coherent A observation"
        );
        assert!(
            during
                .active_operations
                .iter()
                .any(|active| active == &intent)
        );
        cut.release.notify_one();
        execution.await.unwrap().unwrap();
        source.running.handle().set_public_operation_cut(None);
        let after = source.report().await;
        assert_eq!(after.configuration.as_ref().unwrap().voting, configuration);
        let observed = after
            .engine_idle
            .expect("completed installation permits a coherent native sample");
        assert_eq!(observed.configuration, configuration);
        assert!(observed.idle_replicas.is_empty());
        source.close().await;
        target.close().await;
    })
    .await
    .expect("bounded real builtin native-success/receipt observation seam");
}
