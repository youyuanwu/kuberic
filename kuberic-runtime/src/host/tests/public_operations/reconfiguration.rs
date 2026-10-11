use super::*;
use crate::host::operation::PartitionOperation;
use crate::host::public_lifecycle::{
    self, PublicCutPosition, PublicLifecycleCallbacks, PublicOperationCut,
};
use crate::host::state::{PublicCloseChild, PublicInstruction, PublicInstructionOutcome};
use crate::protocol::public_operations::{
    PossibleDataLossIntent, PublicBuildInput, PublicCatchUpMode, PublicConfiguration,
    PublicLifecycleInput, PublicLifecycleRecipe, PublicOperationProgram as Program,
};

struct Fixture {
    _directory: tempfile::TempDir,
    path: std::path::PathBuf,
    store: Arc<SqliteStore>,
    preview: PublicOperationPreviewIdentity,
    owner: PublicOperationPreviewRuntime,
    callbacks: PublicLifecycleCallbacks,
    trace: Arc<Trace>,
    replicator: Arc<TraceReplicator>,
    _host: Arc<PodRuntime>,
}

fn configuration(epoch: i64) -> PublicConfiguration {
    configuration_with_data_loss(0, epoch)
}

fn data_loss_configuration(epoch: i64) -> PublicConfiguration {
    configuration_with_data_loss(1, epoch)
}

fn configuration_with_data_loss(
    data_loss_number: i64,
    configuration_number: i64,
) -> PublicConfiguration {
    let current = ConfigurationDescriptor::new(
        Epoch::new(data_loss_number, configuration_number),
        ReplicaId::new(1),
        vec![ConfigurationMember {
            identity: replica_identity(),
            role: ReplicaRole::Primary,
        }],
        1,
    );
    PublicConfiguration {
        previous: Some(ConfigurationDescriptor::new(
            Epoch::new(data_loss_number, configuration_number - 1),
            ReplicaId::new(1),
            current.members.clone(),
            1,
        )),
        current,
        replicas: Vec::new(),
    }
}

fn build(attempt: &str) -> PublicBuildInput {
    let mut replica = replica_identity();
    replica.replica_id = ReplicaId::new(2);
    PublicBuildInput {
        attempt: OperationId::new(attempt),
        replica,
        process_session_id: ProcessSessionId::new("target-session"),
        replication_address: "trace://target".into(),
        history: None,
        authorization: None,
        build_authorization: None,
    }
}

fn primary_lifecycle() -> PublicLifecycleInput {
    let configuration = configuration(1);
    PublicLifecycleInput {
        recipe: PublicLifecycleRecipe::InitialPrimary,
        replica: replica_identity(),
        epoch: configuration.current.epoch,
        possible_data_loss: PossibleDataLossIntent::NotPossible,
        current: configuration.current,
        previous: configuration.previous,
        replicas: Vec::new(),
        data_loss_recovery: None,
    }
}

fn configuration_with_target(epoch: i64) -> PublicConfiguration {
    let mut target = replica_identity();
    target.replica_id = ReplicaId::new(2);
    let members = vec![
        ConfigurationMember {
            identity: replica_identity(),
            role: ReplicaRole::Primary,
        },
        ConfigurationMember {
            identity: target.clone(),
            role: ReplicaRole::ActiveSecondary,
        },
    ];
    let replica = crate::protocol::public_operations::PublicReplicaDescriptor {
        build_id: OperationId::default(),
        identity: target.clone(),
        replication_address: "trace://secondary".into(),
        process_session_id: ProcessSessionId::new("secondary-session"),
        role: ReplicaRole::ActiveSecondary,
        current_progress: 0,
        catch_up_capability: 0,
        must_catchup: false,
        peer_authorization: None,
        configuration_revision: None,
    };
    PublicConfiguration {
        previous: Some(ConfigurationDescriptor::new(
            Epoch::new(0, epoch - 1),
            ReplicaId::new(1),
            members.clone(),
            1,
        )),
        current: ConfigurationDescriptor::new(Epoch::new(0, epoch), ReplicaId::new(1), members, 1),
        replicas: vec![replica],
    }
}

fn swap() -> Program {
    Program::Swap {
        starting: configuration(10),
        refreshed: configuration(11),
        epoch: Epoch::new(0, 11),
        handoff: ReplicaRole::ActiveSecondary,
        catch_up_specific_quorum: false,
        mode: PublicCatchUpMode::All,
    }
}

fn specific_quorum_swap() -> Program {
    let mut starting = configuration_with_target(10);
    starting.replicas[0].must_catchup = true;
    let mut refreshed = configuration_with_target(11);
    refreshed.replicas[0].must_catchup = true;
    Program::Swap {
        starting,
        refreshed,
        epoch: Epoch::new(0, 11),
        handoff: ReplicaRole::ActiveSecondary,
        catch_up_specific_quorum: true,
        mode: PublicCatchUpMode::WriteQuorum,
    }
}

fn legacy_write_quorum_swap() -> Program {
    Program::Swap {
        starting: configuration(10),
        refreshed: configuration(11),
        epoch: Epoch::new(0, 11),
        handoff: ReplicaRole::ActiveSecondary,
        catch_up_specific_quorum: false,
        mode: PublicCatchUpMode::WriteQuorum,
    }
}

impl Fixture {
    async fn new() -> Self {
        Self::new_with_preview(PublicOperationPreviewIdentity::new(430)).await
    }

    async fn public_v1() -> Self {
        Self::new_with_preview(PublicOperationPreviewIdentity::public_v1(430)).await
    }

    async fn new_with_preview(preview: PublicOperationPreviewIdentity) -> Self {
        let (directory, path, store) = durable_preview_store(&preview);
        let (trace, application, replicator, _) = trace_fixture();
        application.convergent_open.store(true, Ordering::Release);
        let host = Arc::new(PodRuntime::new(
            replica_identity(),
            application.clone(),
            store.clone(),
        ));
        let callbacks = PublicLifecycleCallbacks {
            application,
            replicator: replicator.clone(),
            primary: replicator.clone(),
            capabilities: if preview.is_public_v1() {
                ReplicatorCapabilities {
                    catch_up_specific_quorum: true,
                    data_loss_replay:
                        crate::protocol::public_operations::DataLossReplay::ReplaceOnAmbiguity,
                }
            } else {
                ReplicatorCapabilities::conservative()
            },
            replicator_creation_id: crate::protocol::types::ReplicatorCreationId::new("creation-1"),
            open_context: Some(
                host.public_open_context(crate::application::OpenMode::New)
                    .await,
            ),
            containment: Some(replicator.containment.subscribe()),
            aborted: Arc::new(Mutex::new(false)),
            cut: None,
        };
        let owner = PublicOperationPreviewRuntime::start(
            store.clone(),
            preview.clone(),
            ProcessSessionId::new("session-1"),
        )
        .unwrap();
        let fixture = Self {
            _directory: directory,
            path,
            store,
            preview,
            owner,
            callbacks,
            trace,
            replicator,
            _host: host,
        };
        fixture
            .run(
                "open",
                1,
                Program::Open {
                    replica: replica_identity(),
                    existing: false,
                },
            )
            .await;
        let lifecycle = primary_lifecycle();
        let mut role = intent(
            &fixture.preview,
            "role",
            2,
            PublicOperationClass::Authority,
            "session-1",
        );
        role.input_digest = lifecycle.digest();
        role.lifecycle = Some(lifecycle);
        terminal(
            &public_lifecycle::launch(
                &fixture.owner.registry(),
                fixture.store.clone(),
                role,
                fixture.callbacks.clone(),
            )
            .await
            .unwrap(),
        )
        .await;
        fixture.trace.events.lock().unwrap().clear();
        fixture
    }

    fn intent(&self, id: &str, revision: u64, mut program: Program) -> PublicOperationIntent {
        if self.preview.is_public_v1()
            && let Program::Build(build) | Program::Remove(build) = &mut program
        {
            build.history = Some(public_v1_history(&self.preview).1);
        }
        let class = match &program {
            Program::Swap { .. } => PublicOperationClass::PlannedSwap,
            Program::Build(input) => PublicOperationClass::Build {
                target: input.replica.replica_id,
            },
            Program::Remove(input) => PublicOperationClass::Remove {
                target: input.replica.replica_id,
            },
            Program::Close => PublicOperationClass::Close,
            Program::Abort => PublicOperationClass::Abort,
            _ => PublicOperationClass::Authority,
        };
        let mut intent = intent(&self.preview, id, revision, class, "session-1");
        intent.input_digest = program.digest();
        if self.preview.is_public_v1() {
            intent.replay_disposition = program.replay_disposition();
        }
        intent.program = Some(program);
        intent
    }

    async fn launch(
        &self,
        intent: PublicOperationIntent,
        cut: Option<Arc<PublicOperationCut>>,
    ) -> Arc<PartitionOperation> {
        let mut callbacks = self.callbacks.clone();
        callbacks.cut = cut;
        public_lifecycle::launch(
            &self.owner.registry(),
            self.store.clone(),
            intent,
            callbacks,
        )
        .await
        .unwrap()
    }

    async fn run(
        &self,
        id: &str,
        revision: u64,
        program: Program,
    ) -> crate::host::state::PublicOperationRecord {
        let operation = self.launch(self.intent(id, revision, program), None).await;
        terminal(&operation).await
    }
}

async fn terminal(
    operation: &Arc<PartitionOperation>,
) -> crate::host::state::PublicOperationRecord {
    let record = tokio::time::timeout(Duration::from_secs(2), operation.wait_for_terminal())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(record.stage, PublicOperationStage::Completed, "{record:?}");
    assert_eq!(
        record.disposition,
        Some(PublicOperationDisposition::Succeeded),
        "{record:?}"
    );
    record
}

fn cut(position: PublicCutPosition, index: usize) -> Arc<PublicOperationCut> {
    Arc::new(PublicOperationCut {
        position,
        index,
        entered: Notify::new(),
        release: Notify::new(),
    })
}

async fn reached(cut: &PublicOperationCut) {
    tokio::time::timeout(Duration::from_secs(2), cut.entered.notified())
        .await
        .expect("cut was not reached");
}

#[tokio::test]
async fn public_v1_role_and_epoch_transitions_persist_current_authority() {
    let fixture = Fixture::public_v1().await;
    fixture
        .run(
            "authorization-role-transition",
            3,
            Program::Role {
                epoch: Epoch::new(0, 3),
                role: ReplicaRole::Primary,
            },
        )
        .await;
    let state = fixture.store.load_state().await.unwrap();
    assert_eq!(state.role, ReplicaRole::Primary);
    assert_eq!(state.highest_epoch, Epoch::new(0, 3));

    fixture
        .run(
            "authorization-epoch-transition",
            4,
            Program::Epoch {
                epoch: Epoch::new(0, 4),
            },
        )
        .await;
    let state = fixture.store.load_state().await.unwrap();
    assert_eq!(state.role, ReplicaRole::Primary);
    assert_eq!(state.highest_epoch, Epoch::new(0, 4));
}

