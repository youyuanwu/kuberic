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

#[path = "public_action_state_tests.rs"]
mod action_state_tests;
#[path = "public_configuration_projection_tests.rs"]
mod configuration_projection_tests;
#[path = "public_remove_planner_tests.rs"]
mod remove_planner_tests;
#[path = "public_lifecycle_retry_tests.rs"]
mod retry_tests;
#[path = "public_swap_admission_tests.rs"]
mod swap_admission_tests;

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
    block_application_role: AtomicBool,
    application_role_entered: Notify,
    application_role_calls: AtomicUsize,
    fail_at_application_call: AtomicUsize,
    block_at_application_call: AtomicUsize,
    fail_configuration: AtomicBool,
    fail_catchup: AtomicBool,
    fail_epoch: AtomicBool,
    fail_build: AtomicBool,
    role_rebuild: Mutex<Option<ReplicaIdentity>>,
    application_rebuild: Mutex<Option<ReplicaIdentity>>,
    application_child: Mutex<Option<tokio::task::JoinHandle<()>>>,
    application_child_entered: Notify,
    application_child_release: Notify,
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

struct Application(Arc<Probe>, Arc<dyn Replicator>);
struct Replication(Arc<Probe>);

#[async_trait::async_trait]
impl StatefulServiceReplica for Application {
    async fn open(self: Arc<Self>, context: OpenContext) -> crate::Result<Arc<dyn Replicator>> {
        self.0.event(format!("application.open.{:?}", context.mode));
        Ok(self.1.clone())
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
        let call = self.0.application_role_calls.fetch_add(1, Ordering::AcqRel) + 1;
        self.0.application_role_entered.notify_one();
        if self.0.block_application_role.load(Ordering::Acquire)
            || self.0.block_at_application_call.load(Ordering::Acquire) == call
        {
            std::future::pending::<()>().await;
        }
        if let Some(replica) = self.0.application_rebuild.lock().unwrap().clone() {
            let owner = self.0.clone();
            let child = tokio::spawn(async move {
                owner.application_child_entered.notify_one();
                owner.application_child_release.notified().await;
            });
            *self.0.application_child.lock().unwrap() = Some(child);
            return Err(RuntimeError::ReplicaRebuildRequired(Box::new(
                ReplicaRebuildRequired::LocalProvider(
                    crate::protocol::public_operations::LocalProviderRebuildRequired {
                        reason: crate::protocol::public_operations::ReplicaRebuildReason::IncompatibleRetainedHistory,
                        provider: crate::protocol::public_operations::ProviderReplicaIdentity { replica },
                    },
                ),
            )));
        }
        if self.0.fail_application_role.load(Ordering::Acquire)
            || self.0.fail_at_application_call.load(Ordering::Acquire) == call
        {
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
        if let Some(replica) = self.0.role_rebuild.lock().unwrap().clone() {
            return Err(RuntimeError::ReplicaRebuildRequired(Box::new(
                ReplicaRebuildRequired::LocalProvider(
                    crate::protocol::public_operations::LocalProviderRebuildRequired {
                        reason: crate::protocol::public_operations::ReplicaRebuildReason::IncompatibleRetainedHistory,
                        provider: crate::protocol::public_operations::ProviderReplicaIdentity { replica },
                    },
                ),
            )));
        }
        if self.0.fail_role.load(Ordering::Acquire) {
            return Err(RuntimeError::Application("actual role injected".into()));
        }
        *self.0.role.lock().unwrap() = Some(role);
        Ok(())
    }
    async fn update_epoch(&self, _: Epoch) -> crate::Result<()> {
        self.0.event("epoch");
        if self.0.fail_epoch.load(Ordering::Acquire) {
            return Err(RuntimeError::Application("epoch injected".into()));
        }
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
        if self.0.fail_configuration.load(Ordering::Acquire) {
            return Err(RuntimeError::Application("configuration injected".into()));
        }
        Ok(())
    }
    async fn update_current_replica_set_configuration(
        &self,
        _: ReplicaSetConfiguration,
    ) -> crate::Result<()> {
        self.0.event("configuration");
        if self.0.fail_configuration.load(Ordering::Acquire) {
            return Err(RuntimeError::Application("configuration injected".into()));
        }
        Ok(())
    }
    async fn wait_for_catch_up_quorum(&self, _: ReplicaSetQuorumMode) -> crate::Result<()> {
        assert_eq!(*self.0.role.lock().unwrap(), Some(ReplicaRole::Primary));
        self.0.event("catchup");
        self.0.catchup_entered.notify_one();
        if self.0.block_catchup.load(Ordering::Acquire) {
            self.0.catchup_release.notified().await;
        }
        if self.0.fail_catchup.load(Ordering::Acquire) {
            return Err(RuntimeError::Application("catchup injected".into()));
        }
        Ok(())
    }
    async fn build_replica(&self, _: ReplicaInformation) -> crate::Result<()> {
        self.0.event("build");
        if self.0.fail_build.load(Ordering::Acquire) {
            return Err(RuntimeError::Application("build injected".into()));
        }
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
            native: None,
            peer_address: None,
            peer_credentials: None,
            ordinary: false,
            abort_creation: None,
            access_factory: None,
            access_owner: None,
            application: Arc::new(Application(probe.clone(), replication.clone())),
            replicator: replication.clone(),
            primary: replication,
            application_control_match: None,
            revoke_access: Arc::new(|_| Ok(())),
            capabilities: ReplicatorCapabilities::conservative(),
            replicator_creation_id: ReplicatorCreationId::new("creation"),
            open_context: None,
            containment: None,
            retry_proof: Default::default(),
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

    async fn bind_open_context(&mut self, mode: crate::application::OpenMode) -> ReplicaIdentity {
        let state = self.store.load_state().await.unwrap();
        let replica = state.identity.local_identity;
        let runtime = crate::host::hosting::PodRuntime::new_replacement(
            replica.clone(),
            Arc::new(Application(
                self.probe.clone(),
                self.callbacks.replicator.clone(),
            )),
            self.store.clone(),
            state.public_v1_operation_state.unwrap().binding.unwrap(),
            "127.0.0.1:0".parse().unwrap(),
        );
        self.callbacks.open_context = Some(runtime.public_open_context(mode).await);
        replica
    }
}

async fn completed(operation: &PartitionOperation) -> PublicOperationRecord {
    tokio::time::timeout(Duration::from_secs(5), operation.wait_for_terminal())
        .await
        .unwrap()
        .unwrap()
}

#[tokio::test]
async fn open_mode_disagreement_rejects_before_the_application_callback() {
    use crate::application::OpenMode;

    for mode in [OpenMode::New, OpenMode::Existing] {
        let mut fixture = Fixture::new().await;
        let replica = fixture.bind_open_context(mode).await;
        let operation = fixture
            .launch(
                PublicOperationProgram::Open {
                    replica,
                    existing: mode == OpenMode::New,
                },
                1,
            )
            .await;
        assert!(matches!(
            completed(&operation).await.disposition,
            Some(PublicOperationDisposition::Failed(detail))
                if detail.contains("open context changed")
        ));
        assert!(
            fixture
                .probe
                .events
                .lock()
                .unwrap()
                .iter()
                .all(|event| !event.starts_with("application.open."))
        );
        fixture.registry.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn completed_new_open_replays_after_the_bound_mode_changes_to_existing() {
    use crate::application::OpenMode;

    let mut fixture = Fixture::new().await;
    let replica = fixture.bind_open_context(OpenMode::New).await;
    let program = PublicOperationProgram::Open {
        replica,
        existing: false,
    };
    let operation = fixture.launch(program.clone(), 1).await;
    assert_eq!(
        completed(&operation).await.disposition,
        Some(PublicOperationDisposition::Succeeded)
    );
    fixture.callbacks.open_context.as_mut().unwrap().mode = OpenMode::Existing;
    let replay = fixture.launch(program, 1).await;
    assert_eq!(
        completed(&replay).await.disposition,
        Some(PublicOperationDisposition::Succeeded)
    );
    assert_eq!(
        fixture
            .probe
            .events
            .lock()
            .unwrap()
            .iter()
            .filter(|event| event.starts_with("application.open."))
            .collect::<Vec<_>>(),
        vec!["application.open.New"]
    );
    fixture.registry.shutdown().await.unwrap();
}

#[tokio::test]
async fn promoted_build_releases_its_slot_for_a_fresh_incarnation_without_losing_replay() {
    use crate::protocol::public_operations::{PublicOperationClass, PublicReplicaDescriptor};

    let fixture = Fixture::new().await;
    let source = fixture
        .store
        .load_state()
        .await
        .unwrap()
        .identity
        .local_identity;
    let initial = PublicLifecycleInput {
        recipe: PublicLifecycleRecipe::InitialPrimary,
        replica: source.clone(),
        epoch: Epoch::new(0, 1),
        possible_data_loss: PossibleDataLossIntent::NotPossible,
        current: ConfigurationDescriptor::new(
            Epoch::new(0, 1),
            source.replica_id,
            vec![ConfigurationMember {
                identity: source.clone(),
                role: ReplicaRole::Primary,
            }],
            1,
        ),
        previous: None,
        replicas: Vec::new(),
        data_loss_recovery: None,
    };
    let mut initial_intent = fixture.intent(
        PublicOperationProgram::Epoch {
            epoch: initial.epoch,
        },
        1,
    );
    initial_intent.program = None;
    initial_intent.input_digest = initial.digest();
    initial_intent.lifecycle = Some(initial);
    let authority = launch(
        &fixture.registry,
        fixture.store.clone(),
        initial_intent,
        fixture.callbacks.clone(),
    )
    .await
    .unwrap();
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
        source_address: "http://127.0.0.1:1".into(),
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
        is_up: true,
        is_ready: true,
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
                    identity: epoch_primary.clone(),
                    role: ReplicaRole::Primary,
                },
            ],
            1,
        ),
        previous: None,
        replicas: vec![
            descriptor(target_storage.clone()),
            PublicReplicaDescriptor {
                is_up: true,
                is_ready: true,
                identity: epoch_primary.clone(),
                replication_address: "http://127.0.0.1:4".into(),
                process_session_id: ProcessSessionId::new("epoch-primary-session"),
                storage_incarnation: Some(ReplicaStorageIncarnationContext {
                    replica: epoch_primary,
                    storage: ReplicaStorageIdentity::Persistent {
                        pod_uid: PodUid::new("epoch-primary"),
                        pvc_uid: PvcUid::new("epoch-primary-pvc"),
                    },
                    storage_generation: StorageGeneration::new("epoch-primary-storage"),
                }),
                role: ReplicaRole::Primary,
                current_progress: 0,
                catch_up_capability: 0,
                must_catchup: false,
            },
        ],
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
        let state = fixture.store.load_state().await.unwrap();
        let preview = state.public_v1_operation_state.as_ref().unwrap();
        let projected = preview.remote_build_states(&ProcessSessionId::new("session"));
        assert_eq!(
            projected
                .iter()
                .find(|remote| remote.build.operation_id == first_intent.operation_id)
                .unwrap()
                .state,
            if exact_storage {
                crate::host::state::PublicRemoteBuildState::Dropped
            } else {
                crate::host::state::PublicRemoteBuildState::Ready
            }
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
    let before = fixture.store.load_state().await.unwrap();
    assert!(
        fixture
            .registry
            .admit(build_intent(successor.clone(), 4))
            .await
            .is_err(),
        "a promoted logical replica must leave voting membership before its successor Build",
    );
    assert_eq!(fixture.store.load_state().await.unwrap(), before);
    let retained = ConfigurationDescriptor::new(
        Epoch::new(0, 5),
        source.replica_id,
        vec![ConfigurationMember {
            identity: source,
            role: ReplicaRole::Primary,
        }],
        1,
    );
    let released = fixture
        .launch(
            PublicOperationProgram::Configuration(PublicConfiguration {
                current: retained.clone(),
                previous: None,
                replicas: Vec::new(),
            }),
            5,
        )
        .await;
    assert_eq!(
        completed(&released).await.disposition,
        Some(PublicOperationDisposition::Succeeded)
    );
    let state = fixture.store.load_state().await.unwrap();
    let preview = state.public_v1_operation_state.as_ref().unwrap();
    assert!(
        preview.active_builds.contains_key(&ReplicaId::new(2)),
        "physical slot cleanup is lazy; it must not masquerade as idle ownership"
    );
    assert_eq!(
        preview.remote_build_states(&ProcessSessionId::new("session"))[0].state,
        crate::host::state::PublicRemoteBuildState::Dropped,
        "withdrawing a voter cannot resurrect its transferred Build as Ready"
    );
    successor.epoch = retained.epoch;
    successor.current_configuration_id = Some(retained.configuration_id);
    let next = launch(
        &fixture.registry,
        fixture.store.clone(),
        build_intent(successor, 5),
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
        assert!(
            matches!(&record.disposition, Some(PublicOperationDisposition::Failed(error))
            if error.contains("close injected"))
        );
        assert!(
            record
                .lifecycle
                .outcomes
                .iter()
                .any(|outcome| matches!(outcome, PublicInstructionOutcome::CloseFailure { child, error }
                    if *child == if replication { crate::host::state::PublicCloseChild::Replicator } else { crate::host::state::PublicCloseChild::Application }
                        && error.contains(if replication { "replicator close injected" } else { "application close injected" })))
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

fn tracked_listener_test_hooks(
    shutdown: tokio::sync::watch::Sender<bool>,
    task: tokio::task::JoinHandle<crate::Result<()>>,
) -> (TerminalDrain, TerminalDrain, Arc<dyn Fn() + Send + Sync>) {
    // Exercise the TaskTracker ownership used by the selected carrier, including
    // a child poll that cannot acknowledge Abort until it returns from native code.
    let tracker = tokio_util::task::TaskTracker::new();
    let outcome = Arc::new(Mutex::new(None::<std::result::Result<(), String>>));
    let completed = outcome.clone();
    let abort = task.abort_handle();
    tracker.spawn(async move {
        let result = match task.await {
            Ok(Ok(())) => Ok(()),
            Err(error) if error.is_cancelled() => Ok(()),
            Ok(Err(error)) => Err(error.to_string()),
            Err(error) => Err(error.to_string()),
        };
        *completed.lock().unwrap() = Some(result);
    });
    tracker.close();
    let close_tracker = tracker.clone();
    let close_shutdown = shutdown.clone();
    let close_abort = abort.clone();
    let close_outcome = outcome.clone();
    let close: TerminalDrain = Arc::new(move || {
        let tracker = close_tracker.clone();
        let shutdown = close_shutdown.clone();
        let abort = close_abort.clone();
        let outcome = close_outcome.clone();
        Box::pin(async move {
            shutdown.send_replace(true);
            if tokio::time::timeout(Duration::from_millis(30), tracker.wait())
                .await
                .is_err()
            {
                abort.abort();
                tracker.wait().await;
                return Err(RuntimeError::OperationCancelled);
            }
            outcome
                .lock()
                .unwrap()
                .as_ref()
                .expect("tracked listener terminal")
                .as_ref()
                .map_err(|error| RuntimeError::Application(error.clone()))
                .copied()
        })
    });
    let drain_tracker = tracker.clone();
    let drain_outcome = outcome.clone();
    let drain: TerminalDrain = Arc::new(move || {
        let tracker = drain_tracker.clone();
        let outcome = drain_outcome.clone();
        Box::pin(async move {
            tracker.wait().await;
            outcome
                .lock()
                .unwrap()
                .as_ref()
                .expect("tracked listener terminal")
                .as_ref()
                .map_err(|error| RuntimeError::Application(error.clone()))
                .copied()
        })
    });
    let abort_hook = Arc::new(move || {
        shutdown.send_replace(true);
        abort.abort();
    });
    (close, drain, abort_hook)
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
        let (close_hook, drain_hook, abort_hook) = tracked_listener_test_hooks(shutdown, task);
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
                matches!(&record.disposition, Some(PublicOperationDisposition::Failed(error))
                if error.contains("replicator close injected"))
            );
            assert!(record.lifecycle.outcomes.iter().any(|outcome| {
                matches!(outcome, PublicInstructionOutcome::CloseFailure { error, .. }
                    if error.contains("replicator close injected"))
            }));
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
async fn native_observer_between_receipted_callbacks_does_not_require_runtime_shutdown() {
    for (position, index, receipts) in [
        (PublicCutPosition::BeforeInstruction, 0, 0),
        (PublicCutPosition::BeforeInstruction, 1, 1),
        (PublicCutPosition::InstructionApplied, 0, 1),
        (PublicCutPosition::InstructionApplied, 1, 2),
    ] {
        let mut fixture = Fixture::new().await;
        let (_contained, witness) = tokio::sync::watch::channel(false);
        fixture.callbacks.containment = Some(witness);
        let cut = Arc::new(PublicOperationCut::new(position, index));
        fixture.callbacks.cut = Some(cut.clone());
        let operation = fixture
            .launch(
                PublicOperationProgram::Role {
                    epoch: Epoch::new(0, 2),
                    role: ReplicaRole::Primary,
                },
                1,
            )
            .await;
        tokio::time::timeout(Duration::from_secs(5), cut.entered.notified())
            .await
            .unwrap();
        let record = fixture
            .store
            .public_operation_records()
            .await
            .unwrap()
            .into_iter()
            .find(|record| record.intent == operation.intent())
            .unwrap();
        assert_eq!(record.lifecycle.outcomes.len(), receipts);
        assert!(record.lifecycle.in_flight.is_none());
        let cancelled = operation.cancel_root().await.unwrap();
        assert_eq!(cancelled.stage, PublicOperationStage::Completed);
        assert_eq!(
            cancelled.disposition,
            Some(PublicOperationDisposition::Cancelled)
        );
        fixture.callbacks.cut = None;
        let successor = fixture
            .launch(
                PublicOperationProgram::Epoch {
                    epoch: Epoch::new(0, 3),
                },
                2,
            )
            .await;
        assert_eq!(
            completed(&successor).await.disposition,
            Some(PublicOperationDisposition::Succeeded)
        );
        assert!(!*fixture.callbacks.containment.as_ref().unwrap().borrow());
        assert!(
            !fixture
                .probe
                .events
                .lock()
                .unwrap()
                .iter()
                .any(|event| event.starts_with("drain"))
        );
        fixture.registry.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn native_observer_selected_host_revoke_does_not_require_runtime_shutdown() {
    for composed in [false, true] {
        let (mut fixture, configuration) = swap_admission_tests::installed_primary().await;
        let (_contained, witness) = tokio::sync::watch::channel(false);
        fixture.callbacks.containment = Some(witness);
        let mut refreshed = configuration.clone();
        let epoch = Epoch::new(0, 4);
        refreshed.current = ConfigurationDescriptor::new(
            epoch,
            refreshed.current.primary_id,
            refreshed.current.members.clone(),
            refreshed.current.write_quorum,
        );
        let cut = Arc::new(PublicOperationCut::new(
            PublicCutPosition::InstructionSelected,
            if composed { 0 } else { 2 },
        ));
        fixture.callbacks.cut = Some(cut.clone());
        let operation = fixture
            .launch(
                PublicOperationProgram::Swap {
                    starting: configuration,
                    refreshed,
                    epoch,
                    handoff: ReplicaRole::ActiveSecondary,
                    catch_up_specific_quorum: false,
                    mode: PublicCatchUpMode::All,
                    controller_composed: composed,
                },
                3,
            )
            .await;
        tokio::time::timeout(Duration::from_secs(5), cut.entered.notified())
            .await
            .unwrap();
        let cancelled = operation.cancel_root().await.unwrap();
        assert_eq!(
            cancelled.lifecycle.in_flight,
            Some(PublicInstruction::Revoke)
        );
        assert_eq!(cancelled.stage, PublicOperationStage::Completed);
        assert_eq!(
            cancelled.disposition,
            Some(PublicOperationDisposition::Cancelled)
        );
        assert!(!*fixture.callbacks.containment.as_ref().unwrap().borrow());
        fixture.callbacks.cut = None;
        let successor = fixture
            .launch(
                PublicOperationProgram::Epoch {
                    epoch: Epoch::new(0, 5),
                },
                4,
            )
            .await;
        assert_eq!(
            completed(&successor).await.disposition,
            Some(PublicOperationDisposition::Succeeded)
        );
        fixture.registry.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn canonical_application_rebuild_receipt_keeps_real_child_containment_and_blocks_successor() {
    for recipe in [false, true] {
        let mut fixture = Fixture::new().await;
        // Match a live creation's retained witness: supersession must observe
        // its eventual actual join, rather than a one-off bare-fixture poll.
        let local = fixture
            .store
            .load_state()
            .await
            .unwrap()
            .identity
            .local_identity;
        *fixture.probe.application_rebuild.lock().unwrap() = Some(local.clone());
        let (contained, witness) = tokio::sync::watch::channel(false);
        fixture.callbacks.containment = Some(witness);
        let epoch = Epoch::new(0, 2);
        let mut intent = fixture.intent(
            PublicOperationProgram::Role {
                epoch,
                role: ReplicaRole::Primary,
            },
            1,
        );
        if recipe {
            let input = PublicLifecycleInput {
                recipe: PublicLifecycleRecipe::FailoverPromotion,
                replica: local.clone(),
                epoch,
                possible_data_loss: PossibleDataLossIntent::NotPossible,
                current: ConfigurationDescriptor::new(
                    epoch,
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
                    vec![ConfigurationMember {
                        identity: local,
                        role: ReplicaRole::Primary,
                    }],
                    1,
                )),
                replicas: Vec::new(),
                data_loss_recovery: None,
            };
            intent.program = None;
            intent.input_digest = input.digest();
            intent.lifecycle = Some(input);
        }
        let operation = launch(
            &fixture.registry,
            fixture.store.clone(),
            intent.clone(),
            fixture.callbacks.clone(),
        )
        .await
        .unwrap();
        fixture.probe.application_child_entered.notified().await;
        let child = fixture
            .probe
            .application_child
            .lock()
            .unwrap()
            .take()
            .unwrap();
        assert!(!child.is_finished());
        eprintln!("canonical application recipe={recipe}: waiting for failed callback");
        let record = completed(&operation).await;
        assert_eq!(record.stage, PublicOperationStage::ContainmentPending);
        assert!(matches!(
            record.disposition,
            Some(PublicOperationDisposition::Failed(_))
        ));
        assert!(
            record.lifecycle.in_flight.is_none(),
            "canonical failure is receipted, not a successful gap"
        );
        assert!(matches!(
            record.lifecycle.outcomes.last(),
            Some(PublicInstructionOutcome::RebuildRequired(_))
        ));
        assert_eq!(
            operation_instructions(&intent)[record.lifecycle.outcomes.len() - 1],
            if recipe {
                PublicInstruction::ApplicationPrimary
            } else {
                PublicInstruction::ApplicationRole
            },
        );
        assert!(record.retry_failure.is_none());
        assert!(operation.complete_containment().await.is_err());
        let calls = fixture.probe.application_role_calls.load(Ordering::Acquire);
        let replay = launch(
            &fixture.registry,
            fixture.store.clone(),
            intent,
            fixture.callbacks.clone(),
        )
        .await
        .unwrap();
        assert_eq!(replay.snapshot().attempt, record.attempt);
        assert_eq!(
            replay.snapshot().stage,
            PublicOperationStage::ContainmentPending
        );
        assert_eq!(
            fixture.probe.application_role_calls.load(Ordering::Acquire),
            calls
        );
        assert!(!child.is_finished());
        let successor = fixture
            .launch(
                PublicOperationProgram::Epoch {
                    epoch: Epoch::new(0, 3),
                },
                2,
            )
            .await;
        eprintln!(
            "canonical application recipe={recipe}: successor {:?}",
            successor.snapshot().stage
        );
        assert_eq!(
            successor.snapshot().stage,
            PublicOperationStage::WaitingForContainment
        );
        fixture.probe.application_child_release.notify_one();
        tokio::time::timeout(Duration::from_secs(5), child)
            .await
            .unwrap()
            .unwrap();
        // Only this actual application-child join supplies positive containment.
        contained.send_replace(true);
        operation.complete_containment().await.unwrap();
        eprintln!(
            "canonical application recipe={recipe}: joined child, old {:?}, successor {:?}",
            operation.snapshot().stage,
            successor.snapshot().stage
        );
        assert_eq!(
            completed(&successor).await.disposition,
            Some(PublicOperationDisposition::Succeeded)
        );
        assert_eq!(operation.snapshot().disposition, record.disposition);
        assert_eq!(
            fixture.probe.application_role_calls.load(Ordering::Acquire),
            calls
        );
        fixture.registry.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn native_engine_ownership_does_not_certify_failed_or_cancelled_application_role() {
    for cancel in [false, true] {
        let mut fixture = Fixture::new().await;
        fixture
            .probe
            .fail_application_role
            .store(!cancel, Ordering::Release);
        fixture
            .probe
            .block_application_role
            .store(cancel, Ordering::Release);
        let (contained, witness) = tokio::sync::watch::channel(false);
        fixture.callbacks.containment = Some(witness);
        let operation = fixture
            .launch(
                PublicOperationProgram::Role {
                    epoch: Epoch::new(0, 2),
                    role: ReplicaRole::Primary,
                },
                1,
            )
            .await;
        fixture.probe.application_role_entered.notified().await;
        if cancel {
            operation.cancel_root().await.unwrap();
        }
        let result = completed(&operation).await;
        assert_eq!(result.stage, PublicOperationStage::ContainmentPending);
        assert!(if cancel {
            result.disposition == Some(PublicOperationDisposition::Cancelled)
        } else {
            matches!(
                result.disposition,
                Some(PublicOperationDisposition::Failed(_))
            )
        });
        assert!(operation.complete_containment().await.is_err());
        let successor = fixture
            .launch(
                PublicOperationProgram::Epoch {
                    epoch: Epoch::new(0, 3),
                },
                2,
            )
            .await;
        assert_eq!(
            successor.snapshot().stage,
            PublicOperationStage::WaitingForContainment
        );
        contained.send_replace(true);
        operation.complete_containment().await.unwrap();
        assert_eq!(
            completed(&successor).await.disposition,
            Some(PublicOperationDisposition::Succeeded)
        );
        fixture.registry.shutdown().await.unwrap();
    }
}
#[tokio::test]
async fn native_engine_ownership_does_not_certify_access_application_callback() {
    for cancel in [false, true] {
        let mut fixture = Fixture::new().await;
        if cancel {
            fixture
                .probe
                .block_at_application_call
                .store(2, Ordering::Release);
        } else {
            fixture
                .probe
                .fail_at_application_call
                .store(2, Ordering::Release);
        }
        let (contained, witness) = tokio::sync::watch::channel(false);
        fixture.callbacks.containment = Some(witness);
        let local = fixture
            .store
            .load_state()
            .await
            .unwrap()
            .identity
            .local_identity;
        let input = PublicLifecycleInput {
            recipe: PublicLifecycleRecipe::FailoverPromotion,
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
                vec![ConfigurationMember {
                    identity: local,
                    role: ReplicaRole::Primary,
                }],
                1,
            )),
            replicas: Vec::new(),
            data_loss_recovery: None,
        };
        let mut intent = fixture.intent(PublicOperationProgram::Epoch { epoch: input.epoch }, 1);
        intent.program = None;
        intent.input_digest = input.digest();
        intent.lifecycle = Some(input);
        let operation = launch(
            &fixture.registry,
            fixture.store.clone(),
            intent.clone(),
            fixture.callbacks.clone(),
        )
        .await
        .unwrap();
        while fixture.probe.application_role_calls.load(Ordering::Acquire) < 2 {
            fixture.probe.application_role_entered.notified().await;
        }
        if cancel {
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
                Some(PublicInstruction::Access)
            );
            operation.cancel_root().await.unwrap();
        }
        let record = completed(&operation).await;
        assert_eq!(record.stage, PublicOperationStage::ContainmentPending);
        assert_eq!(record.lifecycle.in_flight, Some(PublicInstruction::Access));
        assert!(operation.complete_containment().await.is_err());
        let successor = fixture
            .launch(
                PublicOperationProgram::Epoch {
                    epoch: Epoch::new(0, 3),
                },
                2,
            )
            .await;
        assert_eq!(
            successor.snapshot().stage,
            PublicOperationStage::WaitingForContainment
        );
        contained.send_replace(true);
        operation.complete_containment().await.unwrap();
        assert_eq!(
            completed(&successor).await.disposition,
            Some(PublicOperationDisposition::Succeeded)
        );
        fixture.registry.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn sf06_terminal_custom_witness_blocks_completion_and_lost_witness_is_not_containment() {
    for lose_witness in [false, true] {
        let mut fixture = Fixture::new().await;
        let (sender, receiver) = tokio::sync::watch::channel(false);
        fixture.callbacks.containment = Some(receiver);
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
            .spawn_root(async move {
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
        assert_eq!(
            record.stage,
            if cut == 3 {
                PublicOperationStage::ContainmentPending
            } else {
                PublicOperationStage::Completed
            }
        );
        if cut == 0 {
            assert_eq!(
                record.disposition,
                Some(PublicOperationDisposition::Succeeded)
            );
        } else {
            if cut == 3 {
                assert!(
                    matches!(record.disposition, Some(PublicOperationDisposition::Failed(ref error))
                        if error.contains("drain injected") && error.contains("retained first close failure"))
                );
            } else {
                assert!(
                    matches!(&record.disposition, Some(PublicOperationDisposition::Failed(error))
                    if error.contains("retained first close failure"))
                );
            }
            assert!(record.lifecycle.outcomes.iter().any(|outcome| {
                matches!(outcome, PublicInstructionOutcome::CloseFailure { error, .. }
                    if error == "retained first close failure")
            }));
            assert_eq!(
                *fixture.probe.events.lock().unwrap(),
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

impl Fixture {
    async fn interrupt_close_at(
        &self,
        position: PublicCutPosition,
        index: usize,
    ) -> PublicOperationIntent {
        self.interrupt_terminal_at(PublicOperationProgram::Close, position, index)
            .await
    }

    async fn interrupt_terminal_at(
        &self,
        program: PublicOperationProgram,
        position: PublicCutPosition,
        index: usize,
    ) -> PublicOperationIntent {
        let intent = self.intent(program, 1);
        self.registry.admit(intent.clone()).await.unwrap();
        self.store
            .advance_public_operation(
                &intent.operation_id,
                intent.revision,
                &intent.process_session_id,
                PublicOperationStage::Ready,
                PublicOperationStage::Running,
                None,
            )
            .await
            .unwrap();
        let cut = Arc::new(PublicOperationCut::new(position, index));
        let mut callbacks = self.callbacks.clone();
        callbacks.aborted = self.registry.abort_guard();
        callbacks.cut = Some(cut.clone());
        let (cleanup, owner) = terminal::bind(callbacks.clone());
        callbacks.terminal_cleanup = Some(cleanup);
        let owner = tokio::spawn(owner);
        let store = self.store.clone();
        let captured = intent.clone();
        // Lose process-local owners without publishing cancellation, at the
        // actual instruction/receipt boundary exercised by the executor.
        let task = tokio::spawn(async move { program::execute(store, captured, callbacks).await });
        tokio::time::timeout(Duration::from_secs(5), cut.entered.notified())
            .await
            .unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        owner.abort();
        let _ = owner.await;
        intent
    }

    fn replace_registry(&mut self, predecessor: bool, bind_callbacks: bool) {
        self.registry = PartitionOperationRegistry::new(
            self.store.clone(),
            PublicV1Identity::new(6),
            ProcessSessionId::new(if predecessor {
                "successor-session"
            } else {
                "session"
            }),
        )
        .unwrap();
        if bind_callbacks {
            let callbacks = self.callbacks.clone();
            self.registry
                .bind_terminal_recovery(move || Ok(callbacks.clone()))
                .unwrap();
        }
    }

    async fn retained_close(&self) -> PublicOperationRecord {
        self.store
            .public_operation_records()
            .await
            .unwrap()
            .into_iter()
            .find(|record| record.intent.program == Some(PublicOperationProgram::Close))
            .unwrap()
    }
}

#[tokio::test]
async fn sf06_interrupted_destructive_close_cuts_are_ambiguous_and_owned_not_repeated() {
    for predecessor in [false, true] {
        for index in [1, 2] {
            for position in [
                PublicCutPosition::InstructionSelected,
                PublicCutPosition::CallbackSuccess,
            ] {
                let mut fixture = Fixture::new().await;
                let intent = fixture.interrupt_close_at(position, index).await;
                let events = fixture.probe.events.lock().unwrap().clone();
                assert_eq!(
                    events
                        .iter()
                        .filter(|event| *event == "replicator.close")
                        .count(),
                    usize::from(index == 2 || position == PublicCutPosition::CallbackSuccess)
                );
                assert_eq!(
                    events
                        .iter()
                        .filter(|event| *event == "application.close")
                        .count(),
                    usize::from(index == 2 && position == PublicCutPosition::CallbackSuccess)
                );
                fixture.probe.block_drain.store(true, Ordering::Release);
                fixture.replace_registry(predecessor, true);
                fixture.registry.recover_unowned().await.unwrap();
                fixture.probe.drain_entered.notified().await;
                let pending = fixture.retained_close().await;
                assert_eq!(pending.stage, PublicOperationStage::ContainmentPending);
                assert!(matches!(
                    pending.disposition,
                    Some(PublicOperationDisposition::Ambiguous(_))
                ));
                if !predecessor {
                    let replay = fixture.launch(PublicOperationProgram::Close, 1).await;
                    assert!(replay.complete_containment().await.is_err());
                } else {
                    assert!(fixture.registry.admit(intent).await.is_err());
                }
                let during = fixture.probe.events.lock().unwrap().clone();
                for child in ["replicator.close", "application.close"] {
                    assert_eq!(
                        events
                            .iter()
                            .filter(|event| event.as_str() == child)
                            .count(),
                        during
                            .iter()
                            .filter(|event| event.as_str() == child)
                            .count()
                    );
                }
                assert_eq!(
                    during
                        .iter()
                        .filter(|event| *event == "application.abort")
                        .count(),
                    1
                );
                fixture.probe.drain_release.notify_one();
                tokio::time::timeout(Duration::from_secs(5), async {
                    while fixture.retained_close().await.stage != PublicOperationStage::Completed {
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .unwrap();
                assert_eq!(
                    fixture.retained_close().await.disposition,
                    pending.disposition
                );
                if !predecessor {
                    let replay = fixture.launch(PublicOperationProgram::Close, 1).await;
                    assert_eq!(completed(&replay).await, fixture.retained_close().await);
                    assert_eq!(fixture.probe.events.lock().unwrap().len(), during.len() + 1);
                }
                fixture.registry.shutdown().await.unwrap();
            }
        }
    }
}

#[tokio::test]
async fn sf06_recorded_close_failure_and_cleanup_receipts_rejoin_current_process_owner() {
    for index in [1, 2, 3] {
        let mut fixture = Fixture::new().await;
        fixture
            .probe
            .fail_replicator_close
            .store(index == 1, Ordering::Release);
        fixture
            .probe
            .fail_application_close
            .store(index != 1, Ordering::Release);
        fixture
            .interrupt_close_at(PublicCutPosition::InstructionApplied, index)
            .await;
        // Consume the original owner's notification at the Cleanup receipt cut.
        if index == 3 {
            fixture.probe.drain_entered.notified().await;
        }
        let before = fixture.probe.events.lock().unwrap().clone();
        fixture.probe.block_drain.store(true, Ordering::Release);
        fixture.replace_registry(false, true);
        fixture.registry.recover_unowned().await.unwrap();
        let replay = fixture.launch(PublicOperationProgram::Close, 1).await;
        fixture.probe.drain_entered.notified().await;
        assert_eq!(replay.snapshot().stage, PublicOperationStage::Running);
        assert!(
            tokio::time::timeout(Duration::from_millis(30), replay.wait_for_terminal())
                .await
                .is_err()
        );
        fixture.probe.drain_release.notify_one();
        let record = completed(&replay).await;
        assert!(
            matches!(&record.disposition, Some(PublicOperationDisposition::Failed(error))
            if error.contains("close injected"))
        );
        assert!(record.lifecycle.outcomes.iter().any(|outcome| {
            matches!(outcome, PublicInstructionOutcome::CloseFailure { error, .. }
                                    if error.contains("close injected"))
        }));
        let after = fixture.probe.events.lock().unwrap().clone();
        for child in ["replicator.close", "application.close"] {
            assert_eq!(
                before
                    .iter()
                    .filter(|event| event.as_str() == child)
                    .count(),
                after.iter().filter(|event| event.as_str() == child).count()
            );
        }
        fixture.registry.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn sf06_recovery_missing_owner_cleanup_error_and_lost_witness_never_settle() {
    for failure in ["drain", "witness"] {
        let mut fixture = Fixture::new().await;
        let intent = fixture
            .interrupt_close_at(PublicCutPosition::InstructionSelected, 1)
            .await;
        fixture.replace_registry(false, false);
        fixture.registry.recover_unowned().await.unwrap();
        let operation = fixture.registry.admit(intent).await.unwrap();
        let pending = operation.snapshot();
        assert_eq!(pending.stage, PublicOperationStage::ContainmentPending);
        assert!(operation.complete_containment().await.is_err());
        if failure == "drain" {
            fixture.probe.fail_drain.store(true, Ordering::Release);
        } else {
            let (sender, receiver) = tokio::sync::watch::channel(false);
            fixture.callbacks.containment = Some(receiver);
            drop(sender);
        }
        let callbacks = fixture.callbacks.clone();
        fixture
            .registry
            .bind_terminal_recovery(move || Ok(callbacks.clone()))
            .unwrap();
        fixture.registry.recover_unowned().await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while operation.retained_failure().is_none() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(
            operation
                .retained_failure()
                .unwrap()
                .contains(if failure == "drain" {
                    "drain injected"
                } else {
                    "witness lost"
                })
        );
        assert_eq!(operation.snapshot().disposition, pending.disposition);
        assert!(operation.complete_containment().await.is_err());
        fixture.registry.shutdown().await.unwrap();
        assert_eq!(
            operation.snapshot().stage,
            PublicOperationStage::ContainmentPending
        );
    }
}

#[tokio::test]
async fn sf06_shutdown_joins_terminal_owners_first_discovered_by_final_recovery() {
    for failure in ["none", "drain", "witness"] {
        let mut fixture = Fixture::new().await;
        fixture
            .interrupt_close_at(PublicCutPosition::InstructionSelected, 1)
            .await;
        fixture.probe.block_drain.store(true, Ordering::Release);
        if failure == "drain" {
            fixture.probe.fail_drain.store(true, Ordering::Release);
        } else if failure == "witness" {
            let (sender, receiver) = tokio::sync::watch::channel(false);
            fixture.callbacks.containment = Some(receiver);
            drop(sender);
        }
        fixture.replace_registry(false, true);
        let registry = fixture.registry.clone();
        let shutdown = tokio::spawn(async move { registry.shutdown().await });
        tokio::time::timeout(
            Duration::from_secs(5),
            fixture.probe.drain_entered.notified(),
        )
        .await
        .unwrap();
        assert!(!shutdown.is_finished());
        fixture.probe.drain_release.notify_one();
        tokio::time::timeout(Duration::from_secs(5), shutdown)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let record = fixture.retained_close().await;
        assert!(matches!(
            record.disposition,
            Some(PublicOperationDisposition::Ambiguous(_))
        ));
        assert_eq!(
            record.stage,
            if failure == "none" {
                PublicOperationStage::Completed
            } else {
                PublicOperationStage::ContainmentPending
            }
        );
    }
}

#[tokio::test]
async fn sf06_cancelled_close_retains_cleanup_error_separately_from_cancellation() {
    let fixture = Fixture::new().await;
    fixture
        .probe
        .block_application_close
        .store(true, Ordering::Release);
    fixture.probe.block_drain.store(true, Ordering::Release);
    fixture.probe.fail_drain.store(true, Ordering::Release);
    let operation = fixture.launch(PublicOperationProgram::Close, 1).await;
    fixture.probe.close_entered.notified().await;
    operation.cancel_root().await.unwrap();
    fixture.probe.drain_entered.notified().await;
    assert_eq!(
        operation.snapshot().disposition,
        Some(PublicOperationDisposition::Cancelled)
    );
    assert!(operation.complete_containment().await.is_err());
    fixture.probe.drain_release.notify_one();
    tokio::time::timeout(Duration::from_secs(5), async {
        while operation.retained_failure().is_none() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(
        operation
            .retained_failure()
            .unwrap()
            .contains("drain injected")
    );
    assert_eq!(
        operation.snapshot().disposition,
        Some(PublicOperationDisposition::Cancelled)
    );
    fixture.registry.shutdown().await.unwrap();
    assert_ne!(
        operation.snapshot().disposition,
        Some(PublicOperationDisposition::Succeeded)
    );
}

#[tokio::test]
async fn sf06_recorded_abort_cleanup_receipt_reestablishes_application_abort_and_join() {
    let mut fixture = Fixture::new().await;
    fixture
        .interrupt_terminal_at(
            PublicOperationProgram::Abort,
            PublicCutPosition::InstructionApplied,
            2,
        )
        .await;
    fixture.probe.drain_entered.notified().await;
    fixture.probe.block_drain.store(true, Ordering::Release);
    fixture.replace_registry(false, true);
    fixture.registry.recover_unowned().await.unwrap();
    let operation = fixture.launch(PublicOperationProgram::Abort, 1).await;
    fixture.probe.drain_entered.notified().await;
    assert_eq!(operation.snapshot().stage, PublicOperationStage::Running);
    assert_eq!(
        fixture
            .probe
            .events
            .lock()
            .unwrap()
            .iter()
            .filter(|event| *event == "application.abort")
            .count(),
        2
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(30), operation.wait_for_terminal())
            .await
            .is_err()
    );
    fixture.probe.drain_release.notify_one();
    assert_eq!(
        completed(&operation).await.disposition,
        Some(PublicOperationDisposition::Succeeded)
    );
    fixture.registry.shutdown().await.unwrap();
}
