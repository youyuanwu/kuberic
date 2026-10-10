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
    let current = ConfigurationDescriptor::new(
        Epoch::new(1, epoch),
        ReplicaId::new(1),
        vec![ConfigurationMember {
            identity: replica_identity(),
            role: ReplicaRole::Primary,
        }],
        1,
    );
    PublicConfiguration {
        previous: Some(ConfigurationDescriptor::new(
            Epoch::new(1, epoch - 1),
            ReplicaId::new(1),
            current.members.clone(),
            1,
        )),
        current,
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
            identity: target,
            role: ReplicaRole::ActiveSecondary,
        },
    ];
    PublicConfiguration {
        previous: Some(ConfigurationDescriptor::new(
            Epoch::new(1, epoch - 1),
            ReplicaId::new(1),
            members.clone(),
            1,
        )),
        current: ConfigurationDescriptor::new(Epoch::new(1, epoch), ReplicaId::new(1), members, 1),
    }
}

fn swap() -> Program {
    Program::Swap {
        starting: configuration(10),
        refreshed: configuration(11),
        epoch: Epoch::new(1, 11),
        handoff: ReplicaRole::ActiveSecondary,
        mode: PublicCatchUpMode::All,
    }
}

impl Fixture {
    async fn new() -> Self {
        let preview = PublicOperationPreviewIdentity::new(430);
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

    fn intent(&self, id: &str, revision: u64, program: Program) -> PublicOperationIntent {
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
async fn swap_installs_exact_starting_configuration_before_both_captured_mode_waits() {
    for selected in [PublicCatchUpMode::All, PublicCatchUpMode::WriteQuorum] {
        let fixture = Fixture::new().await;
        *fixture.trace.installed.lock().unwrap() = Some(configuration(2));
        fixture.trace.events.lock().unwrap().clear();
        let mut program = swap();
        if let Program::Swap { mode, .. } = &mut program {
            *mode = selected;
        }
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
        assert_eq!(
            *fixture.trace.waits.lock().unwrap(),
            vec![(configuration(10), mode), (configuration(11), mode)]
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
    let fixture = Fixture::new().await;
    let expanded = configuration_with_target(3);
    let lifecycle = PublicLifecycleInput {
        recipe: PublicLifecycleRecipe::InitialPrimary,
        replica: replica_identity(),
        epoch: expanded.current.epoch,
        possible_data_loss: PossibleDataLossIntent::NotPossible,
        current: expanded.current,
        previous: expanded.previous,
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
                epoch: Epoch::new(1, 4),
                handoff: ReplicaRole::ActiveSecondary,
                mode: PublicCatchUpMode::All,
            },
        ),
        (
            "swap-refreshed",
            Program::Swap {
                starting: configuration(3),
                refreshed: configuration_with_target(4),
                epoch: Epoch::new(1, 4),
                handoff: ReplicaRole::ActiveSecondary,
                mode: PublicCatchUpMode::All,
            },
        ),
    ] {
        let fixture = Fixture::new().await;
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

    let fixture = Fixture::new().await;
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
    let build = fixture.intent(
        "historical-target-build",
        5,
        Program::Build(build("historical-target-build")),
    );
    assert!(
        fixture
            .store
            .begin_public_operation(&build, &[], &[])
            .await
            .is_ok(),
        "superseded historical topology must not poison a future build"
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
                    epoch: Epoch::new(1, 2)
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
                epoch: Epoch::new(1, 3),
                role: ReplicaRole::Primary,
            },
            "replicator.change_role",
        ),
        (
            Program::Role {
                epoch: Epoch::new(1, 3),
                role: ReplicaRole::Primary,
            },
            "application.change_role",
        ),
        (
            Program::Epoch {
                epoch: Epoch::new(1, 3),
            },
            "replicator.update_epoch",
        ),
        (
            Program::Epoch {
                epoch: Epoch::new(1, 3),
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
                epoch: Epoch::new(1, 3),
                role: ReplicaRole::Primary,
            },
        ),
        (
            "epoch-replay",
            Program::Epoch {
                epoch: Epoch::new(1, 3),
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
                        epoch: Epoch::new(1, 2),
                    };
                    operation_intent.input_digest = program.digest();
                    operation_intent.program = Some(program);
                }
                1 => {
                    let program = Program::Role {
                        epoch: Epoch::new(1, 2),
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
