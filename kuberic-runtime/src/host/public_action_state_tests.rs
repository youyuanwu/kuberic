use super::*;

async fn current(fixture: &Fixture) -> PublicConfiguration {
    let local = fixture
        .store
        .load_state()
        .await
        .unwrap()
        .identity
        .local_identity;
    PublicConfiguration {
        current: ConfigurationDescriptor::new(
            Epoch::new(0, 2),
            local.replica_id,
            vec![ConfigurationMember {
                identity: local,
                role: ReplicaRole::Primary,
            }],
            1,
        ),
        previous: None,
        replicas: Vec::new(),
    }
}

#[tokio::test]
async fn accepted_epoch_fences_lower_epoch_even_with_newer_revision() {
    let fixture = Fixture::new().await;
    let applied = fixture
        .launch(
            PublicOperationProgram::Epoch {
                epoch: Epoch::new(0, 3),
            },
            1,
        )
        .await;
    assert_eq!(
        completed(&applied).await.disposition,
        Some(PublicOperationDisposition::Succeeded)
    );
    let stale = fixture.intent(
        PublicOperationProgram::Role {
            epoch: Epoch::new(0, 2),
            role: ReplicaRole::Primary,
        },
        2,
    );
    assert!(
        matches!(launch(&fixture.registry, fixture.store.clone(), stale, fixture.callbacks.clone()).await,
        Err(HostError::CommandRejected(message)) if message.contains("older than accepted"))
    );
    fixture.registry.shutdown().await.unwrap();
}

#[tokio::test]
async fn completed_current_rejects_joint_regression_but_allows_fresh_current_sync() {
    let fixture = Fixture::new().await;
    let configuration = current(&fixture).await;
    let installed = fixture
        .launch(
            PublicOperationProgram::Configuration(configuration.clone()),
            1,
        )
        .await;
    assert_eq!(
        completed(&installed).await.disposition,
        Some(PublicOperationDisposition::Succeeded)
    );
    let mut stale = configuration.clone();
    stale.previous = Some(configuration.current.clone());
    let intent = fixture.intent(PublicOperationProgram::Configuration(stale.clone()), 2);
    assert!(
        matches!(launch(&fixture.registry, fixture.store.clone(), intent, fixture.callbacks.clone()).await,
        Err(HostError::CommandRejected(message)) if message.contains("cannot regress"))
    );
    let stale_catchup = fixture.intent(
        PublicOperationProgram::CatchUp {
            configuration: stale,
            mode: PublicCatchUpMode::All,
        },
        2,
    );
    let events_before = fixture.probe.events.lock().unwrap().clone();
    assert!(
        matches!(launch(&fixture.registry, fixture.store.clone(), stale_catchup, fixture.callbacks.clone()).await,
        Err(HostError::CommandRejected(message)) if message.contains("cannot regress"))
    );
    assert_eq!(*fixture.probe.events.lock().unwrap(), events_before);
    assert_eq!(
        installed_configuration_for_state(
            &fixture.store.load_state().await.unwrap(),
            &ProcessSessionId::new("session")
        ),
        Some(configuration.clone())
    );
    let sync = fixture
        .launch(
            PublicOperationProgram::CatchUp {
                configuration,
                mode: PublicCatchUpMode::All,
            },
            3,
        )
        .await;
    assert_eq!(
        completed(&sync).await.disposition,
        Some(PublicOperationDisposition::Succeeded)
    );
    fixture.registry.shutdown().await.unwrap();
}