#[tokio::test]
async fn swap_installs_exact_starting_configuration_before_both_captured_mode_waits() {
    for selected in [PublicCatchUpMode::All, PublicCatchUpMode::WriteQuorum] {
        let fixture = Fixture::new().await;
        *fixture.trace.installed.lock().unwrap() = Some(configuration(2));
        fixture.trace.events.lock().unwrap().clear();
        let program = if selected == PublicCatchUpMode::WriteQuorum {
            legacy_write_quorum_swap()
        } else {
            swap()
        };
        let operation_intent = fixture.intent("swap", 3, program);
        let revoke = cut(PublicCutPosition::BeforeInstruction, 2);
        let operation = fixture
            .launch(operation_intent.clone(), Some(revoke.clone()))
            .await;
        reached(&revoke).await;
        let state = fixture
            .store
            .load_state()
            .await
            .unwrap()
            .public_operation_preview
            .unwrap();
        assert!(!state.writes_revoked);
        assert_eq!(fixture.trace.waits.lock().unwrap().len(), 1);
        assert!(fixture.owner.lifecycle_report().await.unwrap().write_access);
        revoke.release.notify_one();
        let result = terminal(&operation).await;
        assert!(
            fixture
                .store
                .load_state()
                .await
                .unwrap()
                .public_operation_preview
                .unwrap()
                .writes_revoked
        );
        assert!(!fixture.owner.lifecycle_report().await.unwrap().write_access);
        assert_eq!(result.lifecycle.outcomes.len(), 8);
        let mode = if selected == PublicCatchUpMode::All {
            ReplicaSetQuorumMode::All
        } else {
            ReplicaSetQuorumMode::WriteQuorum
        };
        let expected_configurations = [configuration(10), configuration(11)];
        assert_eq!(
            *fixture.trace.waits.lock().unwrap(),
            vec![
                (expected_configurations[0].clone(), mode),
                (expected_configurations[1].clone(), mode),
            ]
        );
        assert_eq!(
            *fixture.trace.events.lock().unwrap(),
            vec![
                "primary.update_catch_up_configuration.begin",
                "primary.update_catch_up_configuration.end",
                "primary.wait_for_catch_up.begin",
                "primary.wait_for_catch_up.end",
                "replicator.update_epoch.begin",
                "replicator.update_epoch.end",
                "provider.update_epoch.begin",
                "provider.update_epoch.end",
                "primary.update_catch_up_configuration.begin",
                "primary.update_catch_up_configuration.end",
                "primary.wait_for_catch_up.begin",
                "primary.wait_for_catch_up.end",
                "replicator.change_role.begin",
                "replicator.change_role.end",
                "application.change_role.begin",
                "application.change_role.end",
            ]
        );
        fixture.launch(operation_intent, None).await;
        assert_eq!(fixture.trace.waits.lock().unwrap().len(), 2);
        fixture.owner.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn public_v1_swap_captures_specific_quorum_capability() {
    let fixture = Fixture::public_v1().await;
    let program = specific_quorum_swap();
    let admitted = fixture.intent("public-v1-specific-swap", 3, program);
    let operation = fixture.launch(admitted.clone(), None).await;
    terminal(&operation).await;
    assert_eq!(
        fixture
            .trace
            .waits
            .lock()
            .unwrap()
            .iter()
            .map(|(_, mode)| *mode)
            .collect::<Vec<_>>(),
        vec![
            ReplicaSetQuorumMode::WriteQuorum,
            ReplicaSetQuorumMode::WriteQuorum,
        ]
    );
    let successors = fixture
        .trace
        .waits
        .lock()
        .unwrap()
        .iter()
        .map(|(configuration, _)| configuration.successor().cloned())
        .collect::<Vec<_>>();
    assert!(successors.iter().all(Option::is_some));
    assert_eq!(successors[0], successors[1]);

    let unsupported = fixture.intent(
        "public-v1-unsupported-specific-swap",
        4,
        specific_quorum_swap(),
    );
    let mut callbacks = fixture.callbacks.clone();
    callbacks.capabilities.catch_up_specific_quorum = false;
    assert!(
        public_lifecycle::launch(
            &fixture.owner.registry(),
            fixture.store.clone(),
            unsupported,
            callbacks,
        )
        .await
        .is_err()
    );

    let mut changed = admitted;
    let program = changed.program.as_mut().unwrap();
    let Program::Swap {
        catch_up_specific_quorum,
        mode,
        ..
    } = program
    else {
        unreachable!()
    };
    *catch_up_specific_quorum = false;
    *mode = PublicCatchUpMode::All;
    changed.input_digest = program.digest();
    changed.replay_disposition = program.replay_disposition();
    assert!(
        public_lifecycle::launch(
            &fixture.owner.registry(),
            fixture.store.clone(),
            changed,
            fixture.callbacks.clone(),
        )
        .await
        .is_err()
    );

    let missing_history_program = Program::Build(build("missing-history-build"));
    let mut missing_history = intent(
        &fixture.preview,
        "missing-history-build",
        3,
        PublicOperationClass::Build {
            target: ReplicaId::new(2),
        },
        "session-1",
    );
    missing_history.input_digest = missing_history_program.digest();
    missing_history.replay_disposition = missing_history_program.replay_disposition();
    missing_history.program = Some(missing_history_program);
    assert!(
        fixture
            .owner
            .registry()
            .admit(missing_history)
            .await
            .is_err()
    );

    let mut stale_build = build("stale-history-build");
    let (_, predecessor) = public_v1_history(&fixture.preview);
    stale_build.history = Some(
        crate::protocol::types::derive_data_loss_history(
            &OperationId::new("stale-history"),
            &predecessor,
            1,
            &public_v1_storage_incarnation(),
        )
        .unwrap(),
    );
    let stale_program = Program::Build(stale_build);
    let mut stale_history = intent(
        &fixture.preview,
        "stale-history-build",
        3,
        PublicOperationClass::Build {
            target: ReplicaId::new(2),
        },
        "session-1",
    );
    stale_history.input_digest = stale_program.digest();
    stale_history.replay_disposition = stale_program.replay_disposition();
    stale_history.program = Some(stale_program);
    assert!(fixture.owner.registry().admit(stale_history).await.is_err());
    fixture.owner.shutdown().await.unwrap();
}

#[test]
fn protocol_10_rejects_public_v1_command_shapes() {
    use crate::protocol::public_operations::{
        AuthorizationAuditDigest, PublicBuildAuthorizationReference,
        PublicOperationReplayDisposition,
    };

    let preview = PublicOperationPreviewIdentity::new(431);
    let program = specific_quorum_swap();
    let mut specific = intent(
        &preview,
        "legacy-specific-quorum",
        3,
        PublicOperationClass::PlannedSwap,
        "session-1",
    );
    specific.input_digest = program.digest();
    specific.program = Some(program);
    assert!(specific.validate().is_err());

    let program = Program::Progress { capability: false };
    let mut replay = intent(
        &preview,
        "legacy-repeatable-read",
        4,
        PublicOperationClass::Authority,
        "session-1",
    );
    replay.input_digest = program.digest();
    replay.replay_disposition = PublicOperationReplayDisposition::RepeatableRead;
    replay.program = Some(program);
    assert!(replay.validate().is_err());

    let mut input = build("legacy-authorized-build");
    input.authorization = Some(PublicBuildAuthorizationReference {
        resource_uid: ResourceUid::new("resource-1"),
        epoch: Epoch::new(0, 3),
        build_id: input.attempt.clone(),
        source: replica_identity(),
        source_storage: PvcUid::new("pvc-1"),
        source_session: ProcessSessionId::new("session-1"),
        target: input.replica.clone(),
        target_storage: PvcUid::new("pvc-2"),
        target_session: input.process_session_id.clone(),
        audit_digest: AuthorizationAuditDigest("ab".repeat(32)),
    });
    let program = Program::Build(input);
    let mut authorized = intent(
        &preview,
        "legacy-authorized-build",
        5,
        PublicOperationClass::Build {
            target: ReplicaId::new(2),
        },
        "session-1",
    );
    authorized.input_digest = program.digest();
    authorized.program = Some(program);
    assert!(authorized.validate().is_err());
}

#[tokio::test]
async fn public_operation_replay_swap_install_cuts_never_wait_on_stale_topology() {
    for (position, index) in [
        (PublicCutPosition::BeforeInstruction, 0),
        (PublicCutPosition::CallbackSuccess, 0),
        (PublicCutPosition::InstructionApplied, 0),
        (PublicCutPosition::BeforeInstruction, 1),
        (PublicCutPosition::InstructionApplied, 4),
    ] {
        let fixture = Fixture::new().await;
        *fixture.trace.installed.lock().unwrap() = Some(configuration(2));
        let intent = fixture.intent("swap", 3, swap());
        let barrier = cut(position, index);
        let operation = fixture.launch(intent.clone(), Some(barrier.clone())).await;
        reached(&barrier).await;
        operation.crash_root().await;
        *fixture.trace.installed.lock().unwrap() = Some(configuration(3));
        fixture.owner.registry().recover_unowned().await.unwrap();
        let recovered = fixture.launch(intent, None).await;
        terminal(&recovered).await;
        assert_eq!(
            *fixture.trace.waits.lock().unwrap(),
            vec![
                (configuration(10), ReplicaSetQuorumMode::All),
                (configuration(11), ReplicaSetQuorumMode::All),
            ],
            "{position:?}:{index}"
        );
        assert!(
            fixture
                .store
                .load_state()
                .await
                .unwrap()
                .public_operation_preview
                .unwrap()
                .writes_revoked
        );
        fixture.owner.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn public_operation_replay_retains_applied_data_loss_error_as_failure() {
    let fixture = Fixture::new().await;
    let configuration = configuration(3);
    let lifecycle = PublicLifecycleInput {
        recipe: PublicLifecycleRecipe::InitialPrimary,
        replica: replica_identity(),
        epoch: configuration.current.epoch,
        possible_data_loss: PossibleDataLossIntent::Possible,
        current: configuration.current,
        previous: configuration.previous,
        replicas: Vec::new(),
        data_loss_recovery: None,
    };
    let mut operation_intent = intent(
        &fixture.preview,
        "data-loss-error",
        3,
        PublicOperationClass::Authority,
        "session-1",
    );
    operation_intent.input_digest = lifecycle.digest();
    operation_intent.lifecycle = Some(lifecycle);
    *fixture.trace.data_loss.lock().unwrap() = Some(Err("retained failure".into()));
    let barrier = cut(PublicCutPosition::InstructionApplied, 2);
    let mut callbacks = fixture.callbacks.clone();
    callbacks.cut = Some(barrier.clone());
    let operation = public_lifecycle::launch(
        &fixture.owner.registry(),
        fixture.store.clone(),
        operation_intent.clone(),
        callbacks,
    )
    .await
    .unwrap();
    reached(&barrier).await;
    operation.crash_root().await;
    let callbacks_before = fixture
        .trace
        .events
        .lock()
        .unwrap()
        .iter()
        .filter(|event| event.as_str() == "primary.on_data_loss.begin")
        .count();
    fixture.owner.registry().recover_unowned().await.unwrap();
    let replayed = public_lifecycle::launch(
        &fixture.owner.registry(),
        fixture.store.clone(),
        operation_intent,
        fixture.callbacks.clone(),
    )
    .await
    .unwrap();
    let failed = tokio::time::timeout(Duration::from_secs(2), replayed.wait_for_terminal())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        failed.stage,
        PublicOperationStage::ContainmentPending | PublicOperationStage::Completed
    ));
    assert!(matches!(
        failed.disposition,
        Some(PublicOperationDisposition::Failed(ref error))
            if error.contains("retained failure")
    ));
    assert_eq!(
        fixture
            .trace
            .events
            .lock()
            .unwrap()
            .iter()
            .filter(|event| event.as_str() == "primary.on_data_loss.begin")
            .count(),
        callbacks_before
    );
    fixture.owner.shutdown().await.unwrap();
}

#[tokio::test]
async fn public_v1_data_loss_replay_uses_attached_capability() {
    use crate::protocol::public_operations::{DataLossReplay, PublicOperationReplayDisposition};

    let fixture = Fixture::public_v1().await;
    let configuration = data_loss_configuration(3);
    let epoch = configuration.current.epoch;
    let lifecycle = PublicLifecycleInput {
        recipe: PublicLifecycleRecipe::InitialPrimary,
        replica: replica_identity(),
        epoch: configuration.current.epoch,
        possible_data_loss: PossibleDataLossIntent::Possible,
        current: configuration.current,
        previous: configuration.previous,
        replicas: Vec::new(),
        data_loss_recovery: Some(public_v1_data_loss_recovery(
            &fixture.preview,
            "convergent-data-loss",
            3,
            epoch,
            DataLossReplay::Convergent,
        )),
    };
    let mut operation_intent = intent(
        &fixture.preview,
        "convergent-data-loss",
        3,
        PublicOperationClass::Authority,
        "session-1",
    );
    operation_intent.input_digest = lifecycle.digest();
    operation_intent.replay_disposition =
        PublicOperationReplayDisposition::DataLoss(DataLossReplay::Convergent);
    operation_intent.lifecycle = Some(lifecycle);

    let mut wrong_creation = operation_intent.clone();
    let wrong_creation_lifecycle = wrong_creation.lifecycle.as_mut().unwrap();
    wrong_creation_lifecycle
        .data_loss_recovery
        .as_mut()
        .unwrap()
        .attempt
        .process
        .replicator_creation_id =
        crate::protocol::types::ReplicatorCreationId::new("other-creation");
    wrong_creation.input_digest = wrong_creation_lifecycle.digest();
    let mut wrong_callbacks = fixture.callbacks.clone();
    wrong_callbacks.capabilities.data_loss_replay = DataLossReplay::Convergent;
    assert!(
        public_lifecycle::launch(
            &fixture.owner.registry(),
            fixture.store.clone(),
            wrong_creation,
            wrong_callbacks,
        )
        .await
        .is_err()
    );

    let mut wrong_quorum = operation_intent.clone();
    let wrong_quorum_lifecycle = wrong_quorum.lifecycle.as_mut().unwrap();
    wrong_quorum_lifecycle
        .data_loss_recovery
        .as_mut()
        .unwrap()
        .attempt
        .process
        .catch_up_specific_quorum = false;
    wrong_quorum.input_digest = wrong_quorum_lifecycle.digest();
    let mut wrong_callbacks = fixture.callbacks.clone();
    wrong_callbacks.capabilities.data_loss_replay = DataLossReplay::Convergent;
    assert!(
        public_lifecycle::launch(
            &fixture.owner.registry(),
            fixture.store.clone(),
            wrong_quorum,
            wrong_callbacks,
        )
        .await
        .is_err()
    );
    assert_eq!(
        fixture
            .trace
            .events
            .lock()
            .unwrap()
            .iter()
            .filter(|event| event.as_str() == "primary.on_data_loss.begin")
            .count(),
        0
    );

    let barrier = cut(PublicCutPosition::CallbackSuccess, 3);
    let mut callbacks = fixture.callbacks.clone();
    callbacks.capabilities.data_loss_replay = DataLossReplay::Convergent;
    callbacks.cut = Some(barrier.clone());
    let operation = public_lifecycle::launch(
        &fixture.owner.registry(),
        fixture.store.clone(),
        operation_intent.clone(),
        callbacks.clone(),
    )
    .await
    .unwrap();
    reached(&barrier).await;
    operation.crash_root().await;
    let interrupted = fixture
        .store
        .public_operation_records()
        .await
        .unwrap()
        .into_iter()
        .find(|record| record.intent == operation_intent)
        .unwrap();
    assert_eq!(
        interrupted.lifecycle.in_flight,
        Some(PublicInstruction::DataLoss)
    );
    assert!(public_lifecycle::replayable(&interrupted));

    fixture.owner.registry().recover_unowned().await.unwrap();
    callbacks.cut = None;
    terminal(
        &public_lifecycle::launch(
            &fixture.owner.registry(),
            fixture.store.clone(),
            operation_intent,
            callbacks,
        )
        .await
        .unwrap(),
    )
    .await;
    assert_eq!(
        fixture
            .trace
            .events
            .lock()
            .unwrap()
            .iter()
            .filter(|event| event.as_str() == "primary.on_data_loss.begin")
            .count(),
        2
    );
    fixture.owner.shutdown().await.unwrap();
}

#[tokio::test]
async fn public_v1_ordinary_operations_cannot_advance_data_loss_number() {
    let fixture = Fixture::public_v1().await;
    let changed = data_loss_configuration(3);
    let lifecycle = PublicLifecycleInput {
        recipe: PublicLifecycleRecipe::InitialPrimary,
        replica: replica_identity(),
        epoch: changed.current.epoch,
        possible_data_loss: PossibleDataLossIntent::NotPossible,
        current: changed.current,
        previous: changed.previous,
        replicas: Vec::new(),
        data_loss_recovery: None,
    };
    let mut ordinary = intent(
        &fixture.preview,
        "ordinary-data-loss-change",
        3,
        PublicOperationClass::Authority,
        "session-1",
    );
    ordinary.input_digest = lifecycle.digest();
    ordinary.lifecycle = Some(lifecycle);
    assert!(
        fixture
            .owner
            .registry()
            .admit(ordinary.clone())
            .await
            .is_err()
    );
    assert!(
        fixture
            .store
            .begin_public_operation(&ordinary, &[], &[])
            .await
            .is_err()
    );

    let program = Program::Epoch {
        epoch: Epoch::new(1, 3),
    };
    let mut epoch = intent(
        &fixture.preview,
        "ordinary-program-data-loss-change",
        3,
        PublicOperationClass::Authority,
        "session-1",
    );
    epoch.input_digest = program.digest();
    epoch.program = Some(program);
    assert!(fixture.owner.registry().admit(epoch.clone()).await.is_err());
    assert!(
        fixture
            .store
            .begin_public_operation(&epoch, &[], &[])
            .await
            .is_err()
    );
    fixture.owner.shutdown().await.unwrap();
}

#[tokio::test]
async fn public_v1_data_loss_renewal_is_durable_and_capability_exact() {
    use crate::protocol::public_operations::{
        DataLossAttempt, DataLossRenewalDecision, DataLossReplay, ProcessAttemptContext,
        PublicDataLossRecoveryInput, PublicOperationReplayDisposition,
    };
    use crate::protocol::types::ReplicatorCreationId;

    async fn seed_ambiguous_attempt(
        replay: DataLossReplay,
    ) -> (
        tempfile::TempDir,
        Arc<SqliteStore>,
        PublicOperationPreviewIdentity,
        PublicDataLossRecoveryInput,
    ) {
        let preview = PublicOperationPreviewIdentity::public_v1(430);
        let (directory, _path, store) = durable_preview_store(&preview);
        let configuration = data_loss_configuration(3);
        let recovery = public_v1_data_loss_recovery(
            &preview,
            "attempt-1",
            3,
            configuration.current.epoch,
            replay,
        );
        let lifecycle = PublicLifecycleInput {
            recipe: PublicLifecycleRecipe::InitialPrimary,
            replica: replica_identity(),
            epoch: configuration.current.epoch,
            possible_data_loss: PossibleDataLossIntent::Possible,
            current: configuration.current,
            previous: configuration.previous,
            replicas: Vec::new(),
            data_loss_recovery: Some(recovery.clone()),
        };
        let mut first = intent(
            &preview,
            "attempt-1",
            3,
            PublicOperationClass::Authority,
            "session-1",
        );
        first.input_digest = lifecycle.digest();
        first.replay_disposition = PublicOperationReplayDisposition::DataLoss(replay);
        first.lifecycle = Some(lifecycle);
        store
            .begin_public_operation(&first, &[], &[])
            .await
            .unwrap();
        store
            .advance_public_operation(
                &first.operation_id,
                first.revision,
                &first.process_session_id,
                PublicOperationStage::Ready,
                PublicOperationStage::Running,
                None,
            )
            .await
            .unwrap();
        for (index, outcome) in [
            PublicInstructionOutcome::Done,
            PublicInstructionOutcome::Done,
            PublicInstructionOutcome::ApplicationRole(None),
        ]
        .into_iter()
        .enumerate()
        {
            let instruction = public_lifecycle::operation_instructions(&first)[index];
            store
                .public_instruction(&first, index, instruction, None)
                .await
                .unwrap();
            store
                .public_instruction(&first, index, instruction, Some(outcome))
                .await
                .unwrap();
        }
        store
            .public_instruction(&first, 3, PublicInstruction::DataLoss, None)
            .await
            .unwrap();
        store
            .advance_public_operation(
                &first.operation_id,
                first.revision,
                &first.process_session_id,
                PublicOperationStage::Running,
                PublicOperationStage::ContainmentPending,
                Some(PublicOperationDisposition::Ambiguous(
                    "predecessor process exited".into(),
                )),
            )
            .await
            .unwrap();
        (directory, store, preview, recovery)
    }

    fn successor(
        preview: &PublicOperationPreviewIdentity,
        recovery: &PublicDataLossRecoveryInput,
        replay: DataLossReplay,
        configuration_number: i64,
    ) -> PublicOperationIntent {
        let configuration = data_loss_configuration(configuration_number);
        let mut recovery = recovery.clone();
        recovery.attempt = DataLossAttempt {
            authorization_id: recovery.authorization.authorization_id.clone(),
            attempt_id: OperationId::new("attempt-2"),
            process: ProcessAttemptContext {
                process_session_id: ProcessSessionId::new("session-2"),
                replicator_creation_id: ReplicatorCreationId::new("creation-2"),
                catch_up_specific_quorum: true,
                data_loss_replay: replay,
                epoch: configuration.current.epoch,
                attempt_revision: 4,
            },
        };
        let lifecycle = PublicLifecycleInput {
            recipe: PublicLifecycleRecipe::InitialPrimary,
            replica: replica_identity(),
            epoch: configuration.current.epoch,
            possible_data_loss: PossibleDataLossIntent::Possible,
            current: configuration.current,
            previous: configuration.previous,
            replicas: Vec::new(),
            data_loss_recovery: Some(recovery.clone()),
        };
        let mut intent = intent(
            preview,
            "attempt-2",
            4,
            PublicOperationClass::Authority,
            "session-2",
        );
        intent.input_digest = lifecycle.digest();
        intent.replay_disposition =
            PublicOperationReplayDisposition::DataLoss(recovery.authorization.replay_disposition);
        intent.lifecycle = Some(lifecycle);
        intent
    }

    let (_directory, store, preview, recovery) =
        seed_ambiguous_attempt(DataLossReplay::Convergent).await;
    let registry = PartitionOperationRegistry::new(
        store.clone(),
        preview.clone(),
        ProcessSessionId::new("session-2"),
    )
    .unwrap();
    assert!(
        registry
            .admit(successor(
                &preview,
                &recovery,
                DataLossReplay::Convergent,
                3,
            ))
            .await
            .is_err()
    );
    assert!(
        registry
            .admit(successor(
                &preview,
                &recovery,
                DataLossReplay::Convergent,
                2,
            ))
            .await
            .is_err()
    );
    let mut changed_reset = successor(&preview, &recovery, DataLossReplay::Convergent, 4);
    let changed_reset_input = changed_reset.lifecycle.as_mut().unwrap();
    changed_reset_input
        .data_loss_recovery
        .as_mut()
        .unwrap()
        .authorization
        .reset_scope = crate::protocol::public_operations::ProviderProgressReset::Preserve;
    changed_reset.input_digest = changed_reset_input.digest();
    assert!(registry.admit(changed_reset).await.is_err());

    let mut changed_storage = successor(&preview, &recovery, DataLossReplay::Convergent, 4);
    let changed_storage_input = changed_storage.lifecycle.as_mut().unwrap();
    let changed_recovery = changed_storage_input.data_loss_recovery.as_mut().unwrap();
    changed_recovery
        .authorization
        .provisional_primary
        .storage_generation = crate::protocol::types::StorageGeneration::new("changed-storage");
    changed_recovery.authorization.proposed = crate::protocol::types::derive_data_loss_history(
        &changed_recovery.authorization.authorization_id,
        &changed_recovery.authorization.predecessor,
        changed_recovery.authorization.proposed.data_loss_number,
        &changed_recovery.authorization.provisional_primary,
    )
    .unwrap();
    changed_storage.input_digest = changed_storage_input.digest();
    assert!(registry.admit(changed_storage).await.is_err());

    let renewed = registry
        .admit(successor(
            &preview,
            &recovery,
            DataLossReplay::Convergent,
            4,
        ))
        .await
        .unwrap();
    assert_eq!(renewed.snapshot().stage, PublicOperationStage::Ready);
    let state = store
        .load_state()
        .await
        .unwrap()
        .public_operation_preview
        .unwrap();
    assert_eq!(
        state.data_loss_recovery.unwrap().attempt.attempt_id,
        OperationId::new("attempt-2")
    );
    assert!(
        state
            .history_barriers
            .contains_key(&OperationId::new("attempt-2"))
    );
    assert!(
        !state
            .history_barriers
            .contains_key(&OperationId::new("attempt-1"))
    );

    let (_directory, store, preview, recovery) =
        seed_ambiguous_attempt(DataLossReplay::Convergent).await;
    let registry = PartitionOperationRegistry::new(
        store.clone(),
        preview.clone(),
        ProcessSessionId::new("session-2"),
    )
    .unwrap();
    let (trace, application, replicator, _) = trace_fixture();
    let callbacks_before = trace
        .events
        .lock()
        .unwrap()
        .iter()
        .filter(|event| event.as_str() == "primary.on_data_loss.begin")
        .count();
    let retired = public_lifecycle::launch(
        &registry,
        store.clone(),
        successor(&preview, &recovery, DataLossReplay::ReplaceOnAmbiguity, 4),
        PublicLifecycleCallbacks {
            application,
            replicator: replicator.clone(),
            primary: replicator.clone(),
            capabilities: ReplicatorCapabilities {
                catch_up_specific_quorum: true,
                data_loss_replay: DataLossReplay::ReplaceOnAmbiguity,
            },
            replicator_creation_id: ReplicatorCreationId::new("creation-2"),
            open_context: None,
            containment: None,
            aborted: Arc::new(Mutex::new(false)),
            cut: None,
        },
    )
    .await
    .unwrap();
    assert_eq!(retired.snapshot().stage, PublicOperationStage::Completed);
    assert_eq!(
        trace
            .events
            .lock()
            .unwrap()
            .iter()
            .filter(|event| event.as_str() == "primary.on_data_loss.begin")
            .count(),
        callbacks_before
    );
    assert!(matches!(
        store
            .load_state()
            .await
            .unwrap()
            .public_operation_preview
            .unwrap()
            .data_loss_recovery,
        Some(
            crate::protocol::public_operations::PublicDataLossRecoveryStatus {
                stage: crate::protocol::public_operations::PublicDataLossRecoveryStage::Retired,
                ..
            }
        )
    ));
    assert_eq!(
        DataLossAttempt::renewal_decision(
            &recovery.authorization,
            &recovery.attempt,
            &retired
                .intent()
                .lifecycle
                .unwrap()
                .data_loss_recovery
                .unwrap()
                .attempt,
            true,
        ),
        DataLossRenewalDecision::RetireWithoutCallback
    );

    let (_directory, store, preview, recovery) =
        seed_ambiguous_attempt(DataLossReplay::ReplaceOnAmbiguity).await;
    let registry = PartitionOperationRegistry::new(
        store.clone(),
        preview.clone(),
        ProcessSessionId::new("session-2"),
    )
    .unwrap();
    let (trace, application, replicator, _) = trace_fixture();
    let callbacks_before = trace
        .events
        .lock()
        .unwrap()
        .iter()
        .filter(|event| event.as_str() == "primary.on_data_loss.begin")
        .count();
    let terminal = public_lifecycle::launch(
        &registry,
        store.clone(),
        successor(&preview, &recovery, DataLossReplay::Convergent, 4),
        PublicLifecycleCallbacks {
            application,
            replicator: replicator.clone(),
            primary: replicator.clone(),
            capabilities: ReplicatorCapabilities {
                catch_up_specific_quorum: true,
                data_loss_replay: DataLossReplay::Convergent,
            },
            replicator_creation_id: ReplicatorCreationId::new("creation-2"),
            open_context: None,
            containment: None,
            aborted: Arc::new(Mutex::new(false)),
            cut: None,
        },
    )
    .await
    .unwrap();
    assert_eq!(terminal.snapshot().stage, PublicOperationStage::Completed);
    assert_eq!(
        trace
            .events
            .lock()
            .unwrap()
            .iter()
            .filter(|event| event.as_str() == "primary.on_data_loss.begin")
            .count(),
        callbacks_before
    );
    assert!(matches!(
        store
            .load_state()
            .await
            .unwrap()
            .public_operation_preview
            .unwrap()
            .data_loss_recovery,
        Some(crate::protocol::public_operations::PublicDataLossRecoveryStatus {
            stage: crate::protocol::public_operations::PublicDataLossRecoveryStage::ReplacementRequired,
            ..
        })
    ));
}

#[tokio::test]
async fn public_v1_history_admission_gates_all_data_loss_outcomes() {
    use crate::protocol::public_operations::{
        DataLossReplay, PublicDataLossRecoveryStage, PublicOperationReplayDisposition,
        PublicProgress, PublicProgressRole,
    };

    for changed in [false, true] {
        let fixture = Fixture::public_v1().await;
        *fixture.trace.data_loss.lock().unwrap() = Some(Ok(changed));
        let configuration = data_loss_configuration(3);
        let epoch = configuration.current.epoch;
        let recovery = public_v1_data_loss_recovery(
            &fixture.preview,
            if changed {
                "data-loss-true"
            } else {
                "data-loss-false"
            },
            3,
            epoch,
            DataLossReplay::ReplaceOnAmbiguity,
        );
        let predecessor = recovery.authorization.predecessor.clone();
        let proposed = recovery.authorization.proposed.clone();
        let attempt_id = recovery.attempt.attempt_id.clone();
        let lifecycle = PublicLifecycleInput {
            recipe: PublicLifecycleRecipe::InitialPrimary,
            replica: replica_identity(),
            epoch,
            possible_data_loss: PossibleDataLossIntent::Possible,
            current: configuration.current,
            previous: configuration.previous,
            replicas: Vec::new(),
            data_loss_recovery: Some(recovery),
        };
        let mut operation_intent = intent(
            &fixture.preview,
            attempt_id.as_str(),
            3,
            PublicOperationClass::Authority,
            "session-1",
        );
        operation_intent.input_digest = lifecycle.digest();
        operation_intent.replay_disposition =
            PublicOperationReplayDisposition::DataLoss(DataLossReplay::ReplaceOnAmbiguity);
        operation_intent.lifecycle = Some(lifecycle);
        terminal(&fixture.launch(operation_intent, None).await).await;
        let preview = fixture
            .store
            .load_state()
            .await
            .unwrap()
            .public_operation_preview
            .unwrap();
        assert_eq!(preview.history, Some(proposed));
        assert!(preview.history_barriers.is_empty());
        assert!(!preview.writes_revoked);
        assert_eq!(preview.operations.len(), 1);
        assert!(preview.operations.contains_key(&attempt_id));
        assert!(matches!(
            preview.data_loss_recovery,
            Some(
                crate::protocol::public_operations::PublicDataLossRecoveryStatus {
                    stage: PublicDataLossRecoveryStage::Admitted,
                    observed_progress: Some(_),
                    ..
                }
            )
        ));
        let report = public_lifecycle::public_v1_report(
            fixture.store.as_ref(),
            &fixture.preview,
            public_lifecycle::PublicV1ReportProjection {
                process_session_id: ProcessSessionId::new("session-1"),
                report_sequence: 1,
                role: ReplicaRole::Primary,
                read_status: AccessStatus::Granted,
                write_status: AccessStatus::Granted,
                progress: PublicProgress {
                    role: PublicProgressRole::Primary,
                    first: 0,
                    last: 0,
                },
            },
        )
        .await
        .unwrap();
        assert!(report.completed_operations.iter().all(|completion| {
            completion.operation_id == attempt_id
                && completion.revision == 3
                && completion.replay_disposition
                    == PublicOperationReplayDisposition::DataLoss(
                        DataLossReplay::ReplaceOnAmbiguity,
                    )
        }));
        {
            let events = fixture.trace.events.lock().unwrap();
            let position = |name: &str| {
                events
                    .iter()
                    .position(|event| event == name)
                    .expect("typed recovery trace event")
            };
            assert!(
                position("replicator.change_role.begin")
                    < position("replicator.update_epoch.begin")
            );
            assert!(
                position("replicator.update_epoch.begin")
                    < position("application.change_role.begin")
            );
            assert!(
                position("application.change_role.begin") < position("primary.on_data_loss.begin")
            );
        }
        let wire = crate::control::public_v1_agent_status_to_proto(&report);
        assert_eq!(
            crate::control::normalize_public_v1_agent_status_report(wire.clone()).unwrap(),
            report
        );
        let mut crossed_history = wire;
        let history = crossed_history
            .public_v1
            .as_mut()
            .unwrap()
            .history
            .as_mut()
            .unwrap();
        history.history_id = predecessor.history_id.as_str().to_string();
        history.data_loss_number = predecessor.data_loss_number;
        assert!(crate::control::normalize_public_v1_agent_status_report(crossed_history).is_err());
        fixture.owner.shutdown().await.unwrap();
    }

    let fixture = Fixture::public_v1().await;
    let ambiguous_configuration = data_loss_configuration(3);
    let epoch = ambiguous_configuration.current.epoch;
    let recovery = public_v1_data_loss_recovery(
        &fixture.preview,
        "data-loss-ambiguous-v1",
        3,
        epoch,
        DataLossReplay::ReplaceOnAmbiguity,
    );
    let predecessor = recovery.authorization.predecessor.clone();
    let lifecycle = PublicLifecycleInput {
        recipe: PublicLifecycleRecipe::InitialPrimary,
        replica: replica_identity(),
        epoch,
        possible_data_loss: PossibleDataLossIntent::Possible,
        current: ambiguous_configuration.current,
        previous: ambiguous_configuration.previous,
        replicas: Vec::new(),
        data_loss_recovery: Some(recovery),
    };
    let mut operation_intent = intent(
        &fixture.preview,
        "data-loss-ambiguous-v1",
        3,
        PublicOperationClass::Authority,
        "session-1",
    );
    operation_intent.input_digest = lifecycle.digest();
    operation_intent.replay_disposition =
        PublicOperationReplayDisposition::DataLoss(DataLossReplay::ReplaceOnAmbiguity);
    operation_intent.lifecycle = Some(lifecycle);
    let barrier = cut(PublicCutPosition::CallbackSuccess, 3);
    let operation = fixture
        .launch(operation_intent, Some(barrier.clone()))
        .await;
    reached(&barrier).await;
    operation.crash_root().await;
    fixture.owner.registry().recover_unowned().await.unwrap();
    let pending = wait_for_stage(&operation, PublicOperationStage::ContainmentPending).await;
    assert!(matches!(
        pending.disposition,
        Some(PublicOperationDisposition::Ambiguous(_))
    ));
    let preview = fixture
        .store
        .load_state()
        .await
        .unwrap()
        .public_operation_preview
        .unwrap();
    assert_eq!(preview.history, Some(predecessor));
    assert!(preview.writes_revoked);
    assert_eq!(preview.operations.len(), 1);
    assert!(
        preview
            .operations
            .contains_key(&OperationId::new("data-loss-ambiguous-v1"))
    );
    assert!(matches!(
        preview.data_loss_recovery,
        Some(
            crate::protocol::public_operations::PublicDataLossRecoveryStatus {
                stage: PublicDataLossRecoveryStage::CallbackProcessing,
                ..
            }
        )
    ));
    fixture.owner.registry().recover_unowned().await.unwrap();
    wait_for_stage(&operation, PublicOperationStage::Completed).await;
    fixture.owner.shutdown().await.unwrap();

    let fixture = Fixture::public_v1().await;
    *fixture.trace.data_loss.lock().unwrap() = Some(Err("typed data-loss failure".into()));
    let configuration = data_loss_configuration(3);
    let epoch = configuration.current.epoch;
    let recovery = public_v1_data_loss_recovery(
        &fixture.preview,
        "data-loss-error-v1",
        3,
        epoch,
        DataLossReplay::ReplaceOnAmbiguity,
    );
    let predecessor = recovery.authorization.predecessor.clone();
    let lifecycle = PublicLifecycleInput {
        recipe: PublicLifecycleRecipe::InitialPrimary,
        replica: replica_identity(),
        epoch,
        possible_data_loss: PossibleDataLossIntent::Possible,
        current: configuration.current,
        previous: configuration.previous,
        replicas: Vec::new(),
        data_loss_recovery: Some(recovery),
    };
    let mut operation_intent = intent(
        &fixture.preview,
        "data-loss-error-v1",
        3,
        PublicOperationClass::Authority,
        "session-1",
    );
    operation_intent.input_digest = lifecycle.digest();
    operation_intent.replay_disposition =
        PublicOperationReplayDisposition::DataLoss(DataLossReplay::ReplaceOnAmbiguity);
    operation_intent.lifecycle = Some(lifecycle);
    let failed = fixture.launch(operation_intent, None).await;
    let record = tokio::time::timeout(Duration::from_secs(2), failed.wait_for_terminal())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        record.disposition,
        Some(PublicOperationDisposition::Failed(ref message))
            if message.contains("typed data-loss failure")
    ));
    let preview = fixture
        .store
        .load_state()
        .await
        .unwrap()
        .public_operation_preview
        .unwrap();
    assert_eq!(preview.history, Some(predecessor));
    assert!(!preview.history_barriers.is_empty());
    assert!(preview.writes_revoked);
    assert!(matches!(
        preview.data_loss_recovery,
        Some(
            crate::protocol::public_operations::PublicDataLossRecoveryStatus {
                stage: PublicDataLossRecoveryStage::Failed,
                ..
            }
        )
    ));
    fixture.owner.shutdown().await.unwrap();
}

