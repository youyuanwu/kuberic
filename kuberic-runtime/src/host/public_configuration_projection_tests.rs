use super::*;
use crate::protocol::public_operations::{
    ProgressQueryResult, PublicProgressRole, PublicReplicaDescriptor,
};

fn peer(identity: ReplicaIdentity, role: ReplicaRole) -> PublicReplicaDescriptor {
    PublicReplicaDescriptor {
        storage_incarnation: Some(ReplicaStorageIncarnationContext {
            replica: identity.clone(),
            storage: ReplicaStorageIdentity::Persistent {
                pod_uid: PodUid::new("peer-pod"),
                pvc_uid: PvcUid::new("peer-pvc"),
            },
            storage_generation: StorageGeneration::new("peer-storage"),
        }),
        identity,
        role,
        replication_address: "http://127.0.0.1:2".into(),
        process_session_id: ProcessSessionId::new("peer-session"),
        current_progress: 0,
        catch_up_capability: 0,
        is_up: true,
        is_ready: true,
        must_catchup: false,
    }
}

fn recipe(fixture: &Fixture, input: PublicLifecycleInput, revision: u64) -> PublicOperationIntent {
    let mut intent = fixture.intent(
        PublicOperationProgram::Epoch { epoch: input.epoch },
        revision,
    );
    intent.program = None;
    intent.input_digest = input.digest();
    intent.lifecycle = Some(input);
    intent
}

async fn reported_install(fixture: &Fixture) -> PublicConfiguration {
    let state = fixture.store.load_state().await.unwrap();
    let report = public_v1_report(
        fixture.store.as_ref(),
        &PublicV1Identity::new(6),
        PublicV1ReportProjection {
            catch_up_specific_quorum: None,
            process_session_id: ProcessSessionId::new("session"),
            replicator_creation_id: ReplicatorCreationId::new("creation"),
            report_sequence: 1,
            role: ReplicaRole::Primary,
            read_status: AccessStatus::ReconfigurationPending,
            write_status: AccessStatus::ReconfigurationPending,
            progress: PublicProgress {
                role: PublicProgressRole::Primary,
                current_progress: ProgressQueryResult::Value(0),
                catch_up_capability: ProgressQueryResult::Value(0),
            },
            progress_history: state.public_v1_operation_state.unwrap().history,
            engine_idle: None,
            observation_alive: None,
        },
    )
    .await
    .unwrap();
    report.configuration.unwrap().voting
}

#[tokio::test]
async fn epoch_only_recipe_cannot_publish_its_requested_configuration_as_installed() {
    let fixture = Fixture::new().await;
    let local = fixture
        .store
        .load_state()
        .await
        .unwrap()
        .identity
        .local_identity;
    let a = PublicConfiguration {
        current: ConfigurationDescriptor::new(
            Epoch::new(0, 2),
            local.replica_id,
            vec![ConfigurationMember {
                identity: local.clone(),
                role: ReplicaRole::Primary,
            }],
            1,
        ),
        previous: None,
        replicas: Vec::new(),
    };
    let installed = fixture
        .launch(PublicOperationProgram::Configuration(a.clone()), 1)
        .await;
    assert_eq!(
        completed(&installed).await.disposition,
        Some(PublicOperationDisposition::Succeeded)
    );
    let other = ReplicaIdentity {
        replica_id: ReplicaId::new(2),
        instance_id: ReplicaInstanceId::new("peer"),
        agent_generation: AgentGeneration::new("peer-agent"),
    };
    let b = PublicLifecycleInput {
        recipe: PublicLifecycleRecipe::SecondaryEpochAdvance,
        replica: local.clone(),
        epoch: Epoch::new(0, 3),
        possible_data_loss: PossibleDataLossIntent::NotPossible,
        current: ConfigurationDescriptor::new(
            Epoch::new(0, 3),
            other.replica_id,
            vec![
                ConfigurationMember {
                    identity: local,
                    role: ReplicaRole::ActiveSecondary,
                },
                ConfigurationMember {
                    identity: other.clone(),
                    role: ReplicaRole::Primary,
                },
            ],
            2,
        ),
        previous: None,
        replicas: vec![peer(other, ReplicaRole::Primary)],
        data_loss_recovery: None,
    };
    let epoch = launch(
        &fixture.registry,
        fixture.store.clone(),
        recipe(&fixture, b, 2),
        fixture.callbacks.clone(),
    )
    .await
    .unwrap();
    assert_eq!(
        completed(&epoch).await.disposition,
        Some(PublicOperationDisposition::Succeeded)
    );
    assert_eq!(reported_install(&fixture).await, a);
    assert_eq!(
        fixture.probe.events.lock().unwrap().last().unwrap(),
        "epoch"
    );
    fixture.registry.shutdown().await.unwrap();
}

#[tokio::test]
async fn current_only_receipt_normalizes_previous_only_peers_and_blocks_joint_regression() {
    let fixture = Fixture::new().await;
    let local = fixture
        .store
        .load_state()
        .await
        .unwrap()
        .identity
        .local_identity;
    let other = ReplicaIdentity {
        replica_id: ReplicaId::new(2),
        instance_id: ReplicaInstanceId::new("peer"),
        agent_generation: AgentGeneration::new("peer-agent"),
    };
    let input = PublicLifecycleInput {
        recipe: PublicLifecycleRecipe::InitialPrimary,
        replica: local.clone(),
        epoch: Epoch::new(0, 2),
        possible_data_loss: PossibleDataLossIntent::NotPossible,
        current: ConfigurationDescriptor::new(
            Epoch::new(0, 2),
            local.replica_id,
            vec![ConfigurationMember {
                identity: local.clone(),
                role: ReplicaRole::Primary,
            }],
            1,
        ),
        previous: Some(ConfigurationDescriptor::new(
            Epoch::new(0, 1),
            local.replica_id,
            vec![
                ConfigurationMember {
                    identity: local,
                    role: ReplicaRole::Primary,
                },
                ConfigurationMember {
                    identity: other.clone(),
                    role: ReplicaRole::ActiveSecondary,
                },
            ],
            2,
        )),
        replicas: vec![peer(other, ReplicaRole::ActiveSecondary)],
        data_loss_recovery: None,
    };
    let intent = recipe(&fixture, input.clone(), 1);
    intent.validate().unwrap();
    let initial = launch(
        &fixture.registry,
        fixture.store.clone(),
        intent,
        fixture.callbacks.clone(),
    )
    .await
    .unwrap();
    assert_eq!(
        completed(&initial).await.disposition,
        Some(PublicOperationDisposition::Succeeded)
    );
    assert_eq!(
        reported_install(&fixture).await,
        PublicConfiguration {
            current: input.current.clone(),
            previous: None,
            replicas: Vec::new(),
        }
    );
    let before = fixture.probe.events.lock().unwrap().clone();
    for program in [
        PublicOperationProgram::Configuration(input.configuration()),
        PublicOperationProgram::CatchUp {
            configuration: input.configuration(),
            mode: PublicCatchUpMode::All,
        },
    ] {
        assert!(
            matches!(launch(&fixture.registry, fixture.store.clone(), fixture.intent(program, 2),
            fixture.callbacks.clone()).await, Err(HostError::CommandRejected(detail)) if detail.contains("cannot regress"))
        );
    }
    assert_eq!(*fixture.probe.events.lock().unwrap(), before);
    fixture.registry.shutdown().await.unwrap();
}