#[tokio::test]
async fn secondary_host_installation_does_not_claim_primary_configuration_stage() {
    use crate::protocol::public_operations::PublicReplicaDescriptor;
    let fixture = Fixture::new().await;
    let local = fixture
        .store
        .load_state()
        .await
        .unwrap()
        .identity
        .local_identity;
    let primary = ReplicaIdentity {
        replica_id: ReplicaId::new(2),
        instance_id: ReplicaInstanceId::new("other-primary"),
        agent_generation: AgentGeneration::new("other-agent"),
    };
    let mut configuration = PublicConfiguration {
        current: ConfigurationDescriptor::new(
            Epoch::new(0, 2),
            primary.replica_id,
            vec![
                ConfigurationMember {
                    identity: local,
                    role: ReplicaRole::ActiveSecondary,
                },
                ConfigurationMember {
                    identity: primary.clone(),
                    role: ReplicaRole::Primary,
                },
            ],
            2,
        ),
        previous: None,
        replicas: vec![PublicReplicaDescriptor {
            identity: primary.clone(),
            role: ReplicaRole::Primary,
            build_id: OperationId::default(),
            replication_address: "http://127.0.0.1:2".into(),
            process_session_id: ProcessSessionId::new("other-session"),
            storage_incarnation: Some(ReplicaStorageIncarnationContext {
                replica: primary,
                storage: ReplicaStorageIdentity::Persistent {
                    pod_uid: PodUid::new("other-pod"),
                    pvc_uid: PvcUid::new("other-pvc"),
                },
                storage_generation: StorageGeneration::new("other-storage"),
            }),
            current_progress: 0,
            catch_up_capability: 0,
            is_up: true,
            is_ready: true,
            must_catchup: false,
        }],
    };
    let first = fixture
        .launch(
            PublicOperationProgram::Configuration(configuration.clone()),
            1,
        )
        .await;
    assert_eq!(
        completed(&first).await.disposition,
        Some(PublicOperationDisposition::Succeeded)
    );
    configuration.previous = Some(configuration.current.clone());
    let joint = fixture
        .launch(
            PublicOperationProgram::Configuration(configuration.clone()),
            2,
        )
        .await;
    assert_eq!(
        completed(&joint).await.disposition,
        Some(PublicOperationDisposition::Succeeded)
    );
    assert!(
        fixture.probe.events.lock().unwrap().is_empty(),
        "secondary host installation must not fabricate a primary engine callback"
    );
    assert_eq!(
        installed_configuration_for_state(
            &fixture.store.load_state().await.unwrap(),
            &ProcessSessionId::new("session")
        ),
        Some(configuration)
    );
    fixture.registry.shutdown().await.unwrap();
}