#[tokio::test]
async fn public_v1_rebuild_owner_enriches_captured_local_failure() {
    use crate::protocol::public_operations::{
        DataLossReplay, LocalProviderRebuildRequired, ProviderReplicaIdentity,
        PublicOperationReplayDisposition, PublicProgress, PublicProgressRole,
        PublicV1OperationResult, ReplicaRebuildReason, ReplicaRebuildRequired,
    };

    let fixture = Fixture::public_v1().await;
    *fixture.trace.rebuild.lock().unwrap() = Some(ReplicaRebuildRequired::LocalProvider(
        LocalProviderRebuildRequired {
            reason: ReplicaRebuildReason::IncompatibleRetainedHistory,
            provider: ProviderReplicaIdentity {
                replica: replica_identity(),
            },
        },
    ));
    let configuration = data_loss_configuration(3);
    let epoch = configuration.current.epoch;
    let lifecycle = PublicLifecycleInput {
        recipe: PublicLifecycleRecipe::InitialPrimary,
        replica: replica_identity(),
        epoch: configuration.current.epoch,
        possible_data_loss: PossibleDataLossIntent::Possible,
        current: configuration.current,
        previous: configuration.previous,
        replicas: Vec::new(),
        data_loss_recovery: Some(public_v1_data_loss_recovery(
            &fixture.preview,
            "public-v1-local-rebuild",
            3,
            epoch,
            DataLossReplay::ReplaceOnAmbiguity,
        )),
    };
    let mut operation_intent = intent(
        &fixture.preview,
        "public-v1-local-rebuild",
        3,
        PublicOperationClass::Authority,
        "session-1",
    );
    operation_intent.input_digest = lifecycle.digest();
    operation_intent.replay_disposition =
        PublicOperationReplayDisposition::DataLoss(DataLossReplay::ReplaceOnAmbiguity);
    operation_intent.lifecycle = Some(lifecycle);
    let operation = fixture.launch(operation_intent.clone(), None).await;
    let record = tokio::time::timeout(Duration::from_secs(2), operation.wait_for_terminal())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(record.stage, PublicOperationStage::Completed);
    assert!(matches!(
        record.disposition,
        Some(PublicOperationDisposition::Failed(ref message))
            if message.contains("local provider requires replica rebuild")
    ));
    let canonical = record
        .lifecycle
        .outcomes
        .iter()
        .find_map(|outcome| match outcome {
            PublicInstructionOutcome::RebuildRequired(failure) => Some((**failure).clone()),
            _ => None,
        })
        .expect("typed local rebuild outcome");
    let crate::protocol::public_operations::CanonicalReplicaRebuildRequired::LocalProvider {
        operation: captured,
        local_replica,
        local_storage,
        ..
    } = &canonical
    else {
        panic!("local provider result changed variant");
    };
    assert_eq!(captured.operation_id, operation_intent.operation_id);
    assert_eq!(captured.revision, operation_intent.revision);
    assert_eq!(
        captured.detecting_session,
        operation_intent.process_session_id
    );
    assert_eq!(local_replica, &replica_identity());
    assert_eq!(local_storage, &PvcUid::new("pvc-1"));

    // A delayed or superseded completion is checked against the captured
    // operation record rather than whichever operation is current.
    assert!(
        fixture
            .store
            .public_instruction(
                &operation_intent,
                3,
                PublicInstruction::DataLoss,
                Some(PublicInstructionOutcome::RebuildRequired(Box::new(
                    canonical.clone(),
                ))),
            )
            .await
            .is_err()
    );

    let report = public_lifecycle::public_v1_report(
        fixture.store.as_ref(),
        &fixture.preview,
        public_lifecycle::PublicV1ReportProjection {
            process_session_id: ProcessSessionId::new("session-1"),
            report_sequence: 1,
            role: ReplicaRole::Primary,
            read_status: AccessStatus::NotPrimary,
            write_status: AccessStatus::NotPrimary,
            progress: PublicProgress {
                role: PublicProgressRole::Primary,
                first: 0,
                last: 0,
            },
        },
    )
    .await
    .unwrap();
    assert!(report.completed_operations.iter().any(|completion| {
        matches!(
            &completion.result,
            PublicV1OperationResult::RebuildRequired(failure)
                if failure.as_ref() == &canonical
        )
    }));
    let mut crossed_completion = report.clone();
    let rebuild = crossed_completion
        .completed_operations
        .iter_mut()
        .find_map(|completion| match &mut completion.result {
            PublicV1OperationResult::RebuildRequired(rebuild) => Some(rebuild),
            _ => None,
        })
        .expect("local rebuild completion");
    let crate::protocol::public_operations::CanonicalReplicaRebuildRequired::LocalProvider {
        operation,
        ..
    } = rebuild.as_mut()
    else {
        panic!("local rebuild evidence changed variant");
    };
    operation.operation_id = OperationId::new("crossed-operation");
    assert!(crossed_completion.validate().is_err());

    let mut crossed_reporter = report.clone();
    crossed_reporter.replica.replica_id = ReplicaId::new(9);
    assert!(crossed_reporter.validate().is_err());

    let wire = crate::control::public_v1_agent_status_to_proto(&report);
    assert_eq!(
        crate::control::normalize_public_v1_agent_status_report(wire.clone()).unwrap(),
        report
    );
    let mut crossed_history = wire.clone();
    crossed_history
        .public_v1
        .as_mut()
        .unwrap()
        .data_loss_recovery
        .as_mut()
        .unwrap()
        .predecessor_history
        .as_mut()
        .unwrap()
        .history_id = "crossed-history".into();
    assert!(crate::control::normalize_public_v1_agent_status_report(crossed_history).is_err());

    let mut crossed_storage = wire.clone();
    crossed_storage
        .public_v1
        .as_mut()
        .unwrap()
        .data_loss_recovery
        .as_mut()
        .unwrap()
        .provisional_primary
        .as_mut()
        .unwrap()
        .storage_generation = "crossed-storage".into();
    assert!(crate::control::normalize_public_v1_agent_status_report(crossed_storage).is_err());

    let mut crossed_session = wire;
    crossed_session
        .public_v1
        .as_mut()
        .unwrap()
        .data_loss_recovery
        .as_mut()
        .unwrap()
        .attempt
        .as_mut()
        .unwrap()
        .process_session_id = "crossed-session".into();
    assert!(crate::control::normalize_public_v1_agent_status_report(crossed_session).is_err());

    let wire = crate::control::public_v1_agent_status_to_proto(&report);
    let mut invalid_replay = wire.clone();
    invalid_replay
        .public_v1
        .as_mut()
        .unwrap()
        .data_loss_recovery
        .as_mut()
        .unwrap()
        .authorization_replay = crate::control::proto::PublicV1ReplayDisposition::Convergent as i32;
    assert!(crate::control::normalize_public_v1_agent_status_report(invalid_replay).is_err());

    let mut noncanonical_reset = wire;
    let recovery = noncanonical_reset
        .public_v1
        .as_mut()
        .unwrap()
        .data_loss_recovery
        .as_mut()
        .unwrap();
    recovery.reset_scope_kind =
        crate::control::proto::PublicV1ProviderProgressResetKind::Preserve as i32;
    recovery.retired_storage = recovery.provisional_primary.clone();
    assert!(crate::control::normalize_public_v1_agent_status_report(noncanonical_reset).is_err());
    fixture.owner.shutdown().await.unwrap();
}

