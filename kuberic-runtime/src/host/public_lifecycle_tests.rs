use super::*;
use crate::RuntimeError;
use crate::application::{OpenContext, RoleChange};
use crate::host::sqlite_store::SqliteStore;
use crate::host::state::{AgentState, StorageIdentity};
use crate::protocol::public_operations::{
    PublicConfiguration, PublicV1LifecycleBinding, StatePersistence,
};
use crate::protocol::types::*;
use crate::replicator::{ReplicaInformation, ReplicaSetConfiguration};
use std::sync::{
    Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::time::Duration;
use tokio::sync::Notify;

#[derive(Default)]
struct Probe {
    events: Mutex<Vec<String>>,
    none_calls: AtomicUsize,
    block_none: AtomicBool,
    fail_replicator_close: AtomicBool,
    fail_application_close: AtomicBool,
    block_application_close: AtomicBool,
    fail_role: AtomicBool,
    fail_application_role: AtomicBool,
    listener_close: Mutex<Option<TerminalDrain>>,
    listener_abort: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    role: Mutex<Option<ReplicaRole>>,
    close_entered: Notify,
    catchup_entered: Notify,
    catchup_release: Notify,
    block_catchup: AtomicBool,
    drain_entered: Notify,
    drain_release: Notify,
    block_drain: AtomicBool,
    fail_drain: AtomicBool,
}

impl Probe {
    fn event(&self, event: impl Into<String>) {
        self.events.lock().unwrap().push(event.into());
    }
}

struct Application(Arc<Probe>);
struct Replication(Arc<Probe>);

#[async_trait::async_trait]
impl StatefulServiceReplica for Application {
    async fn open(self: Arc<Self>, _: OpenContext) -> crate::Result<Arc<dyn Replicator>> {
        Ok(Arc::new(Replication(self.0.clone())))
    }

    async fn change_role(&self, role: ReplicaRole) -> crate::Result<RoleChange> {
        if role == ReplicaRole::None {
            self.0.none_calls.fetch_add(1, Ordering::SeqCst);
            if self.0.block_none.load(Ordering::Acquire) {
                std::future::pending::<()>().await;
            }
            return Err(RuntimeError::Application("synthetic None callback".into()));
        }
        assert_eq!(*self.0.role.lock().unwrap(), Some(role));
        self.0.event(format!("application.role.{role:?}"));
        if self.0.fail_application_role.load(Ordering::Acquire) {
            return Err(RuntimeError::Application(
                "actual application role injected".into(),
            ));
        }
        Ok(RoleChange {
            service_address: None,
        })
    }

    async fn close(&self) -> crate::Result<()> {
        self.0.event("application.close");
        self.0.close_entered.notify_one();
        if self.0.block_application_close.load(Ordering::Acquire) {
            std::future::pending::<()>().await;
        }
        if self.0.fail_application_close.load(Ordering::Acquire) {
            return Err(RuntimeError::Application(
                "application close injected".into(),
            ));
        }
        Ok(())
    }

    fn abort(&self) {
        self.0.event("application.abort");
    }
}

#[async_trait::async_trait]
impl Replicator for Replication {
    async fn open(&self) -> crate::Result<String> {
        Ok("http://127.0.0.1:1".into())
    }
    async fn change_role(&self, _: Epoch, role: ReplicaRole) -> crate::Result<()> {
        self.0.event(format!("replicator.role.{role:?}"));
        if self.0.fail_role.load(Ordering::Acquire) {
            return Err(RuntimeError::Application("actual role injected".into()));
        }
        *self.0.role.lock().unwrap() = Some(role);
        Ok(())
    }
    async fn update_epoch(&self, _: Epoch) -> crate::Result<()> {
        self.0.event("epoch");
        Ok(())
    }
    async fn close(&self) -> crate::Result<()> {
        self.0.event("replicator.close");
        let listener_close = self.0.listener_close.lock().unwrap().clone();
        if let Some(close) = listener_close {
            close().await?;
        }
        if self.0.fail_replicator_close.load(Ordering::Acquire) {
            return Err(RuntimeError::Application(
                "replicator close injected".into(),
            ));
        }
        Ok(())
    }
    fn abort(&self) {
        self.0.event("replicator.abort");
        if let Some(abort) = self.0.listener_abort.lock().unwrap().as_ref() {
            abort();
        }
    }
    async fn current_progress(&self) -> crate::Result<i64> {
        Ok(0)
    }
    async fn catch_up_capability(&self) -> crate::Result<i64> {
        Ok(0)
    }
}

#[async_trait::async_trait]
impl PrimaryReplicator for Replication {
    async fn on_data_loss(&self) -> crate::Result<bool> {
        Ok(false)
    }
    async fn update_catch_up_replica_set_configuration(
        &self,
        _: ReplicaSetConfiguration,
        _: ReplicaSetConfiguration,
    ) -> crate::Result<()> {
        self.0.event("configuration");
        Ok(())
    }
    async fn update_current_replica_set_configuration(
        &self,
        _: ReplicaSetConfiguration,
    ) -> crate::Result<()> {
        self.0.event("configuration");
        Ok(())
    }
    async fn wait_for_catch_up_quorum(&self, _: ReplicaSetQuorumMode) -> crate::Result<()> {
        assert_eq!(*self.0.role.lock().unwrap(), Some(ReplicaRole::Primary));
        self.0.event("catchup");
        self.0.catchup_entered.notify_one();
        if self.0.block_catchup.load(Ordering::Acquire) {
            self.0.catchup_release.notified().await;
        }
        Ok(())
    }
    async fn build_replica(&self, _: ReplicaInformation) -> crate::Result<()> {
        self.0.event("build");
        Ok(())
    }
    async fn remove_replica(&self, _: ReplicaId) -> crate::Result<()> {
        Ok(())
    }
}

struct Fixture {
    _root: tempfile::TempDir,
    store: Arc<SqliteStore>,
    registry: Arc<PartitionOperationRegistry>,
    probe: Arc<Probe>,
    callbacks: PublicLifecycleCallbacks,
}

impl Fixture {
    async fn new() -> Self {
        let root = crate::host::tests::tempdir().unwrap();
        let resource = ResourceUid::new("sf06");
        let identity = ReplicaIdentity {
            replica_id: ReplicaId::new(1),
            instance_id: ReplicaInstanceId::new("pod"),
            agent_generation: AgentGeneration::new("agent"),
        };
        let preview = PublicV1Identity::new(6);
        let store = Arc::new(
            SqliteStore::create_replacement_authorized(
                root.path().join("agent.sqlite3"),
                AgentState::new(StorageIdentity {
                    schema_version: crate::host::state::SCHEMA_VERSION,
                    resource_uid: resource.clone(),
                    pod_uid: PodUid::new("pod"),
                    pvc_uid: PvcUid::new("pvc"),
                    initialization_id: InitializationId::new("init"),
                    local_identity: identity.clone(),
                    effective_policy: EffectivePolicy::fixed(1, 30).unwrap(),
                }),
                PublicV1LifecycleBinding {
                    identity: preview.clone(),
                    resource_uid: resource.clone(),
                    spec_generation: 1,
                    state_persistence: StatePersistence::Persisted,
                },
            )
            .unwrap(),
        );
        let generation = PublicProtocolGeneration::new(6);
        let initialization = derive_history_initialization_id(&resource, generation);
        let history = derive_bootstrap_history(&resource, generation, &initialization);
        let storage = ReplicaStorageIncarnationContext {
            replica: identity,
            storage: ReplicaStorageIdentity::Persistent {
                pod_uid: PodUid::new("pod"),
                pvc_uid: PvcUid::new("pvc"),
            },
            storage_generation: StorageGeneration::new("storage"),
        };
        store
            .initialize_public_v1_history(&initialization, &history, &storage)
            .await
            .unwrap();
        let registry = PartitionOperationRegistry::new(
            store.clone(),
            preview,
            ProcessSessionId::new("session"),
        )
        .unwrap();
        let probe = Arc::new(Probe::default());
        *probe.role.lock().unwrap() = Some(ReplicaRole::Primary);
        let replication = Arc::new(Replication(probe.clone()));
        let drain_probe = probe.clone();
        let callbacks = PublicLifecycleCallbacks {
            application: Arc::new(Application(probe.clone())),
            replicator: replication.clone(),
            primary: replication,
            application_control_match: None,
            capabilities: ReplicatorCapabilities::conservative(),
            replicator_creation_id: ReplicatorCreationId::new("creation"),
            open_context: None,
            containment: None,
            wait_for_containment: false,
            terminal_drain: Arc::new(move || {
                let probe = drain_probe.clone();
                Box::pin(async move {
                    probe.event("drain.begin");
                    probe.drain_entered.notify_one();
                    if probe.block_drain.load(Ordering::Acquire) {
                        probe.drain_release.notified().await;
                    }
                    probe.event("drain.end");
                    if probe.fail_drain.load(Ordering::Acquire) {
                        Err(RuntimeError::Application("drain injected".into()))
                    } else {
                        Ok(())
                    }
                })
            }),
            terminal_cleanup: None,
            aborted: Arc::new(Mutex::new(false)),
            cut: None,
        };
        Self {
            _root: root,
            store,
            registry,
            probe,
            callbacks,
        }
    }

    fn intent(&self, program: PublicOperationProgram, revision: u64) -> PublicOperationIntent {
        use crate::protocol::public_operations::PublicOperationClass;
        PublicOperationIntent {
            identity: PublicV1Identity::new(6),
            operation_id: OperationId::new(format!("sf06-{revision}")),
            revision,
            process_session_id: ProcessSessionId::new("session"),
            class: match program {
                PublicOperationProgram::Abort => PublicOperationClass::Abort,
                PublicOperationProgram::Close => PublicOperationClass::Close,
                PublicOperationProgram::Swap { .. } => PublicOperationClass::PlannedSwap,
                _ => PublicOperationClass::Authority,
            },
            input_digest: program.digest(),
            replay_disposition: program.replay_disposition(),
            lifecycle: None,
            program: Some(program),
        }
    }

    async fn launch(
        &self,
        program: PublicOperationProgram,
        revision: u64,
    ) -> Arc<PartitionOperation> {
        launch(
            &self.registry,
            self.store.clone(),
            self.intent(program, revision),
            self.callbacks.clone(),
        )
        .await
        .unwrap()
    }
}

async fn completed(operation: &PartitionOperation) -> PublicOperationRecord {
    tokio::time::timeout(Duration::from_secs(5), operation.wait_for_terminal())
        .await
        .unwrap()
        .unwrap()
}

#[tokio::test]
async fn promoted_build_releases_its_slot_for_a_fresh_incarnation_without_losing_replay() {
    use crate::protocol::public_operations::{PublicOperationClass, PublicReplicaDescriptor};

    let fixture = Fixture::new().await;
    let authority = fixture
        .launch(
            PublicOperationProgram::Role {
                epoch: Epoch::new(0, 1),
                role: ReplicaRole::Primary,
            },
            1,
        )
        .await;
    assert_eq!(
        completed(&authority).await.disposition,
        Some(PublicOperationDisposition::Succeeded)
    );
    let state = fixture.store.load_state().await.unwrap();
    let source = state.identity.local_identity;
    let history = state.public_v1_operation_state.unwrap().history.unwrap();
    let target = ReplicaIdentity {
        replica_id: ReplicaId::new(2),
        instance_id: ReplicaInstanceId::new("built-pod"),
        agent_generation: AgentGeneration::new("built-agent"),
    };
    let target_storage = ReplicaStorageIncarnationContext {
        replica: target.clone(),
        storage: ReplicaStorageIdentity::Persistent {
            pod_uid: PodUid::new("built-pod"),
            pvc_uid: PvcUid::new("built-pvc"),
        },
        storage_generation: StorageGeneration::new("built-storage"),
    };
    let build = PublicBuildInput {
        attempt: OperationId::new("first-build"),
        epoch: Epoch::new(0, 1),
        current_configuration_id: Some(
            ConfigurationDescriptor::new(
                Epoch::new(0, 1),
                source.replica_id,
                vec![ConfigurationMember {
                    identity: source.clone(),
                    role: ReplicaRole::Primary,
                }],
                1,
            )
            .configuration_id,
        ),
        replica: target.clone(),
        target_storage: target_storage.clone(),
        process_session_id: ProcessSessionId::new("built-session"),
        replication_address: "http://127.0.0.1:2".into(),
        history: Some(history),
    };
    let build_intent = |build: PublicBuildInput, revision| {
        let mut intent = fixture.intent(PublicOperationProgram::Build(build.clone()), revision);
        intent.operation_id = build.attempt;
        intent.class = PublicOperationClass::Build {
            target: build.replica.replica_id,
        };
        intent
    };
    let first_intent = build_intent(build.clone(), 1);
    let first = launch(
        &fixture.registry,
        fixture.store.clone(),
        first_intent.clone(),
        fixture.callbacks.clone(),
    )
    .await
    .unwrap();
    let first_result = completed(&first).await;
    assert_eq!(
        first_result.disposition,
        Some(PublicOperationDisposition::Succeeded)
    );
    let replacement = ReplicaIdentity {
        replica_id: target.replica_id,
        instance_id: ReplicaInstanceId::new("replacement-pod"),
        agent_generation: AgentGeneration::new("replacement-agent"),
    };
    let mut successor = build.clone();
    successor.attempt = OperationId::new("replacement-build");
    successor.replica = replacement.clone();
    successor.target_storage = ReplicaStorageIncarnationContext {
        replica: replacement,
        storage: ReplicaStorageIdentity::Persistent {
            pod_uid: PodUid::new("replacement-pod"),
            pvc_uid: PvcUid::new("replacement-pvc"),
        },
        storage_generation: StorageGeneration::new("replacement-storage"),
    };
    successor.process_session_id = ProcessSessionId::new("replacement-session");
    successor.replication_address = "http://127.0.0.1:3".into();
    assert!(
        fixture
            .registry
            .admit(build_intent(successor.clone(), 1))
            .await
            .is_err(),
        "an unpromoted idle Build still owns its logical replica slot"
    );
    let descriptor = |storage| PublicReplicaDescriptor {
        build_id: OperationId::default(),
        identity: target.clone(),
        replication_address: build.replication_address.clone(),
        process_session_id: build.process_session_id.clone(),
        storage_incarnation: Some(storage),
        role: ReplicaRole::ActiveSecondary,
        current_progress: 0,
        catch_up_capability: 0,
        must_catchup: false,
    };
    let epoch_primary = ReplicaIdentity {
        replica_id: ReplicaId::new(3),
        instance_id: ReplicaInstanceId::new("epoch-primary"),
        agent_generation: AgentGeneration::new("epoch-primary-agent"),
    };
    let epoch_only = PublicLifecycleInput {
        recipe: PublicLifecycleRecipe::SecondaryEpochAdvance,
        replica: source.clone(),
        epoch: Epoch::new(0, 2),
        possible_data_loss: PossibleDataLossIntent::NotPossible,
        current: ConfigurationDescriptor::new(
            Epoch::new(0, 2),
            epoch_primary.replica_id,
            vec![
                ConfigurationMember {
                    identity: source.clone(),
                    role: ReplicaRole::ActiveSecondary,
                },
                ConfigurationMember {
                    identity: target.clone(),
                    role: ReplicaRole::ActiveSecondary,
                },
                ConfigurationMember {
                    identity: epoch_primary,
                    role: ReplicaRole::Primary,
                },
            ],
            1,
        ),
        previous: None,
        replicas: vec![descriptor(target_storage.clone())],
        data_loss_recovery: None,
    };
    let mut epoch_intent = fixture.intent(
        PublicOperationProgram::Epoch {
            epoch: epoch_only.epoch,
        },
        2,
    );
    epoch_intent.program = None;
    epoch_intent.input_digest = epoch_only.digest();
    epoch_intent.lifecycle = Some(epoch_only);
    let epoch = launch(
        &fixture.registry,
        fixture.store.clone(),
        epoch_intent,
        fixture.callbacks.clone(),
    )
    .await
    .unwrap();
    assert_eq!(
        completed(&epoch).await.disposition,
        Some(PublicOperationDisposition::Succeeded)
    );
    let before = fixture.store.load_state().await.unwrap();
    assert!(
        fixture
            .registry
            .admit(build_intent(successor.clone(), 2))
            .await
            .is_err(),
        "an epoch-only lifecycle does not prove configuration installation"
    );
    assert_eq!(fixture.store.load_state().await.unwrap(), before);
    for (revision, exact_storage) in [(3, false), (4, true)] {
        let epoch = Epoch::new(0, i64::try_from(revision).unwrap());
        let mut storage = target_storage.clone();
        if !exact_storage {
            storage.storage_generation = StorageGeneration::new("not-the-built-storage");
        }
        let configuration = PublicConfiguration {
            current: ConfigurationDescriptor::new(
                epoch,
                source.replica_id,
                vec![
                    ConfigurationMember {
                        identity: source.clone(),
                        role: ReplicaRole::Primary,
                    },
                    ConfigurationMember {
                        identity: target.clone(),
                        role: ReplicaRole::ActiveSecondary,
                    },
                ],
                1,
            ),
            previous: None,
            replicas: vec![descriptor(storage)],
        };
        let operation = fixture
            .launch(
                PublicOperationProgram::Configuration(configuration.clone()),
                revision,
            )
            .await;
        assert_eq!(
            completed(&operation).await.disposition,
            Some(PublicOperationDisposition::Succeeded)
        );
        successor.epoch = epoch;
        successor.current_configuration_id = Some(configuration.current.configuration_id);
        if !exact_storage {
            let before = fixture.store.load_state().await.unwrap();
            assert!(
                fixture
                    .registry
                    .admit(build_intent(successor.clone(), revision))
                    .await
                    .is_err(),
                "configuration of different storage cannot release the Build slot"
            );
            assert_eq!(fixture.store.load_state().await.unwrap(), before);
        }
    }
    let next = launch(
        &fixture.registry,
        fixture.store.clone(),
        build_intent(successor, 4),
        fixture.callbacks.clone(),
    )
    .await
    .unwrap();
    assert_eq!(
        completed(&next).await.disposition,
        Some(PublicOperationDisposition::Succeeded)
    );
    let preview = fixture
        .store
        .load_state()
        .await
        .unwrap()
        .public_v1_operation_state
        .unwrap();
    assert_eq!(preview.active_builds.len(), 1);
    assert_eq!(
        preview.active_builds[&target.replica_id].attempt,
        OperationId::new("replacement-build")
    );
    assert!(
        preview
            .retired_storage_incarnations
            .contains(&target_storage)
    );
    assert!(preview.absent_builds.contains(&build.attempt));
    assert_eq!(
        preview.build_records[&build.attempt].retirement,
        crate::host::public_build_store::PublicV1BuildRetirement::Retired
    );
    let replay = launch(
        &fixture.registry,
        fixture.store.clone(),
        first_intent,
        fixture.callbacks.clone(),
    )
    .await
    .unwrap();
    assert_eq!(completed(&replay).await, first_result);
    assert_eq!(
        fixture
            .probe
            .events
            .lock()
            .unwrap()
            .iter()
            .filter(|event| *event == "build")
            .count(),
        2,
        "replaying the old success must not rebuild its retired incarnation"
    );
    fixture.registry.shutdown().await.unwrap();
}

#[tokio::test]
async fn sf06_abort_and_close_never_call_erroring_or_blocked_synthetic_none() {
    for block in [false, true] {
        for abort in [false, true] {
            let fixture = Fixture::new().await;
            fixture.probe.block_none.store(block, Ordering::Release);
            let program = if abort {
                PublicOperationProgram::Abort
            } else {
                PublicOperationProgram::Close
            };
            let operation = fixture.launch(program.clone(), 1).await;
            let record = completed(&operation).await;
            assert_eq!(record.stage, PublicOperationStage::Completed);
            assert_eq!(
                record.disposition,
                Some(PublicOperationDisposition::Succeeded)
            );
            assert_eq!(fixture.probe.none_calls.load(Ordering::SeqCst), 0);
            let events = fixture.probe.events.lock().unwrap().clone();
            if abort {
                assert_eq!(
                    events,
                    [
                        "replicator.abort",
                        "application.abort",
                        "drain.begin",
                        "drain.end"
                    ]
                );
            }
            let replay = fixture.launch(program, 1).await;
            assert_eq!(completed(&replay).await, record);
            assert_eq!(*fixture.probe.events.lock().unwrap(), events);
            fixture.registry.shutdown().await.unwrap();
        }
    }
}

#[tokio::test]
async fn sf06_close_child_failures_abort_skip_graceful_work_and_preserve_errors() {
    for replication in [false, true] {
        let fixture = Fixture::new().await;
        fixture
            .probe
            .fail_replicator_close
            .store(replication, Ordering::Release);
        fixture
            .probe
            .fail_application_close
            .store(!replication, Ordering::Release);
        fixture
            .probe
            .block_application_close
            .store(replication, Ordering::Release);
        let operation = fixture.launch(PublicOperationProgram::Close, 1).await;
        let record = completed(&operation).await;
        assert_eq!(record.stage, PublicOperationStage::Completed);
        let Some(PublicOperationDisposition::Failed(error)) = record.disposition else {
            panic!("not failed");
        };
        assert!(error.contains(if replication {
            "replicator close injected"
        } else {
            "application close injected"
        }));
        assert!(
            record
                .lifecycle
                .outcomes
                .iter()
                .any(|outcome| matches!(outcome, PublicInstructionOutcome::CloseFailure { .. }))
        );
        let events = fixture.probe.events.lock().unwrap().clone();
        if replication {
            assert!(!events.iter().any(|event| event == "application.close"));
        }
        let r = events
            .iter()
            .position(|event| event == "replicator.abort")
            .unwrap();
        let a = events
            .iter()
            .position(|event| event == "application.abort")
            .unwrap();
        assert!(r < a);
        assert_eq!(events.last().unwrap(), "drain.end");
        fixture.registry.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn sf06_cancelled_close_retains_drain_owner_before_superseding_abort() {
    let fixture = Fixture::new().await;
    fixture
        .probe
        .block_application_close
        .store(true, Ordering::Release);
    fixture.probe.block_drain.store(true, Ordering::Release);
    let close = fixture.launch(PublicOperationProgram::Close, 1).await;
    fixture.probe.close_entered.notified().await;
    let abort = fixture.launch(PublicOperationProgram::Abort, 2).await;
    fixture.probe.drain_entered.notified().await;
    assert_eq!(
        close.snapshot().stage,
        PublicOperationStage::ContainmentPending
    );
    assert_eq!(
        abort.snapshot().stage,
        PublicOperationStage::WaitingForContainment
    );
    assert!(close.complete_containment().await.is_err());
    fixture.probe.block_drain.store(false, Ordering::Release);
    fixture.probe.drain_release.notify_one();
    assert_eq!(
        completed(&abort).await.stage,
        PublicOperationStage::Completed
    );
    assert_eq!(close.snapshot().stage, PublicOperationStage::Completed);
    let events = fixture.probe.events.lock().unwrap().clone();
    assert_eq!(
        events
            .iter()
            .filter(|event| *event == "application.abort")
            .count(),
        1
    );
    fixture.registry.shutdown().await.unwrap();
}

#[tokio::test]
async fn sf06_shutdown_cancels_roots_before_joining_terminal_owners() {
    let fixture = Fixture::new().await;
    fixture
        .probe
        .block_application_close
        .store(true, Ordering::Release);
    fixture.probe.block_drain.store(true, Ordering::Release);
    let close = fixture.launch(PublicOperationProgram::Close, 1).await;
    fixture.probe.close_entered.notified().await;
    let _abort = fixture.launch(PublicOperationProgram::Abort, 2).await;
    fixture.probe.drain_entered.notified().await;
    let registry = fixture.registry.clone();
    let mut shutdown = tokio::spawn(async move { registry.shutdown().await });
    assert!(
        tokio::time::timeout(Duration::from_millis(30), &mut shutdown)
            .await
            .is_err()
    );
    assert_ne!(close.snapshot().stage, PublicOperationStage::Completed);
    fixture.probe.drain_release.notify_one();
    tokio::time::timeout(Duration::from_secs(5), shutdown)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(close.snapshot().stage, PublicOperationStage::Completed);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sf06_listener_join_survives_close_supersession_and_registry_shutdown() {
    for shutdown_registry in [false, true] {
        let mut fixture = Fixture::new().await;
        let entered = Arc::new(Notify::new());
        let entered_task = entered.clone();
        let (release, blocked) = std::sync::mpsc::channel();
        let task = tokio::spawn(async move {
            entered_task.notify_one();
            // Model a listener poll in non-preemptible native code. Abort is
            // requested promptly but actual termination must still be joined.
            blocked.recv_timeout(Duration::from_secs(10)).unwrap();
            std::future::pending::<crate::Result<()>>().await
        });
        let listener = task.abort_handle();
        entered.notified().await;
        let (shutdown, mut closing) = tokio::sync::watch::channel(false);
        let (close_hook, drain_hook, abort_hook) =
            crate::replicator::stateless::sf06_listener_test_hooks(shutdown, task);
        *fixture.probe.listener_close.lock().unwrap() = Some(close_hook);
        *fixture.probe.listener_abort.lock().unwrap() = Some(abort_hook);
        fixture.callbacks.terminal_drain = drain_hook;
        let close = fixture.launch(PublicOperationProgram::Close, 1).await;
        tokio::time::timeout(Duration::from_secs(5), closing.wait_for(|closing| *closing))
            .await
            .unwrap()
            .unwrap();
        let mut shutdown = None;
        let mut successor = None;
        if shutdown_registry {
            let registry = fixture.registry.clone();
            shutdown = Some(tokio::spawn(async move { registry.shutdown().await }));
        } else {
            successor = Some(fixture.launch(PublicOperationProgram::Abort, 2).await);
        }
        tokio::time::timeout(Duration::from_secs(5), async {
            while close.snapshot().stage != PublicOperationStage::ContainmentPending {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(!listener.is_finished());
        assert!(close.complete_containment().await.is_err());
        if let Some(successor) = &successor {
            assert_eq!(
                successor.snapshot().stage,
                PublicOperationStage::WaitingForContainment
            );
        }
        if let Some(shutdown) = &mut shutdown {
            assert!(
                tokio::time::timeout(Duration::from_millis(30), shutdown)
                    .await
                    .is_err()
            );
        }
        release.send(()).unwrap();
        if let Some(successor) = successor {
            assert_eq!(
                completed(&successor).await.stage,
                PublicOperationStage::Completed
            );
            fixture.registry.shutdown().await.unwrap();
        }
        if let Some(shutdown) = shutdown {
            tokio::time::timeout(Duration::from_secs(5), shutdown)
                .await
                .unwrap()
                .unwrap()
                .unwrap();
        }
        assert!(listener.is_finished());
        assert_eq!(close.snapshot().stage, PublicOperationStage::Completed);
    }
}

#[tokio::test]
async fn sf06_terminal_cleanup_errors_do_not_hide_close_errors() {
    let fixture = Fixture::new().await;
    fixture
        .probe
        .fail_application_close
        .store(true, Ordering::Release);
    fixture.probe.fail_drain.store(true, Ordering::Release);
    let operation = fixture.launch(PublicOperationProgram::Close, 1).await;
    let record = completed(&operation).await;
    let Some(PublicOperationDisposition::Failed(error)) = record.disposition else {
        panic!("not failed");
    };
    assert!(error.contains("application close injected"));
    assert!(error.contains("drain injected"));
    fixture.registry.shutdown().await.unwrap();
}

#[tokio::test]
async fn sf06_failed_close_and_cancelled_abort_cannot_outrun_owned_cleanup() {
    for failed_close in [false, true] {
        let fixture = Fixture::new().await;
        fixture.probe.block_drain.store(true, Ordering::Release);
        fixture
            .probe
            .fail_replicator_close
            .store(failed_close, Ordering::Release);
        let operation = fixture
            .launch(
                if failed_close {
                    PublicOperationProgram::Close
                } else {
                    PublicOperationProgram::Abort
                },
                1,
            )
            .await;
        fixture.probe.drain_entered.notified().await;
        assert_eq!(operation.snapshot().stage, PublicOperationStage::Running);
        assert!(
            fixture
                .probe
                .events
                .lock()
                .unwrap()
                .iter()
                .any(|event| event == "application.abort")
        );
        if !failed_close {
            operation.cancel_root().await.unwrap();
            assert!(operation.complete_containment().await.is_err());
        } else {
            assert!(
                tokio::time::timeout(Duration::from_millis(30), operation.wait_for_terminal())
                    .await
                    .is_err()
            );
        }
        fixture.probe.drain_release.notify_one();
        if failed_close {
            let record = completed(&operation).await;
            assert_eq!(record.stage, PublicOperationStage::Completed);
            assert!(
                matches!(record.disposition, Some(PublicOperationDisposition::Failed(ref error)) if error.contains("replicator close injected"))
            );
        }
        fixture.registry.shutdown().await.unwrap();
        assert_eq!(operation.snapshot().stage, PublicOperationStage::Completed);
        assert_eq!(
            fixture.probe.events.lock().unwrap().last().unwrap(),
            "drain.end"
        );
    }
}

#[tokio::test]
async fn sf06_swap_retains_primary_through_catchup_then_pairs_actual_roles() {
    for controller_composed in [false, true] {
        let fixture = Fixture::new().await;
        let authority = fixture
            .launch(
                PublicOperationProgram::Role {
                    epoch: Epoch::new(0, 1),
                    role: ReplicaRole::Primary,
                },
                1,
            )
            .await;
        assert_eq!(
            completed(&authority).await.disposition,
            Some(PublicOperationDisposition::Succeeded)
        );
        fixture.probe.events.lock().unwrap().clear();
        fixture.probe.block_none.store(true, Ordering::Release);
        fixture.probe.block_catchup.store(true, Ordering::Release);
        let identity = fixture
            .store
            .load_state()
            .await
            .unwrap()
            .identity
            .local_identity;
        let epoch = Epoch::new(0, 2);
        let configuration = PublicConfiguration {
            current: ConfigurationDescriptor::new(
                Epoch::new(0, 1),
                identity.replica_id,
                vec![ConfigurationMember {
                    identity: identity.clone(),
                    role: ReplicaRole::Primary,
                }],
                1,
            ),
            previous: None,
            replicas: vec![],
        };
        let refreshed = PublicConfiguration {
            current: ConfigurationDescriptor::new(
                epoch,
                identity.replica_id,
                vec![ConfigurationMember {
                    identity,
                    role: ReplicaRole::Primary,
                }],
                1,
            ),
            previous: None,
            replicas: vec![],
        };
        let operation = fixture
            .launch(
                PublicOperationProgram::Swap {
                    epoch,
                    starting: configuration,
                    refreshed,
                    mode: PublicCatchUpMode::All,
                    catch_up_specific_quorum: false,
                    handoff: ReplicaRole::ActiveSecondary,
                    controller_composed,
                },
                2,
            )
            .await;
        for _ in 0..if controller_composed { 1 } else { 2 } {
            fixture.probe.catchup_entered.notified().await;
            assert_eq!(
                *fixture.probe.role.lock().unwrap(),
                Some(ReplicaRole::Primary)
            );
            assert!(
                !fixture
                    .probe
                    .events
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|event| event.starts_with("application.role"))
            );
            fixture.probe.catchup_release.notify_one();
        }
        assert_eq!(
            completed(&operation).await.disposition,
            Some(PublicOperationDisposition::Succeeded)
        );
        let events = fixture.probe.events.lock().unwrap().clone();
        assert_eq!(
            &events[events.len() - 2..],
            [
                "replicator.role.ActiveSecondary",
                "application.role.ActiveSecondary"
            ]
        );
        assert_eq!(fixture.probe.none_calls.load(Ordering::SeqCst), 0);
        assert!(
            fixture
                .store
                .load_state()
                .await
                .unwrap()
                .public_v1_operation_state
                .unwrap()
                .writes_revoked
        );
        fixture.registry.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn sf06_actual_replicator_role_failure_is_not_hidden_or_sent_to_application() {
    let fixture = Fixture::new().await;
    fixture.probe.fail_role.store(true, Ordering::Release);
    let operation = fixture
        .launch(
            PublicOperationProgram::Role {
                epoch: Epoch::new(0, 2),
                role: ReplicaRole::ActiveSecondary,
            },
            1,
        )
        .await;
    let record = completed(&operation).await;
    assert!(
        matches!(record.disposition, Some(PublicOperationDisposition::Failed(ref error)) if error.contains("actual role injected"))
    );
    assert_eq!(
        *fixture.probe.events.lock().unwrap(),
        ["replicator.role.ActiveSecondary"]
    );
    fixture.registry.shutdown().await.unwrap();
}

#[tokio::test]
async fn sf06_actual_application_role_failure_remains_failed_after_replication_role() {
    let fixture = Fixture::new().await;
    fixture
        .probe
        .fail_application_role
        .store(true, Ordering::Release);
    let operation = fixture
        .launch(
            PublicOperationProgram::Role {
                epoch: Epoch::new(0, 2),
                role: ReplicaRole::ActiveSecondary,
            },
            1,
        )
        .await;
    assert!(matches!(
        completed(&operation).await.disposition,
        Some(PublicOperationDisposition::Failed(ref error)) if error.contains("actual application role injected")
    ));
    assert_eq!(
        *fixture.probe.events.lock().unwrap(),
        [
            "replicator.role.ActiveSecondary",
            "application.role.ActiveSecondary",
        ]
    );
    fixture.registry.shutdown().await.unwrap();
}

#[tokio::test]
async fn sf06_terminal_custom_witness_blocks_completion_and_lost_witness_is_not_containment() {
    for lose_witness in [false, true] {
        let mut fixture = Fixture::new().await;
        let (sender, receiver) = tokio::sync::watch::channel(false);
        fixture.callbacks.containment = Some(receiver);
        fixture.callbacks.wait_for_containment = true;
        let operation = fixture.launch(PublicOperationProgram::Abort, 1).await;
        fixture.probe.drain_entered.notified().await;
        assert_ne!(operation.snapshot().stage, PublicOperationStage::Completed);
        if lose_witness {
            drop(sender);
            let record = completed(&operation).await;
            assert_eq!(record.stage, PublicOperationStage::ContainmentPending);
            assert!(
                matches!(record.disposition, Some(PublicOperationDisposition::Failed(ref error)) if error.contains("witness lost"))
            );
            assert!(operation.complete_containment().await.is_err());
        } else {
            sender.send_replace(true);
            assert_eq!(
                completed(&operation).await.stage,
                PublicOperationStage::Completed
            );
        }
        fixture.registry.shutdown().await.unwrap();
        if lose_witness {
            assert_ne!(operation.snapshot().stage, PublicOperationStage::Completed);
        }
    }
}

#[tokio::test]
async fn sf06_replayed_terminal_cuts_reestablish_containment_and_preserve_first_close_failure() {
    use crate::host::state::PublicCloseChild;
    for cut in [0, 1, 3] {
        let fixture = Fixture::new().await;
        fixture.probe.fail_drain.store(cut == 3, Ordering::Release);
        fixture.probe.block_none.store(true, Ordering::Release);
        fixture
            .probe
            .block_application_close
            .store(cut > 0, Ordering::Release);
        let intent = fixture.intent(PublicOperationProgram::Close, 1);
        let operation = fixture.registry.admit(intent.clone()).await.unwrap();
        let mut callbacks = fixture.callbacks.clone();
        callbacks.aborted = fixture.registry.abort_guard();
        let (cleanup, owner) = terminal::bind(callbacks.clone());
        operation
            .track_containment(cleanup.witness(), true)
            .await
            .unwrap();
        callbacks.terminal_cleanup = Some(cleanup);
        fixture.registry.own_terminal_task(owner);
        let store = fixture.store.clone();
        operation
            .spawn_root(CallbackContainment::RootTask, async move {
                // Seed the durable state at the same boundaries as an interrupted
                // process, then run the ordinary executor against the retained log.
                if cut == 0 {
                    store
                        .public_instruction(&intent, 0, PublicInstruction::Revoke, None)
                        .await?;
                } else {
                    for (index, instruction, outcome) in [
                        (0, PublicInstruction::Revoke, PublicInstructionOutcome::Done),
                        (
                            1,
                            PublicInstruction::ReplicatorClose,
                            PublicInstructionOutcome::CloseFailure {
                                child: PublicCloseChild::Replicator,
                                error: "retained first close failure".into(),
                            },
                        ),
                    ] {
                        store
                            .public_instruction(&intent, index, instruction, None)
                            .await?;
                        store
                            .public_instruction(&intent, index, instruction, Some(outcome))
                            .await?;
                    }
                    if cut == 3 {
                        store
                            .public_instruction(
                                &intent,
                                2,
                                PublicInstruction::ApplicationClose,
                                None,
                            )
                            .await?;
                        store
                            .public_instruction(
                                &intent,
                                2,
                                PublicInstruction::ApplicationClose,
                                Some(PublicInstructionOutcome::Done),
                            )
                            .await?;
                        store
                            .public_instruction(&intent, 3, PublicInstruction::Cleanup, None)
                            .await?;
                    }
                }
                let _guard = terminal::TerminalRootGuard(callbacks.clone());
                program::execute(store, intent, callbacks).await
            })
            .await
            .unwrap();
        let record = completed(&operation).await;
        assert_eq!(record.stage, PublicOperationStage::Completed);
        if cut == 0 {
            assert_eq!(
                record.disposition,
                Some(PublicOperationDisposition::Succeeded)
            );
        } else {
            assert!(
                matches!(record.disposition, Some(PublicOperationDisposition::Failed(ref error)) if error.contains("retained first close failure"))
            );
            if cut == 3 {
                assert!(
                    matches!(record.disposition, Some(PublicOperationDisposition::Failed(ref error)) if error.contains("drain injected"))
                );
            }
            let events = fixture.probe.events.lock().unwrap().clone();
            assert!(
                !events
                    .iter()
                    .any(|event| event == "replicator.close" || event == "application.close")
            );
            assert_eq!(
                events,
                [
                    "replicator.abort",
                    "application.abort",
                    "drain.begin",
                    "drain.end"
                ]
            );
        }
        assert_eq!(fixture.probe.none_calls.load(Ordering::SeqCst), 0);
        fixture.registry.shutdown().await.unwrap();
    }
}