#[tokio::test]
async fn admitted_recovery_preserves_exact_paired_roles_and_epoch_not_predecessor_authority() {
    use crate::protocol::public_operations::{
        DataLossAttempt, DataLossAuthorization, DataLossReplay, ProcessAttemptContext,
        ProviderProgressReset, PublicDataLossRecoveryInput, PublicDataLossRecoveryStage,
        PublicDataLossRecoveryStatus,
    };
    let fixture = Fixture::new().await;
    let mut state = fixture.store.load_state().await.unwrap();
    let local = state.identity.local_identity.clone();
    let preview = state.public_v1_operation_state.as_mut().unwrap();
    let predecessor = preview.history.clone().unwrap();
    let storage = preview.storage_incarnation.clone().unwrap();
    let authorization_id = OperationId::new("recovery-authority");
    let proposed = derive_data_loss_history(&authorization_id, &predecessor, 1, &storage).unwrap();
    let authorization = DataLossAuthorization {
        authorization_id: authorization_id.clone(),
        predecessor: predecessor.clone(),
        proposed: proposed.clone(),
        provisional_primary: storage.clone(),
        reset_scope: ProviderProgressReset::Preserve,
        replay_disposition: DataLossReplay::Convergent,
    };
    let epoch = Epoch::new(1, 3);
    let attempt = DataLossAttempt {
        authorization_id: authorization_id.clone(),
        attempt_id: OperationId::new("recovery-attempt"),
        process: ProcessAttemptContext {
            process_session_id: ProcessSessionId::new("session"),
            replicator_creation_id: ReplicatorCreationId::new("creation"),
            catch_up_specific_quorum: false,
            data_loss_replay: DataLossReplay::Convergent,
            epoch,
            attempt_revision: 1,
        },
    };
    let current = ConfigurationDescriptor::new(
        epoch,
        local.replica_id,
        vec![ConfigurationMember {
            identity: local.clone(),
            role: ReplicaRole::Primary,
        }],
        1,
    );
    let input = PublicLifecycleInput {
        recipe: PublicLifecycleRecipe::FailoverPromotion,
        replica: local.clone(),
        epoch,
        possible_data_loss: PossibleDataLossIntent::Possible,
        current: current.clone(),
        previous: Some(ConfigurationDescriptor::new(
            Epoch::new(0, 2),
            local.replica_id,
            current.members.clone(),
            1,
        )),
        replicas: Vec::new(),
        data_loss_recovery: Some(PublicDataLossRecoveryInput {
            authorization: authorization.clone(),
            attempt: attempt.clone(),
        }),
    };
    let mut intent = fixture.intent(PublicOperationProgram::Epoch { epoch }, 1);
    intent.operation_id = attempt.attempt_id.clone();
    intent.program = None;
    intent.input_digest = input.digest();
    intent.replay_disposition =
        crate::protocol::public_operations::PublicOperationReplayDisposition::DataLoss(
            DataLossReplay::Convergent,
        );
    intent.lifecycle = Some(input);
    intent.validate().unwrap();
    let outcomes = program::operation_instructions(&intent)
        .into_iter()
        .map(|instruction| match instruction {
            PublicInstruction::ApplicationPrimary => {
                PublicInstructionOutcome::ApplicationRole(None)
            }
            PublicInstruction::DataLoss => {
                PublicInstructionOutcome::DataLoss(DataLossOutcome::False)
            }
            PublicInstruction::Progress => PublicInstructionOutcome::Progress(0),
            _ => PublicInstructionOutcome::Done,
        })
        .collect();
    let record = PublicOperationRecord {
        ordinary: None,
        intent: intent.clone(),
        attempt: 1,
        attempt_results: Default::default(),
        retry_failure: None,
        accepted_history: Some(predecessor.clone()),
        accepted_storage: Some(storage.clone()),
        stage: PublicOperationStage::Completed,
        disposition: Some(PublicOperationDisposition::Succeeded),
        containment: Default::default(),
        superseded_by: None,
        blockers: Default::default(),
        lifecycle: crate::host::state::PublicLifecycleRecord {
            in_flight: None,
            outcomes,
        },
    };
    preview.operations.insert(intent.operation_id, record);
    preview.history = Some(proposed.clone());
    preview.data_loss_recovery = Some(PublicDataLossRecoveryStatus {
        authorization_id,
        attempt,
        predecessor_history: predecessor,
        proposed_history: proposed.clone(),
        provisional_primary: storage,
        reset_scope: ProviderProgressReset::Preserve,
        replay_disposition: DataLossReplay::Convergent,
        stage: PublicDataLossRecoveryStage::Admitted,
        observed_progress: None,
    });
    let installed =
        installed_configuration_for_state(&state, &ProcessSessionId::new("session")).unwrap();
    assert!(
        installed.previous.is_none(),
        "typed recovery selected CurrentConfiguration, not its previous input"
    );
    let mut joint = installed.clone();
    joint.previous = Some(installed.current.clone());
    for program in [
        PublicOperationProgram::Configuration(joint.clone()),
        PublicOperationProgram::CatchUp {
            configuration: joint,
            mode: PublicCatchUpMode::All,
        },
    ] {
        assert!(
            matches!(crate::host::public_action_state::validate(&state, &fixture.intent(program, 2)),
            Err(HostError::CommandRejected(detail)) if detail.contains("cannot regress"))
        );
    }
    let mut stale = fixture.intent(
        PublicOperationProgram::Epoch {
            epoch: Epoch::new(1, 2),
        },
        2,
    );
    assert!(
        matches!(crate::host::public_action_state::validate(&state, &stale),
        Err(HostError::CommandRejected(message)) if message.contains("older than accepted"))
    );
    let target = ReplicaIdentity {
        replica_id: ReplicaId::new(2),
        instance_id: ReplicaInstanceId::new("target"),
        agent_generation: AgentGeneration::new("target-agent"),
    };
    let build = PublicBuildInput {
        attempt: OperationId::new("post-recovery-build"),
        epoch,
        current_configuration_id: Some(current.configuration_id),
        replica: target.clone(),
        target_storage: ReplicaStorageIncarnationContext {
            replica: target,
            storage: ReplicaStorageIdentity::Persistent {
                pod_uid: PodUid::new("target"),
                pvc_uid: PvcUid::new("target-pvc"),
            },
            storage_generation: StorageGeneration::new("target-storage"),
        },
        process_session_id: ProcessSessionId::new("target-session"),
        source_address: "http://127.0.0.1:1".into(),
        replication_address: "http://127.0.0.1:2".into(),
        history: Some(proposed),
    };
    stale = fixture.intent(PublicOperationProgram::Build(build), 2);
    stale.class = crate::protocol::public_operations::PublicOperationClass::Build {
        target: ReplicaId::new(2),
    };
    crate::host::public_action_state::validate(&state, &stale).unwrap();
    for corruption in 0..3 {
        let mut invalid = state.clone();
        let preview = invalid.public_v1_operation_state.as_mut().unwrap();
        match corruption {
            0 => {
                preview.data_loss_recovery.as_mut().unwrap().stage =
                    PublicDataLossRecoveryStage::ConfigurationPending
            }
            1 => {
                preview
                    .data_loss_recovery
                    .as_mut()
                    .unwrap()
                    .attempt
                    .attempt_id = OperationId::new("wrong-attempt")
            }
            _ => {
                preview
                    .storage_incarnation
                    .as_mut()
                    .unwrap()
                    .storage_generation = StorageGeneration::new("new-storage")
            }
        }
        assert!(crate::host::public_action_state::validate(&invalid, &stale).is_err());
    }
    fixture.registry.shutdown().await.unwrap();
}