#[tokio::test]
async fn public_v1_remote_rebuild_claims_are_exact_and_fail_closed() {
    use crate::protocol::public_operations::{
        AuthorizationAuditDigest, PublicBuildAuthorizationReference, PublicProgress,
        PublicProgressRole, PublicV1AgentReport, PublicV1OperationCompletion,
        PublicV1OperationResult, RemoteBuildTargetRebuildRequired, ReplicaRebuildReason,
        ReplicaRebuildRequired,
    };

    let preview = PublicOperationPreviewIdentity::public_v1(902);
    let (_directory, _path, store) = durable_preview_store(&preview);
    let mut target = replica_identity();
    target.replica_id = ReplicaId::new(2);
    let authorization = PublicBuildAuthorizationReference {
        resource_uid: ResourceUid::new("resource-1"),
        epoch: Epoch::new(0, 9),
        build_id: OperationId::new("public-v1-remote-build"),
        source: replica_identity(),
        source_storage: PvcUid::new("pvc-1"),
        source_session: ProcessSessionId::new("session-1"),
        target: target.clone(),
        target_storage: PvcUid::new("pvc-2"),
        target_session: ProcessSessionId::new("target-session"),
        audit_digest: AuthorizationAuditDigest("ab".repeat(32)),
    };
    let input = PublicBuildInput {
        attempt: authorization.build_id.clone(),
        replica: target,
        process_session_id: authorization.target_session.clone(),
        replication_address: "trace://target".into(),
        history: Some(public_v1_history(&preview).1),
        authorization: Some(authorization.clone()),
        build_authorization: None,
    };
    let program = Program::Build(input.clone());
    let mut operation = intent(
        &preview,
        authorization.build_id.as_str(),
        4,
        PublicOperationClass::Build {
            target: input.replica.replica_id,
        },
        "session-1",
    );
    operation.input_digest = program.digest();
    operation.replay_disposition = program.replay_disposition();
    operation.program = Some(program);
    let state = store.load_state().await.unwrap();
    let canonical = public_lifecycle::enrich_remote_rebuild(
        &state.identity,
        &operation,
        &input,
        ReplicaRebuildRequired::RemoteBuildTarget(Box::new(RemoteBuildTargetRebuildRequired {
            reason: ReplicaRebuildReason::RemoteCopyConflict,
            authorization: authorization.clone(),
        })),
    )
    .unwrap();

    let mut changed = authorization.clone();
    changed.source_session = ProcessSessionId::new("changed-source-session");
    assert!(
        public_lifecycle::enrich_remote_rebuild(
            &state.identity,
            &operation,
            &input,
            ReplicaRebuildRequired::RemoteBuildTarget(Box::new(RemoteBuildTargetRebuildRequired {
                reason: ReplicaRebuildReason::RemoteCopyConflict,
                authorization: changed,
            },)),
        )
        .is_err()
    );

    let reject_consistent_wrong_source =
        |changed_authorization: PublicBuildAuthorizationReference| {
            let mut changed_input = input.clone();
            changed_input.authorization = Some(changed_authorization.clone());
            assert!(
                public_lifecycle::enrich_remote_rebuild(
                    &state.identity,
                    &operation,
                    &changed_input,
                    ReplicaRebuildRequired::RemoteBuildTarget(Box::new(
                        RemoteBuildTargetRebuildRequired {
                            reason: ReplicaRebuildReason::RemoteCopyConflict,
                            authorization: changed_authorization,
                        },
                    )),
                )
                .is_err()
            );
        };
    let mut wrong_source = authorization.clone();
    wrong_source.source.replica_id = ReplicaId::new(9);
    reject_consistent_wrong_source(wrong_source);
    let mut wrong_storage = authorization.clone();
    wrong_storage.source_storage = PvcUid::new("wrong-source-pvc");
    reject_consistent_wrong_source(wrong_storage);
    let mut wrong_session = authorization.clone();
    wrong_session.source_session = ProcessSessionId::new("wrong-source-session");
    reject_consistent_wrong_source(wrong_session);

    let report = PublicV1AgentReport {
        identity: preview.clone(),
        resource_uid: ResourceUid::new("resource-1"),
        replica: replica_identity(),
        process_session_id: ProcessSessionId::new("session-1"),
        report_sequence: 1,
        role: ReplicaRole::Primary,
        read_status: AccessStatus::NotPrimary,
        write_status: AccessStatus::NotPrimary,
        history: public_v1_history(&preview).1,
        storage_incarnation: public_v1_storage_incarnation(),
        progress: PublicProgress {
            role: PublicProgressRole::Primary,
            first: 0,
            last: 0,
        },
        data_loss_recovery: None,
        completed_operations: vec![PublicV1OperationCompletion {
            operation_id: operation.operation_id,
            revision: operation.revision,
            input_digest: operation.input_digest,
            replay_disposition: operation.replay_disposition,
            result: PublicV1OperationResult::RebuildRequired(Box::new(canonical)),
        }],
        extensions: Default::default(),
    };
    let wire = crate::control::public_v1_agent_status_to_proto(&report);
    assert_eq!(
        crate::control::normalize_public_v1_agent_status_report(wire.clone()).unwrap(),
        report
    );

    let mut oversized_extension = wire.clone();
    oversized_extension
        .public_v1
        .as_mut()
        .unwrap()
        .history_recovery_extension_version = Some(u32::MAX);
    assert!(crate::control::normalize_public_v1_agent_status_report(oversized_extension).is_err());

    let mut ignored_rebuild = wire.clone();
    let completion = &mut ignored_rebuild
        .public_v1
        .as_mut()
        .unwrap()
        .completed_operations[0];
    completion.result =
        crate::control::proto::PublicV1OperationResultKind::PublicV1OperationResultFailed as i32;
    completion.detail = "failed".into();
    assert!(crate::control::normalize_public_v1_agent_status_report(ignored_rebuild).is_err());

    let mut invalid_operation = wire.clone();
    invalid_operation
        .public_v1
        .as_mut()
        .unwrap()
        .completed_operations[0]
        .rebuild_required
        .as_mut()
        .unwrap()
        .operation
        .as_mut()
        .unwrap()
        .revision = 0;
    assert!(crate::control::normalize_public_v1_agent_status_report(invalid_operation).is_err());

    let mut crossed_completion = wire.clone();
    crossed_completion
        .public_v1
        .as_mut()
        .unwrap()
        .completed_operations[0]
        .operation_id = "crossed-operation".into();
    assert!(crate::control::normalize_public_v1_agent_status_report(crossed_completion).is_err());

    let mut crossed_resource = wire.clone();
    crossed_resource.public_v1.as_mut().unwrap().resource_uid = "crossed-resource".into();
    assert!(crate::control::normalize_public_v1_agent_status_report(crossed_resource).is_err());

    let mut crossed_generation = wire.clone();
    crossed_generation.public_v1.as_mut().unwrap().generation += 1;
    assert!(crate::control::normalize_public_v1_agent_status_report(crossed_generation).is_err());

    let mut crossed_reporter = wire.clone();
    crossed_reporter
        .public_v1
        .as_mut()
        .unwrap()
        .identity
        .as_mut()
        .unwrap()
        .replica_id = 9;
    assert!(crate::control::normalize_public_v1_agent_status_report(crossed_reporter).is_err());

    let mut crossed_session = wire.clone();
    crossed_session
        .public_v1
        .as_mut()
        .unwrap()
        .process_session_id = "crossed-session".into();
    assert!(crate::control::normalize_public_v1_agent_status_report(crossed_session).is_err());

    let mut invalid_authorization = wire;
    invalid_authorization
        .public_v1
        .as_mut()
        .unwrap()
        .completed_operations[0]
        .rebuild_required
        .as_mut()
        .unwrap()
        .authorization
        .as_mut()
        .unwrap()
        .audit_digest_sha256 = "invalid".into();
    assert!(
        crate::control::normalize_public_v1_agent_status_report(invalid_authorization).is_err()
    );
}