#[tokio::test]
async fn cancelled_catchup_cannot_reopen_at_same_epoch_with_another_operation() {
    let fixture = Fixture::new().await;
    fixture.probe.block_catchup.store(true, Ordering::Release);
    let configuration = current(&fixture).await;
    let operation = fixture
        .launch(
            PublicOperationProgram::CatchUp {
                configuration: configuration.clone(),
                mode: PublicCatchUpMode::All,
            },
            1,
        )
        .await;
    fixture.probe.catchup_entered.notified().await;
    operation.cancel_root().await.unwrap();
    let stale = fixture.intent(
        PublicOperationProgram::CatchUp {
            configuration: configuration.clone(),
            mode: PublicCatchUpMode::WriteQuorum,
        },
        2,
    );
    let state = fixture.store.load_state().await.unwrap();
    assert!(
        matches!(crate::host::public_action_state::validate(&state, &stale),
        Err(HostError::CommandRejected(message)) if message.contains("newer epoch"))
    );
    let mut newer = configuration;
    newer.current = ConfigurationDescriptor::new(
        Epoch::new(0, 3),
        newer.current.primary_id,
        newer.current.members,
        newer.current.write_quorum,
    );
    let fresh = fixture.intent(
        PublicOperationProgram::CatchUp {
            configuration: newer,
            mode: PublicCatchUpMode::All,
        },
        3,
    );
    crate::host::public_action_state::validate(&state, &fresh).unwrap();
    fixture.registry.shutdown().await.unwrap();
}

#[tokio::test]
async fn swap_starting_joint_stage_cannot_hide_behind_a_newer_refreshed_epoch() {
    let fixture = Fixture::new().await;
    let configuration = current(&fixture).await;
    let role = fixture
        .launch(
            PublicOperationProgram::Role {
                epoch: configuration.current.epoch,
                role: ReplicaRole::Primary,
            },
            1,
        )
        .await;
    completed(&role).await;
    let installed = fixture
        .launch(
            PublicOperationProgram::Configuration(configuration.clone()),
            2,
        )
        .await;
    assert_eq!(
        completed(&installed).await.disposition,
        Some(PublicOperationDisposition::Succeeded)
    );
    let mut starting = configuration.clone();
    starting.previous = Some(configuration.current.clone());
    let mut refreshed = configuration.clone();
    refreshed.current = ConfigurationDescriptor::new(
        Epoch::new(0, 3),
        refreshed.current.primary_id,
        refreshed.current.members,
        refreshed.current.write_quorum,
    );
    let intent = fixture.intent(
        PublicOperationProgram::Swap {
            starting,
            refreshed,
            epoch: Epoch::new(0, 3),
            handoff: ReplicaRole::ActiveSecondary,
            catch_up_specific_quorum: false,
            mode: PublicCatchUpMode::All,
            controller_composed: false,
        },
        3,
    );
    intent.validate().unwrap();
    let events = fixture.probe.events.lock().unwrap().clone();
    assert!(
        matches!(launch(&fixture.registry, fixture.store.clone(), intent, fixture.callbacks.clone()).await,
        Err(HostError::CommandRejected(message)) if message.contains("cannot regress"))
    );
    assert_eq!(*fixture.probe.events.lock().unwrap(), events);
    assert_eq!(
        installed_configuration_for_state(
            &fixture.store.load_state().await.unwrap(),
            &ProcessSessionId::new("session")
        ),
        Some(configuration)
    );
    fixture.registry.shutdown().await.unwrap();
}

#[tokio::test]
async fn promotion_selected_catchup_cancellation_fences_all_same_epoch_wait_entrypoints() {
    let mut fixture = Fixture::new().await;
    fixture.callbacks.native_observer_callbacks = true;
    fixture.probe.block_catchup.store(true, Ordering::Release);
    let configuration = current(&fixture).await;
    let input = PublicLifecycleInput {
        recipe: PublicLifecycleRecipe::FailoverPromotion,
        replica: fixture
            .store
            .load_state()
            .await
            .unwrap()
            .identity
            .local_identity,
        epoch: configuration.current.epoch,
        possible_data_loss: PossibleDataLossIntent::NotPossible,
        current: configuration.current.clone(),
        previous: Some(ConfigurationDescriptor::new(
            Epoch::new(0, 1),
            configuration.current.primary_id,
            configuration.current.members.clone(),
            1,
        )),
        replicas: Vec::new(),
        data_loss_recovery: None,
    };
    let mut intent = fixture.intent(PublicOperationProgram::Epoch { epoch: input.epoch }, 1);
    intent.program = None;
    intent.input_digest = input.digest();
    intent.lifecycle = Some(input.clone());
    let operation = launch(
        &fixture.registry,
        fixture.store.clone(),
        intent.clone(),
        fixture.callbacks.clone(),
    )
    .await
    .unwrap();
    fixture.probe.catchup_entered.notified().await;
    assert_eq!(
        fixture
            .store
            .load_state()
            .await
            .unwrap()
            .public_v1_operation_state
            .unwrap()
            .operations[&intent.operation_id]
            .lifecycle
            .in_flight,
        Some(PublicInstruction::CatchUp)
    );
    operation.cancel_root().await.unwrap();
    let state = fixture.store.load_state().await.unwrap();
    let stale = fixture.intent(
        PublicOperationProgram::CatchUp {
            configuration: configuration.clone(),
            mode: PublicCatchUpMode::All,
        },
        2,
    );
    assert!(
        matches!(crate::host::public_action_state::validate(&state, &stale),
        Err(HostError::CommandRejected(message)) if message.contains("newer epoch"))
    );
    let mut another_recipe = intent.clone();
    another_recipe.operation_id = OperationId::new("another-promotion");
    another_recipe.revision = 2;
    assert!(crate::host::public_action_state::validate(&state, &another_recipe).is_err());
    let mut newer = configuration;
    newer.current = ConfigurationDescriptor::new(
        Epoch::new(0, 3),
        newer.current.primary_id,
        newer.current.members,
        newer.current.write_quorum,
    );
    let fresh = fixture.intent(
        PublicOperationProgram::CatchUp {
            configuration: newer,
            mode: PublicCatchUpMode::All,
        },
        3,
    );
    crate::host::public_action_state::validate(&state, &fresh).unwrap();
    let mut pre_wait_cancel = state;
    pre_wait_cancel
        .public_v1_operation_state
        .as_mut()
        .unwrap()
        .operations
        .get_mut(&intent.operation_id)
        .unwrap()
        .lifecycle
        .in_flight = Some(PublicInstruction::ApplicationPrimary);
    crate::host::public_action_state::validate(&pre_wait_cancel, &stale).unwrap();
    fixture.registry.shutdown().await.unwrap();
}

#[tokio::test]
async fn build_requires_both_achieved_roles_not_merely_installed_configuration() {
    let fixture = Fixture::new().await;
    let configuration = current(&fixture).await;
    let installed = fixture
        .launch(
            PublicOperationProgram::Configuration(configuration.clone()),
            1,
        )
        .await;
    assert_eq!(
        completed(&installed).await.disposition,
        Some(PublicOperationDisposition::Succeeded)
    );
    let state = fixture.store.load_state().await.unwrap();
    let target = ReplicaIdentity {
        replica_id: ReplicaId::new(2),
        instance_id: ReplicaInstanceId::new("target"),
        agent_generation: AgentGeneration::new("target-agent"),
    };
    let build = PublicBuildInput {
        attempt: OperationId::new("unready-build"),
        epoch: configuration.current.epoch,
        current_configuration_id: Some(configuration.current.configuration_id),
        replica: target.clone(),
        target_storage: ReplicaStorageIncarnationContext {
            replica: target,
            storage: ReplicaStorageIdentity::Persistent {
                pod_uid: PodUid::new("target"),
                pvc_uid: PvcUid::new("target-pvc"),
            },
            storage_generation: StorageGeneration::new("target-storage"),
        },
        process_session_id: ProcessSessionId::new("target-session"),
        source_address: "http://127.0.0.1:1".into(),
        replication_address: "http://127.0.0.1:2".into(),
        history: state.public_v1_operation_state.unwrap().history,
    };
    let mut intent = fixture.intent(PublicOperationProgram::Build(build), 2);
    intent.class = crate::protocol::public_operations::PublicOperationClass::Build {
        target: ReplicaId::new(2),
    };
    assert!(
        launch(
            &fixture.registry,
            fixture.store.clone(),
            intent.clone(),
            fixture.callbacks.clone()
        )
        .await
        .is_err()
    );
    fixture
        .probe
        .fail_application_role
        .store(true, Ordering::Release);
    let partial = fixture
        .launch(
            PublicOperationProgram::Role {
                epoch: Epoch::new(0, 2),
                role: ReplicaRole::Primary,
            },
            2,
        )
        .await;
    assert!(matches!(
        completed(&partial).await.disposition,
        Some(PublicOperationDisposition::Failed(_))
    ));
    // Even a successful replicator child does not supply the missing application role.
    assert!(
        crate::host::public_action_state::validate(
            &fixture.store.load_state().await.unwrap(),
            &intent
        )
        .is_err()
    );
    fixture.registry.shutdown().await.unwrap();
}