#[tokio::test]
async fn exact_build_is_cancelled_and_drained_before_removal_and_successor() {
    let fixture = Fixture::new().await;
    fixture
        .replicator
        .cancel_descendant_with_root
        .store(true, Ordering::Release);
    fixture.trace.block("primary.build_replica");
    let original = fixture.intent("attempt-1", 2, Program::Build(build("attempt-1")));
    let operation = fixture.launch(original.clone(), None).await;
    fixture.trace.wait_for("provider.descendant.begin").await;
    fixture.trace.wait_for("primary.build_replica.begin").await;
    let removal = fixture
        .launch(
            fixture.intent("remove-1", 2, Program::Remove(build("attempt-1"))),
            None,
        )
        .await;
    terminal(&removal).await;
    let events = fixture.trace.events.lock().unwrap().clone();
    let position = |name: &str| events.iter().position(|event| event == name).unwrap();
    assert!(position("primary.build_replica.cancel") < position("primary.remove_replica.begin"));
    assert!(position("provider.descendant.end") < position("primary.remove_replica.begin"));
    assert_eq!(
        operation.snapshot().disposition,
        Some(PublicOperationDisposition::Cancelled)
    );
    fixture
        .trace
        .gates
        .lock()
        .unwrap()
        .remove("primary.build_replica");
    fixture
        .run("attempt-2", 2, Program::Build(build("attempt-2")))
        .await;
    assert!(
        fixture
            .store
            .public_instruction(
                &original,
                0,
                PublicInstruction::Build,
                Some(PublicInstructionOutcome::Done)
            )
            .await
            .is_err()
    );
    assert!(
        public_lifecycle::launch(
            &fixture.owner.registry(),
            fixture.store.clone(),
            fixture.intent("stale-remove", 2, Program::Remove(build("attempt-1"))),
            fixture.callbacks.clone()
        )
        .await
        .is_err()
    );
    assert_eq!(
        fixture
            .store
            .load_state()
            .await
            .unwrap()
            .public_operation_preview
            .unwrap()
            .active_builds[&ReplicaId::new(2)],
        build("attempt-2")
    );
    fixture.owner.shutdown().await.unwrap();
}

#[tokio::test]
async fn build_rejects_targets_present_in_lifecycle_or_swap_topology() {
    let fixture = Fixture::public_v1().await;
    let expanded = configuration_with_target(3);
    let lifecycle = PublicLifecycleInput {
        recipe: PublicLifecycleRecipe::InitialPrimary,
        replica: replica_identity(),
        epoch: expanded.current.epoch,
        possible_data_loss: PossibleDataLossIntent::NotPossible,
        current: expanded.current,
        previous: expanded.previous,
        replicas: expanded.replicas,
        data_loss_recovery: None,
    };
    let mut authority = intent(
        &fixture.preview,
        "expanded-authority",
        3,
        PublicOperationClass::Authority,
        "session-1",
    );
    authority.input_digest = lifecycle.digest();
    authority.lifecycle = Some(lifecycle);
    terminal(
        &public_lifecycle::launch(
            &fixture.owner.registry(),
            fixture.store.clone(),
            authority,
            fixture.callbacks.clone(),
        )
        .await
        .unwrap(),
    )
    .await;
    let lifecycle_build = fixture.intent(
        "lifecycle-build",
        3,
        Program::Build(build("lifecycle-build")),
    );
    assert!(
        fixture
            .store
            .begin_public_operation(&lifecycle_build, &[], &[])
            .await
            .is_err()
    );
    fixture.owner.shutdown().await.unwrap();

    for (name, program) in [
        (
            "standalone-configuration",
            Program::Configuration(configuration_with_target(3)),
        ),
        (
            "standalone-catch-up",
            Program::CatchUp {
                configuration: configuration_with_target(3),
                mode: PublicCatchUpMode::All,
            },
        ),
        (
            "swap-starting",
            Program::Swap {
                starting: configuration_with_target(3),
                refreshed: configuration(4),
                epoch: Epoch::new(0, 4),
                handoff: ReplicaRole::ActiveSecondary,
                catch_up_specific_quorum: false,
                mode: PublicCatchUpMode::All,
            },
        ),
        (
            "swap-refreshed",
            Program::Swap {
                starting: configuration(3),
                refreshed: configuration_with_target(4),
                epoch: Epoch::new(0, 4),
                handoff: ReplicaRole::ActiveSecondary,
                catch_up_specific_quorum: false,
                mode: PublicCatchUpMode::All,
            },
        ),
    ] {
        let fixture = Fixture::public_v1().await;
        fixture
            .owner
            .registry()
            .admit(fixture.intent(name, 3, program))
            .await
            .unwrap();
        let attempt = format!("{name}-build");
        let build_intent = fixture.intent(&attempt, 3, Program::Build(build(&attempt)));
        assert!(
            fixture
                .store
                .begin_public_operation(&build_intent, &[], &[])
                .await
                .is_err(),
            "{name}"
        );
        fixture.owner.shutdown().await.unwrap();
    }

    let fixture = Fixture::public_v1().await;
    fixture
        .run(
            "historical-configuration",
            3,
            Program::Configuration(configuration_with_target(3)),
        )
        .await;
    fixture
        .run(
            "current-configuration",
            4,
            Program::Configuration(configuration(4)),
        )
        .await;
    let historical_build = fixture.intent(
        "historical-target-build",
        5,
        Program::Build(build("historical-target-build")),
    );
    assert!(
        fixture
            .store
            .begin_public_operation(&historical_build, &[], &[])
            .await
            .is_ok(),
        "superseded historical topology must not poison a future build"
    );
    fixture.owner.shutdown().await.unwrap();

    let fixture = Fixture::public_v1().await;
    fixture
        .run(
            "topology-before-progress",
            3,
            Program::Configuration(configuration_with_target(3)),
        )
        .await;
    fixture
        .run(
            "topology-neutral-progress",
            4,
            Program::Progress { capability: false },
        )
        .await;
    let active_build = fixture.intent(
        "active-target-after-progress",
        5,
        Program::Build(build("active-target-after-progress")),
    );
    assert!(
        fixture
            .store
            .begin_public_operation(&active_build, &[], &[])
            .await
            .is_err(),
        "topology-neutral authority must not hide installed configuration"
    );
    fixture.owner.shutdown().await.unwrap();
}

#[tokio::test]
async fn descendant_witness_blocks_close_even_after_root_end() {
    let fixture = Fixture::new().await;
    let operation = fixture
        .owner
        .registry()
        .admit(intent(
            &fixture.preview,
            "descendant",
            3,
            PublicOperationClass::Authority,
            "session-1",
        ))
        .await
        .unwrap();
    let (contained, witness) = watch::channel(false);
    operation.track_containment(witness).await.unwrap();
    operation
        .spawn_root(CallbackContainment::RootTask, async {
            Ok::<_, Infallible>(())
        })
        .await
        .unwrap();
    wait_for_stage(&operation, PublicOperationStage::ContainmentPending).await;
    assert!(
        fixture
            .owner
            .registry()
            .complete_containment(&operation.intent().operation_id)
            .await
            .is_err()
    );
    let close = fixture
        .launch(fixture.intent("close", 4, Program::Close), None)
        .await;
    tokio::time::sleep(Duration::from_millis(60)).await;
    assert_eq!(
        close.snapshot().stage,
        PublicOperationStage::WaitingForContainment
    );
    assert!(
        !fixture
            .trace
            .events
            .lock()
            .unwrap()
            .contains(&"replicator.close.begin".into())
    );
    assert!(!fixture.owner.lifecycle_report().await.unwrap().write_access);
    contained.send_replace(true);
    terminal(&close).await;
    fixture.owner.shutdown().await.unwrap();
}

#[tokio::test]
async fn contained_close_failures_preserve_typed_diagnostics_and_order() {
    for failures in [
        vec![],
        vec!["replicator"],
        vec!["application"],
        vec!["replicator", "application"],
    ] {
        let fixture = Fixture::new().await;
        *fixture.trace.fail_close.lock().unwrap() = failures.clone();
        let record = fixture.run("close", 3, Program::Close).await;
        let expected_diagnostics = failures
            .iter()
            .map(|child| {
                if *child == "replicator" {
                    PublicCloseChild::Replicator
                } else {
                    PublicCloseChild::Application
                }
            })
            .collect::<Vec<_>>();
        assert_eq!(
            record
                .lifecycle
                .outcomes
                .iter()
                .filter_map(|outcome| match outcome {
                    PublicInstructionOutcome::CloseFailure { child, error } => {
                        assert!(error.contains("close failed"));
                        Some(*child)
                    }
                    _ => None,
                })
                .collect::<Vec<_>>(),
            expected_diagnostics
        );
        let mut expected = vec!["replicator.close.begin", "replicator.close.end"];
        if failures.contains(&"replicator") {
            expected.extend(["replicator.abort", "application.abort"]);
        }
        expected.extend(["application.close.begin", "application.close.end"]);
        if failures == vec!["application"] {
            expected.extend(["replicator.abort", "application.abort"]);
        }
        assert_eq!(*fixture.trace.events.lock().unwrap(), expected);
        let reopened =
            SqliteStore::open_preview_existing(&fixture.path, None, &fixture.preview).unwrap();
        assert_eq!(
            reopened
                .public_operation_records()
                .await
                .unwrap()
                .into_iter()
                .find(|r| r.intent.operation_id == record.intent.operation_id)
                .unwrap(),
            record
        );
        let report = fixture.owner.lifecycle_report().await.unwrap();
        assert!(!report.write_access && report.service_location.is_none());
        assert!(fixture.replicator.lifecycle_closed.load(Ordering::Acquire));
        fixture.owner.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn synchronous_repeated_abort_fences_before_return_and_is_ordered_once() {
    let fixture = Fixture::new().await;
    let intent = fixture.intent("abort", 3, Program::Abort);
    for _ in 0..3 {
        public_lifecycle::abort(
            &fixture.owner.registry(),
            fixture.store.clone(),
            intent.clone(),
            fixture.callbacks.clone(),
        )
        .await
        .unwrap();
        assert!(fixture.owner.registry().is_fenced());
        assert_eq!(
            *fixture.trace.events.lock().unwrap(),
            vec!["replicator.abort", "application.abort"]
        );
        assert!(fixture.replicator.lifecycle_closed.load(Ordering::Acquire));
    }
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Some(operation) = fixture
                .owner
                .registry()
                .operation(&intent.operation_id)
                .await
            {
                terminal(&operation).await;
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(
        fixture
            .owner
            .registry()
            .admit(fixture.intent(
                "late",
                4,
                Program::Epoch {
                    epoch: Epoch::new(0, 2)
                }
            ))
            .await
            .is_err()
    );
    fixture.owner.shutdown().await.unwrap();
}

#[tokio::test]
async fn abort_admission_precedes_fencing_and_retained_duplicates_do_not_callback() {
    let fixture = Fixture::new().await;
    let mut rejected_close = fixture.intent("rejected-close", 3, Program::Close);
    rejected_close.process_session_id = ProcessSessionId::new("predecessor-session");
    assert!(
        public_lifecycle::launch(
            &fixture.owner.registry(),
            fixture.store.clone(),
            rejected_close,
            fixture.callbacks.clone(),
        )
        .await
        .is_err()
    );
    assert!(!fixture.owner.registry().is_fenced());
    assert!(fixture.trace.events.lock().unwrap().is_empty());

    let mut rejected = fixture.intent("rejected-abort", 3, Program::Abort);
    rejected.process_session_id = ProcessSessionId::new("predecessor-session");
    assert!(
        public_lifecycle::abort(
            &fixture.owner.registry(),
            fixture.store.clone(),
            rejected,
            fixture.callbacks.clone(),
        )
        .await
        .is_err()
    );
    assert!(!fixture.owner.registry().is_fenced());
    assert!(fixture.trace.events.lock().unwrap().is_empty());

    let accepted = fixture.intent("accepted-abort", 3, Program::Abort);
    public_lifecycle::abort(
        &fixture.owner.registry(),
        fixture.store.clone(),
        accepted.clone(),
        fixture.callbacks.clone(),
    )
    .await
    .unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if let Some(operation) = fixture
                .owner
                .registry()
                .operation(&accepted.operation_id)
                .await
                && operation.snapshot().stage == PublicOperationStage::Completed
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    fixture.trace.events.lock().unwrap().clear();

    let reconstructed = PartitionOperationRegistry::new(
        fixture.store.clone(),
        fixture.preview.clone(),
        ProcessSessionId::new("session-1"),
    )
    .unwrap();
    public_lifecycle::abort(
        &reconstructed,
        fixture.store.clone(),
        accepted,
        fixture.callbacks.clone(),
    )
    .await
    .unwrap();
    tokio::task::yield_now().await;
    assert!(reconstructed.is_fenced());
    assert!(fixture.trace.events.lock().unwrap().is_empty());
    reconstructed.shutdown().await.unwrap();
    fixture.owner.shutdown().await.unwrap();
}

#[tokio::test]
async fn callback_cancellation_cross_product_has_exact_root_end_and_descendant_drain() {
    let cases = [
        (
            Program::Open {
                replica: replica_identity(),
                existing: false,
            },
            "application.open",
        ),
        (
            Program::Open {
                replica: replica_identity(),
                existing: false,
            },
            "replicator.open",
        ),
        (
            Program::Role {
                epoch: Epoch::new(0, 3),
                role: ReplicaRole::Primary,
            },
            "replicator.change_role",
        ),
        (
            Program::Role {
                epoch: Epoch::new(0, 3),
                role: ReplicaRole::Primary,
            },
            "application.change_role",
        ),
        (
            Program::Epoch {
                epoch: Epoch::new(0, 3),
            },
            "replicator.update_epoch",
        ),
        (
            Program::Epoch {
                epoch: Epoch::new(0, 3),
            },
            "provider.update_epoch",
        ),
        (
            Program::Configuration(configuration(3)),
            "primary.update_catch_up_configuration",
        ),
        (
            Program::CatchUp {
                configuration: configuration(3),
                mode: PublicCatchUpMode::All,
            },
            "primary.wait_for_catch_up",
        ),
        (
            Program::Progress { capability: false },
            "replicator.current_progress",
        ),
        (
            Program::Progress { capability: true },
            "replicator.catch_up_capability",
        ),
        (Program::Build(build("blocked")), "primary.build_replica"),
        (Program::Remove(build("attempt")), "primary.remove_replica"),
    ];
    for (program, callback) in cases {
        for terminal_kind in 0..3 {
            let fixture = Fixture::new().await;
            fixture
                .replicator
                .cancel_descendant_with_root
                .store(true, Ordering::Release);
            if matches!(&program, Program::Remove(_)) {
                fixture
                    .run("attempt", 2, Program::Build(build("attempt")))
                    .await;
                fixture.trace.events.lock().unwrap().clear();
            }
            let revision = if matches!(program, Program::Build(_) | Program::Remove(_)) {
                2
            } else {
                3
            };
            fixture.trace.block(callback);
            let blocked = fixture
                .launch(fixture.intent("blocked", revision, program.clone()), None)
                .await;
            fixture.trace.wait_for(&format!("{callback}.begin")).await;
            fixture.trace.gates.lock().unwrap().remove(callback);
            let successor = fixture.intent(
                "successor",
                4,
                match terminal_kind {
                    0 => Program::Progress {
                        capability: callback == "replicator.current_progress",
                    },
                    1 => Program::Close,
                    _ => Program::Abort,
                },
            );
            if terminal_kind == 2 {
                public_lifecycle::abort(
                    &fixture.owner.registry(),
                    fixture.store.clone(),
                    successor.clone(),
                    fixture.callbacks.clone(),
                )
                .await
                .unwrap();
            }
            let next = fixture.launch(successor, None).await;
            terminal(&next).await;
            let events = fixture.trace.events.lock().unwrap().clone();
            assert!(
                events.contains(&format!("{callback}.cancel")),
                "{callback}/{terminal_kind}: {events:?}"
            );
            assert!(
                !events.contains(&format!("{callback}.end")),
                "{callback}/{terminal_kind}"
            );
            assert_ne!(
                blocked.snapshot().disposition,
                Some(PublicOperationDisposition::Succeeded)
            );
            if matches!(program, Program::Build(_)) {
                assert!(
                    fixture
                        .replicator
                        .descendant_stopped
                        .load(Ordering::Acquire)
                );
            }
            fixture.owner.shutdown().await.unwrap();
        }
    }
}

fn replay_programs() -> Vec<(&'static str, Program)> {
    vec![
        (
            "open-replay",
            Program::Open {
                replica: replica_identity(),
                existing: false,
            },
        ),
        (
            "role-replay",
            Program::Role {
                epoch: Epoch::new(0, 3),
                role: ReplicaRole::Primary,
            },
        ),
        (
            "epoch-replay",
            Program::Epoch {
                epoch: Epoch::new(0, 3),
            },
        ),
        (
            "configuration-replay",
            Program::Configuration(configuration(3)),
        ),
        (
            "catch-up-replay",
            Program::CatchUp {
                configuration: configuration(3),
                mode: PublicCatchUpMode::All,
            },
        ),
        ("progress-replay", Program::Progress { capability: false }),
        ("swap-replay", swap()),
        ("remove-operation", Program::Remove(build("remove-attempt"))),
        ("close-replay", Program::Close),
        ("abort-replay", Program::Abort),
    ]
}

#[tokio::test]
async fn public_operation_replay_matrix_converges_each_repeatable_program_family() {
    for (name, program) in replay_programs() {
        for position in [
            PublicCutPosition::CallbackSuccess,
            PublicCutPosition::InstructionApplied,
            PublicCutPosition::CallbackApplied,
            PublicCutPosition::Completed,
        ] {
            let fixture = Fixture::new().await;
            if matches!(program, Program::Remove(_)) {
                fixture
                    .run("remove-attempt", 2, Program::Build(build("remove-attempt")))
                    .await;
                fixture.trace.events.lock().unwrap().clear();
            }
            let revision = if matches!(&program, Program::Remove(_)) {
                2
            } else {
                3
            };
            let intent = fixture.intent(name, revision, program.clone());
            let instruction = match (position, &program) {
                (
                    PublicCutPosition::CallbackSuccess | PublicCutPosition::InstructionApplied,
                    Program::CatchUp { .. } | Program::Close | Program::Abort,
                ) => 1,
                _ => 0,
            };
            let barrier = cut(position, instruction);
            let operation = fixture.launch(intent.clone(), Some(barrier.clone())).await;
            tokio::time::timeout(Duration::from_secs(2), barrier.entered.notified())
                .await
                .unwrap_or_else(|_| panic!("cut was not reached: {name}/{position:?}"));
            operation.crash_root().await;
            fixture.owner.registry().recover_unowned().await.unwrap();
            let recovered = fixture.launch(intent.clone(), None).await;
            let record = terminal(&recovered).await;
            assert_eq!(
                record.lifecycle.outcomes.len(),
                public_lifecycle::operation_instructions(&intent).len(),
                "{name}/{position:?}"
            );
            let events = fixture.trace.events.lock().unwrap().len();
            let duplicate = fixture.launch(intent, None).await;
            terminal(&duplicate).await;
            assert_eq!(
                fixture.trace.events.lock().unwrap().len(),
                events,
                "retained {name}/{position:?} replayed callbacks"
            );
            fixture.owner.shutdown().await.unwrap();
        }
    }
}

#[tokio::test]
async fn public_v1_replay_preserves_declared_disposition() {
    use crate::protocol::public_operations::{
        AuthorizationAuditDigest, PublicOperationReplayDisposition,
    };

    let preview = PublicOperationPreviewIdentity::public_v1(901);
    let (_directory, path, store) = durable_preview_store(&preview);
    let program = Program::Progress { capability: false };
    let mut exact = intent(
        &preview,
        "public-v1-progress-read",
        1,
        PublicOperationClass::Authority,
        "session-1",
    );
    exact.input_digest = program.digest();
    exact.replay_disposition = program.replay_disposition();
    exact.program = Some(program);
    assert_eq!(
        exact.replay_disposition,
        PublicOperationReplayDisposition::RepeatableRead
    );
    store
        .begin_public_operation(&exact, &[], &[])
        .await
        .unwrap();
    store
        .advance_public_operation(
            &exact.operation_id,
            exact.revision,
            &exact.process_session_id,
            PublicOperationStage::Ready,
            PublicOperationStage::Running,
            None,
        )
        .await
        .unwrap();
    store
        .public_instruction(&exact, 0, PublicInstruction::Progress, None)
        .await
        .unwrap();
    store
        .public_instruction(
            &exact,
            0,
            PublicInstruction::Progress,
            Some(PublicInstructionOutcome::Progress(0)),
        )
        .await
        .unwrap();
    store
        .advance_public_operation(
            &exact.operation_id,
            exact.revision,
            &exact.process_session_id,
            PublicOperationStage::Running,
            PublicOperationStage::CallbackApplied,
            Some(PublicOperationDisposition::Succeeded),
        )
        .await
        .unwrap();
    store
        .advance_public_operation(
            &exact.operation_id,
            exact.revision,
            &exact.process_session_id,
            PublicOperationStage::CallbackApplied,
            PublicOperationStage::Completed,
            None,
        )
        .await
        .unwrap();
    drop(store);

    let reopened = Arc::new(SqliteStore::open_preview_existing(&path, None, &preview).unwrap());
    assert!(matches!(
        reopened
            .begin_public_operation(&exact, &[], &[])
            .await
            .unwrap(),
        crate::host::store::BeginPublicOperation::Completed(_)
    ));
    assert_eq!(exact.input_digest.version(), 1);
    let audit_digest = AuthorizationAuditDigest(exact.input_digest.sha256().to_string());
    assert_eq!(audit_digest.0, exact.input_digest.sha256());
    assert_ne!(
        std::any::TypeId::of::<AuthorizationAuditDigest>(),
        std::any::TypeId::of::<crate::protocol::public_operations::CanonicalOperationInputDigest>()
    );

    let changed_program = Program::Progress { capability: true };
    let mut changed = exact.clone();
    changed.input_digest = changed_program.digest();
    changed.replay_disposition = changed_program.replay_disposition();
    changed.program = Some(changed_program);
    assert!(
        reopened
            .begin_public_operation(&changed, &[], &[])
            .await
            .is_err()
    );
}

#[tokio::test]
async fn durable_build_application_never_reinvokes_the_build_callback() {
    for position in [
        PublicCutPosition::InstructionApplied,
        PublicCutPosition::CallbackApplied,
        PublicCutPosition::Completed,
    ] {
        let fixture = Fixture::new().await;
        let build_intent =
            fixture.intent("durable-build", 2, Program::Build(build("durable-build")));
        let barrier = cut(position, 0);
        let operation = fixture
            .launch(build_intent.clone(), Some(barrier.clone()))
            .await;
        reached(&barrier).await;
        operation.crash_root().await;
        fixture.owner.registry().recover_unowned().await.unwrap();
        let recovered = fixture.launch(build_intent.clone(), None).await;
        terminal(&recovered).await;
        assert_eq!(
            fixture
                .trace
                .events
                .lock()
                .unwrap()
                .iter()
                .filter(|event| event.as_str() == "primary.build_replica.begin")
                .count(),
            1,
            "{position:?}"
        );
        terminal(
            &fixture
                .launch(
                    fixture.intent(
                        "remove-durable-build",
                        2,
                        Program::Remove(build("durable-build")),
                    ),
                    None,
                )
                .await,
        )
        .await;
        fixture.owner.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn ambiguous_build_is_retired_before_a_fresh_attempt() {
    let fixture = Fixture::new().await;
    let first = fixture.intent(
        "ambiguous-build",
        2,
        Program::Build(build("ambiguous-build")),
    );
    let barrier = cut(PublicCutPosition::CallbackSuccess, 0);
    let operation = fixture.launch(first.clone(), Some(barrier.clone())).await;
    reached(&barrier).await;
    operation.crash_root().await;
    fixture.owner.registry().recover_unowned().await.unwrap();
    let pending = wait_for_stage(&operation, PublicOperationStage::ContainmentPending).await;
    assert!(matches!(
        pending.disposition,
        Some(PublicOperationDisposition::Ambiguous(_))
    ));
    assert!(
        public_lifecycle::launch(
            &fixture.owner.registry(),
            fixture.store.clone(),
            fixture.intent("fresh-build", 2, Program::Build(build("fresh-build"))),
            fixture.callbacks.clone(),
        )
        .await
        .is_err()
    );

    fixture.owner.registry().recover_unowned().await.unwrap();
    wait_for_stage(&operation, PublicOperationStage::Completed).await;
    let removal = fixture
        .launch(
            fixture.intent(
                "retire-ambiguous",
                2,
                Program::Remove(build("ambiguous-build")),
            ),
            None,
        )
        .await;
    terminal(&removal).await;
    terminal(
        &fixture
            .launch(
                fixture.intent("fresh-build", 2, Program::Build(build("fresh-build"))),
                None,
            )
            .await,
    )
    .await;
    let preview = fixture
        .store
        .load_state()
        .await
        .unwrap()
        .public_operation_preview
        .unwrap();
    assert!(preview.absent_builds.contains(&first.operation_id));
    assert_eq!(
        preview.active_builds[&ReplicaId::new(2)],
        build("fresh-build")
    );
    fixture.owner.shutdown().await.unwrap();
}

#[tokio::test]
async fn fresh_process_reopen_never_reconstructs_program_authority_at_any_cut() {
    for family in 0..4 {
        for cut in 0..5 {
            let preview = PublicOperationPreviewIdentity::new(500 + family * 10 + cut);
            let (_directory, path, store) = durable_preview_store(&preview);
            let mut operation_intent = intent(
                &preview,
                &format!("program-{family}-cut-{cut}"),
                1,
                PublicOperationClass::Authority,
                "session-1",
            );
            match family {
                0 => {
                    let program = Program::Epoch {
                        epoch: Epoch::new(0, 2),
                    };
                    operation_intent.input_digest = program.digest();
                    operation_intent.program = Some(program);
                }
                1 => {
                    let program = Program::Role {
                        epoch: Epoch::new(0, 2),
                        role: ReplicaRole::Primary,
                    };
                    operation_intent.input_digest = program.digest();
                    operation_intent.program = Some(program);
                }
                2 => {
                    let program = Program::Configuration(configuration(2));
                    operation_intent.input_digest = program.digest();
                    operation_intent.program = Some(program);
                }
                3 => {
                    let lifecycle = primary_lifecycle();
                    operation_intent.input_digest = lifecycle.digest();
                    operation_intent.lifecycle = Some(lifecycle);
                }
                _ => unreachable!(),
            }
            store
                .begin_public_operation(&operation_intent, &[], &[])
                .await
                .unwrap();
            if cut >= 1 {
                store
                    .advance_public_operation(
                        &operation_intent.operation_id,
                        operation_intent.revision,
                        &operation_intent.process_session_id,
                        PublicOperationStage::Ready,
                        PublicOperationStage::Running,
                        None,
                    )
                    .await
                    .unwrap();
            }
            if cut >= 2 {
                for (index, instruction) in
                    public_lifecycle::operation_instructions(&operation_intent)
                        .into_iter()
                        .enumerate()
                {
                    let outcome = match instruction {
                        PublicInstruction::ApplicationRole
                        | PublicInstruction::ApplicationPrimary => {
                            PublicInstructionOutcome::ApplicationRole(Some(
                                "historical://location".into(),
                            ))
                        }
                        PublicInstruction::ReplicatorOpen => {
                            PublicInstructionOutcome::Endpoint("historical://replicator".into())
                        }
                        PublicInstruction::Progress => PublicInstructionOutcome::Progress(7),
                        _ => PublicInstructionOutcome::Done,
                    };
                    store
                        .public_instruction(&operation_intent, index, instruction, None)
                        .await
                        .unwrap();
                    store
                        .public_instruction(&operation_intent, index, instruction, Some(outcome))
                        .await
                        .unwrap();
                }
            }
            if cut >= 3 {
                store
                    .advance_public_operation(
                        &operation_intent.operation_id,
                        operation_intent.revision,
                        &operation_intent.process_session_id,
                        PublicOperationStage::Running,
                        PublicOperationStage::CallbackApplied,
                        Some(PublicOperationDisposition::Succeeded),
                    )
                    .await
                    .unwrap();
            }
            if cut >= 4 {
                store
                    .advance_public_operation(
                        &operation_intent.operation_id,
                        operation_intent.revision,
                        &operation_intent.process_session_id,
                        PublicOperationStage::CallbackApplied,
                        PublicOperationStage::Completed,
                        None,
                    )
                    .await
                    .unwrap();
            }
            drop(store);

            let reopened =
                Arc::new(SqliteStore::open_preview_existing(&path, None, &preview).unwrap());
            let registry = PartitionOperationRegistry::new(
                reopened.clone(),
                preview.clone(),
                ProcessSessionId::new("session-2"),
            )
            .unwrap();
            registry.recover_unowned().await.unwrap();
            let record = registry
                .operation(&operation_intent.operation_id)
                .await
                .unwrap()
                .snapshot();
            if cut < 4 {
                assert_eq!(record.stage, PublicOperationStage::ContainmentPending);
                assert!(matches!(
                    record.disposition,
                    Some(PublicOperationDisposition::Ambiguous(_))
                ));
            } else {
                assert_eq!(record.stage, PublicOperationStage::Completed);
            }
            let report = public_lifecycle::report(
                reopened.as_ref(),
                &preview,
                &ProcessSessionId::new("session-2"),
            )
            .await
            .unwrap();
            assert_eq!(report.role, ReplicaRole::None);
            assert!(!report.write_access);
            assert!(report.service_location.is_none());

            let mut successor = operation_intent.clone();
            successor.operation_id = OperationId::new(format!("successor-{family}-{cut}"));
            successor.revision = 2;
            successor.process_session_id = ProcessSessionId::new("session-2");
            successor.input_digest = successor
                .program
                .as_ref()
                .map(Program::digest)
                .or_else(|| {
                    successor
                        .lifecycle
                        .as_ref()
                        .map(PublicLifecycleInput::digest)
                })
                .unwrap();
            assert!(registry.admit(successor).await.is_err());
            registry.shutdown().await.unwrap();
        }
    }
}
