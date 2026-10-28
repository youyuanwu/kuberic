use super::*;
use crate::host::public_lifecycle::{PublicCutPosition, PublicOperationCut};
use crate::host::state::{PublicInstruction as I, PublicInstructionOutcome as O};

pub(super) struct NativeFactory(pub(super) Arc<Probe>);

#[async_trait::async_trait]
impl ReplicatorFactory for NativeFactory {
    async fn create_replicator(
        &self,
        context: ReplicatorFactoryContext,
        provider: Option<Arc<dyn StateProvider>>,
        settings: ReplicatorSettings,
    ) -> crate::Result<ReplicatorInterfaces> {
        *self.0.context.lock().unwrap() = Some(context.clone());
        let interfaces = DefaultReplicatorFactory::new()
            .create_replicator(context, provider, settings)
            .await?;
        let writer = interfaces.state_replicator();
        *self.0.native_primary.lock().unwrap() = interfaces.primary_replicator();
        let recording = Arc::new(NativeRecording {
            primary: interfaces.primary_replicator().unwrap(),
            probe: self.0.clone(),
        });
        Ok(interfaces.wrap_primary(recording, writer))
    }
}

struct NativeRecording {
    primary: Arc<dyn PrimaryReplicator>,
    probe: Arc<Probe>,
}

#[path = "private_effects_tests.rs"]
mod private_effects;

impl NativeRecording {
    fn event(&self, name: &str) {
        self.probe.native_events.lock().unwrap().push(name.into());
    }
}

#[async_trait::async_trait]
impl Replicator for NativeRecording {
    async fn open(&self) -> crate::Result<String> {
        self.event("native.open");
        self.primary.open().await
    }
    async fn change_role(&self, epoch: Epoch, role: ReplicaRole) -> crate::Result<()> {
        self.event(&format!("native.role:{role:?}"));
        self.primary.change_role(epoch, role).await
    }
    async fn update_epoch(&self, epoch: Epoch) -> crate::Result<()> {
        self.event("native.epoch");
        self.primary.update_epoch(epoch).await
    }
    async fn current_progress(&self) -> crate::Result<i64> {
        self.event("native.progress");
        self.primary.current_progress().await
    }
    async fn catch_up_capability(&self) -> crate::Result<i64> {
        self.event("native.capability");
        self.primary.catch_up_capability().await
    }
    async fn close(&self) -> crate::Result<()> {
        self.primary.close().await
    }
    fn abort(&self) {
        self.primary.abort();
    }
}

#[async_trait::async_trait]
impl PrimaryReplicator for NativeRecording {
    async fn on_data_loss(&self) -> crate::Result<bool> {
        self.event("native.data_loss");
        self.primary.on_data_loss().await
    }
    async fn update_catch_up_replica_set_configuration(
        &self,
        current: ReplicaSetConfiguration,
        previous: ReplicaSetConfiguration,
    ) -> crate::Result<()> {
        self.event("native.configuration");
        self.primary
            .update_catch_up_replica_set_configuration(current, previous)
            .await
    }
    async fn wait_for_catch_up_quorum(&self, mode: ReplicaSetQuorumMode) -> crate::Result<()> {
        self.event("native.catchup");
        self.primary.wait_for_catch_up_quorum(mode).await
    }
    async fn update_current_replica_set_configuration(
        &self,
        current: ReplicaSetConfiguration,
    ) -> crate::Result<()> {
        self.event("native.current");
        self.primary
            .update_current_replica_set_configuration(current)
            .await
    }
    async fn build_replica(&self, replica: ReplicaInformation) -> crate::Result<()> {
        self.event("native.build");
        self.primary.build_replica(replica).await
    }
    async fn remove_replica(&self, id: ReplicaId) -> crate::Result<()> {
        self.primary.remove_replica(id).await
    }
}

fn query(fixture: &Fixture, revision: u64, fence: i64) -> ReplicaGoal {
    let mut goal = fixture.goal.clone();
    goal.operation_id = OperationId::new(format!("get-lsn-{revision}"));
    goal.revision = revision;
    goal.scope.epoch = Epoch::new(0, fence);
    goal.scope.configuration.current.epoch = goal.scope.epoch;
    goal.scope.configuration.current.configuration_id =
        goal.scope.configuration.current.expected_id();
    goal.scope.configuration.replicas.clear();
    goal.desired = ReplicaGoalKind::Discover;
    goal
}

async fn native_fixture() -> Fixture {
    let fixture = Fixture::new(1, false).await;
    fixture
        .probe
        .use_default_factory
        .store(true, Ordering::Release);
    fixture
}

async fn reopen_joined_native(old: Fixture) -> Fixture {
    let old_facts = old.snapshot().await.native_lifecycle.unwrap();
    old.runtime.withdraw_ordinary();
    old.shutdown.send_replace(true);
    tokio::time::timeout(Duration::from_secs(10), old.server)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    old.runtime.shutdown_and_drain().await.unwrap();
    let binding = PublicV1LifecycleBinding {
        identity: old.goal.identity.clone(),
        resource_uid: old.goal.scope.history.resource_uid.clone(),
        spec_generation: 1,
        state_persistence: StatePersistence::Persisted,
    };
    let runtime = Arc::new(PodRuntime::new_replacement(
        old.goal.scope.storage.replica.clone(),
        Arc::new(Application(old.probe.clone())),
        old.store.clone(),
        binding,
        "127.0.0.1:0".parse().unwrap(),
    ));
    runtime
        .configure_native_transport(Arc::from("ordinary-token"))
        .unwrap();
    runtime
        .configure_discovery_resolver(old.resolver.clone())
        .unwrap();
    let service = AgentService::new(old.store.clone(), runtime.clone(), "ordinary-token").unwrap();
    runtime
        .bind_replica_session(
            old.goal.scope.history.resource_uid.clone(),
            service.local_session().clone(),
        )
        .unwrap();
    service
        .public_operation_runtime(old.goal.identity.clone())
        .await
        .unwrap();
    assert!(!runtime.native_facts_are_live(&old_facts));
    let mut goal = old.goal.clone();
    goal.scope.process_session_id = service.local_session().clone();
    goal.scope.creation_id = runtime.replacement_replicator_creation_id().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = format!("http://{}", listener.local_addr().unwrap());
    old.resolver
        .0
        .lock()
        .unwrap()
        .insert(goal.scope.storage.replica.replica_id, address.clone());
    let (shutdown, shutdown_rx) = watch::channel(false);
    let (ready, mut ready_rx) = watch::channel(false);
    let server_service = service.clone();
    let server = tokio::spawn(async move {
        server_service
            .serve_control_only(listener, ready, shutdown_rx)
            .await
    });
    ready_rx.wait_for(|ready| *ready).await.unwrap();
    Fixture {
        _root: old._root,
        store: old.store,
        runtime,
        service,
        probe: old.probe,
        goal,
        address,
        shutdown,
        server,
        resolver: old.resolver,
    }
}

#[tokio::test]
async fn native_existing_open_inspects_provider_prefix_without_secondary_or_admission() {
    let fixture = native_fixture().await;
    fixture.probe.provider_progress.store(17, Ordering::Release);
    // This is an actual retained-mode Open, not a constructor Ready flag.
    fixture
        .store
        .complete_application_initialization()
        .await
        .unwrap();
    let goal = query(&fixture, 1, 1);
    fixture.submit(&goal).await.unwrap();
    let snapshot = fixture.achieved(&goal).await;
    let status = snapshot.native_status.as_ref().unwrap();
    assert_eq!(status.input.state, NativeReplicaState::StandBy);
    assert_eq!(status.input.native_role, ReplicaRole::None);
    assert_eq!(status.input.deactivation, None);
    assert_eq!(status.epoch, NativeInspectionEpoch::StandBy);
    assert_eq!(status.progress, ProgressQueryResult::Value(17));
    assert_eq!(status.capability, ProgressQueryResult::Value(0));
    assert_eq!(
        snapshot.publication.as_ref().unwrap().stage,
        ReplicaPublicationStage::DiscoveryPending
    );
    assert_eq!(
        snapshot.publication.as_ref().unwrap().role,
        ReplicaRole::None
    );
    assert!(snapshot.native_lifecycle.is_none());
    assert!(snapshot.remote_operations.is_empty());
    assert_ne!(snapshot.read_status, AccessStatus::Granted);
    assert_ne!(snapshot.write_status, AccessStatus::Granted);
    assert_eq!(
        *fixture.probe.native_events.lock().unwrap(),
        ["native.open", "native.progress", "provider.progress"]
    );
    let before = fixture.probe.native_events.lock().unwrap().clone();
    for _ in 0..3 {
        fixture.snapshot().await;
        fixture.snapshot_sampling(true).await;
    }
    assert_eq!(*fixture.probe.native_events.lock().unwrap(), before);
    assert!(
        fixture
            .probe
            .native_writer
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .replication_operations()
            .is_err()
    );
    let state = fixture.store.load_state().await.unwrap();
    let record = &state.public_v1_operation_state.as_ref().unwrap().operations[&goal.operation_id];
    assert_eq!(
        record
            .ordinary
            .as_ref()
            .unwrap()
            .native_input
            .as_ref()
            .unwrap()
            .lifecycle,
        status.input
    );
    assert_eq!(
        record.ordinary.as_ref().unwrap().actions,
        [I::ApplicationOpen, I::ReplicatorOpen, I::NativeInspect]
    );
    fixture.finish().await;
}

#[tokio::test]
async fn native_initial_primary_decision_not_constructor_produces_ready_and_fresh_history() {
    let fixture = native_fixture().await;
    let actions =
        crate::host::public_lifecycle::program::operation_instructions(&fixture.goal.intent());
    let decision = actions
        .iter()
        .position(|instruction| *instruction == I::NativeDataLossDecision)
        .unwrap();
    let cut = Arc::new(PublicOperationCut::for_instruction(
        &fixture.goal,
        "NativeDataLossDecision",
        PublicCutPosition::InstructionSelected,
        decision,
    ));
    fixture.runtime.set_public_operation_cut(Some(cut.clone()));
    fixture.submit(&fixture.goal).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), cut.entered.notified())
        .await
        .unwrap();
    let state = fixture.store.load_state().await.unwrap();
    let record =
        &state.public_v1_operation_state.as_ref().unwrap().operations[&fixture.goal.operation_id];
    assert_eq!(record.lifecycle.in_flight, Some(I::NativeDataLossDecision));
    assert!(
        record
            .lifecycle
            .outcomes
            .iter()
            .any(|outcome| matches!(outcome, O::ApplicationRole(_)))
    );
    assert!(fixture.snapshot().await.native_lifecycle.is_none());
    assert_eq!(
        fixture
            .probe
            .provider_data_loss_calls
            .load(Ordering::Acquire),
        0
    );
    let mut requires_data_loss = fixture.goal.clone();
    let mut conflicting_pc = requires_data_loss.scope.configuration.current.clone();
    conflicting_pc.epoch = Epoch::new(1, 0);
    requires_data_loss.scope.configuration.previous = Some(conflicting_pc);
    let callbacks = fixture.runtime.public_lifecycle_callbacks().unwrap();
    let error = crate::host::native_history::decision(&requires_data_loss, &callbacks).unwrap_err();
    assert!(error.to_string().contains("explicit OnDataLoss"));
    assert!(fixture.snapshot().await.native_lifecycle.is_none());
    cut.release_all();
    let snapshot = fixture.achieved(&fixture.goal).await;
    let facts = snapshot.native_lifecycle.as_ref().unwrap();
    assert_eq!(facts.state, NativeReplicaState::Ready);
    assert_eq!(facts.application_role, Some(ReplicaRole::Primary));
    assert_eq!(facts.applied_primary_fence, Some(Epoch::new(0, 1)));
    assert_eq!(
        facts.deactivation,
        Some(NativeDeactivationInfo {
            primary_fence: Epoch::new(0, 1),
            catch_up_lsn: 0
        })
    );
    assert!(
        !fixture
            .probe
            .native_events
            .lock()
            .unwrap()
            .iter()
            .any(|event| event == "native.data_loss")
    );
    assert_eq!(
        fixture
            .probe
            .provider_data_loss_calls
            .load(Ordering::Acquire),
        0
    );
    let before = fixture.probe.native_events.lock().unwrap().clone();
    fixture.snapshot().await;
    assert_eq!(*fixture.probe.native_events.lock().unwrap(), before);
    fixture.finish().await;
}

#[tokio::test]
async fn native_warm_query_owns_epoch_status_order_and_skips_only_actual_applied_fence() {
    let fixture = native_fixture().await;
    fixture.submit(&fixture.goal).await.unwrap();
    fixture.achieved(&fixture.goal).await;
    fixture.probe.native_events.lock().unwrap().clear();
    let first = query(&fixture, 2, 2);
    fixture.submit(&first).await.unwrap();
    let snapshot = fixture.achieved(&first).await;
    let status = snapshot.native_status.unwrap();
    assert_eq!(status.input.state, NativeReplicaState::Ready);
    assert_eq!(status.input.applied_primary_fence, Some(Epoch::new(0, 1)));
    assert_eq!(status.epoch, NativeInspectionEpoch::Applied);
    let events = fixture.probe.native_events.lock().unwrap().clone();
    assert_eq!(
        events,
        [
            "native.epoch",
            "provider.epoch:Epoch { data_loss_number: 0, configuration_number: 2 }",
            "native.progress",
            "native.capability"
        ]
    );
    fixture.probe.native_events.lock().unwrap().clear();
    let second = query(&fixture, 3, 2);
    fixture.submit(&second).await.unwrap();
    let snapshot = fixture.achieved(&second).await;
    assert_eq!(
        snapshot.native_status.unwrap().epoch,
        NativeInspectionEpoch::AlreadyApplied
    );
    assert_eq!(
        *fixture.probe.native_events.lock().unwrap(),
        ["native.progress", "native.capability"]
    );
    assert_eq!(snapshot.publication.unwrap().role, ReplicaRole::Primary);
    fixture.probe.native_events.lock().unwrap().clear();
    let third = query(&fixture, 4, 3);
    fixture.submit(&third).await.unwrap();
    let status = fixture.achieved(&third).await.native_status.unwrap();
    assert_eq!(status.input.applied_primary_fence, Some(Epoch::new(0, 2)));
    assert_eq!(status.epoch, NativeInspectionEpoch::Applied);
    assert!(
        fixture
            .probe
            .native_events
            .lock()
            .unwrap()
            .iter()
            .any(|event| {
                event == "provider.epoch:Epoch { data_loss_number: 0, configuration_number: 3 }"
            })
    );
    fixture.finish().await;
}

#[tokio::test]
async fn native_failed_epoch_retains_failure_without_progress_or_provider_fallback() {
    let fixture = native_fixture().await;
    fixture.submit(&fixture.goal).await.unwrap();
    fixture.achieved(&fixture.goal).await;
    fixture.probe.native_events.lock().unwrap().clear();
    fixture
        .probe
        .fail_provider_epoch
        .store(true, Ordering::Release);
    let goal = query(&fixture, 2, 2);
    fixture.submit(&goal).await.unwrap();
    let snapshot = fixture.failed(&goal).await;
    assert!(snapshot.native_status.is_none());
    let state = fixture.store.load_state().await.unwrap();
    let record = &state.public_v1_operation_state.as_ref().unwrap().operations[&goal.operation_id];
    let O::NativeStatus(status) = record.lifecycle.outcomes.last().unwrap() else {
        panic!("actual failed result must persist")
    };
    assert!(matches!(status.epoch, NativeInspectionEpoch::Failed(_)));
    assert!(matches!(
        status.progress,
        ProgressQueryResult::NotQueried(_)
    ));
    assert!(matches!(
        status.capability,
        ProgressQueryResult::NotQueried(_)
    ));
    let events = fixture.probe.native_events.lock().unwrap().clone();
    assert!(!events.iter().any(|event| event == "native.progress"
        || event == "native.capability"
        || event == "provider.progress"));
    assert!(fixture.submit(&goal).await.is_err());
    assert_eq!(fixture.store.load_state().await.unwrap(), state);
    fixture.finish().await;
}

#[tokio::test]
async fn native_opened_status_failure_never_reads_capability_or_invents_a_prefix() {
    let fixture = native_fixture().await;
    fixture
        .store
        .complete_application_initialization()
        .await
        .unwrap();
    fixture
        .probe
        .fail_provider_progress
        .store(true, Ordering::Release);
    let goal = query(&fixture, 1, 1);
    fixture.submit(&goal).await.unwrap();
    fixture.failed(&goal).await;
    let state = fixture.store.load_state().await.unwrap();
    let record = &state.public_v1_operation_state.as_ref().unwrap().operations[&goal.operation_id];
    let O::NativeStatus(status) = record.lifecycle.outcomes.last().unwrap() else {
        panic!("failed actual status")
    };
    assert!(matches!(status.progress, ProgressQueryResult::Failed(_)));
    assert!(matches!(
        status.capability,
        ProgressQueryResult::NotQueried(_)
    ));
    assert_eq!(
        *fixture.probe.native_events.lock().unwrap(),
        ["native.open", "native.progress", "provider.progress"]
    );
    assert!(fixture.snapshot().await.native_status.is_none());
    fixture.finish().await;
}

#[tokio::test]
async fn native_selected_query_rejects_crossed_round_history_configuration_and_owner_before_effect()
{
    let fixture = native_fixture().await;
    let goal = query(&fixture, 1, 1);
    let cut = Arc::new(PublicOperationCut::for_instruction(
        &goal,
        "NativeInspect",
        PublicCutPosition::InstructionSelected,
        2,
    ));
    fixture.runtime.set_public_operation_cut(Some(cut.clone()));
    fixture.submit(&goal).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), cut.entered.notified())
        .await
        .unwrap();
    let before = fixture.store.load_state().await.unwrap();
    let input = before
        .public_v1_operation_state
        .as_ref()
        .unwrap()
        .operations[&goal.operation_id]
        .ordinary
        .as_ref()
        .unwrap()
        .native_input
        .clone()
        .unwrap();
    for changed in 0..7 {
        let mut crossed = input.clone();
        match changed {
            0 => crossed.lifecycle.operation_id = OperationId::new("other-round"),
            1 => crossed.lifecycle.revision += 1,
            2 => crossed.lifecycle.scope.history.history_id = HistoryId::new("other-history"),
            3 => {
                crossed.lifecycle.scope.process_session_id = ProcessSessionId::new("other-process")
            }
            4 => crossed.lifecycle.scope.creation_id = ReplicatorCreationId::new("other-creation"),
            5 => {
                crossed
                    .lifecycle
                    .scope
                    .configuration
                    .current
                    .configuration_id = ConfigurationId::new("other-config")
            }
            _ => crossed.lifecycle.scope.epoch.configuration_number += 1,
        }
        assert!(
            fixture
                .store
                .select_native_input(&goal.intent(), &crossed)
                .await
                .is_err()
        );
        assert_eq!(fixture.store.load_state().await.unwrap(), before);
    }
    assert_eq!(
        *fixture.probe.native_events.lock().unwrap(),
        ["native.open"]
    );
    cut.release_all();
    fixture.achieved(&goal).await;
    fixture.finish().await;
}

#[tokio::test]
async fn native_query_input_and_unknown_effect_survive_observer_loss_without_reexecution() {
    let fixture = native_fixture().await;
    fixture.submit(&fixture.goal).await.unwrap();
    fixture.achieved(&fixture.goal).await;
    fixture.probe.native_events.lock().unwrap().clear();
    fixture
        .probe
        .block_provider_epoch
        .store(true, Ordering::Release);
    if let Ok(permit) = fixture
        .probe
        .provider_epoch_entered
        .try_acquire_many(fixture.probe.provider_epoch_entered.available_permits() as u32)
    {
        permit.forget();
    }
    let goal = query(&fixture, 2, 2);
    fixture.submit(&goal).await.unwrap();
    entered(&fixture.probe.provider_epoch_entered).await;
    let state = fixture.store.load_state().await.unwrap();
    let record = &state.public_v1_operation_state.as_ref().unwrap().operations[&goal.operation_id];
    assert_eq!(record.lifecycle.in_flight, Some(I::NativeInspect));
    assert_eq!(
        record
            .ordinary
            .as_ref()
            .unwrap()
            .native_input
            .as_ref()
            .unwrap()
            .lifecycle
            .scope,
        goal.scope
    );
    assert!(record.lifecycle.outcomes.is_empty());
    assert!(!crate::host::public_lifecycle::replayable(record));
    let empty_owner = crate::host::native_history::NativeLifecycleOwner::default();
    assert!(
        !empty_owner.is_native(),
        "persisted input never recreates a warm native owner"
    );
    fixture.submit(&goal).await.unwrap();
    assert!(fixture.snapshot().await.native_status.is_none());
    fixture.probe.provider_epoch_gate.add_permits(1);
    fixture.achieved(&goal).await;
    let before = fixture.probe.native_events.lock().unwrap().clone();
    fixture.submit(&goal).await.unwrap();
    assert_eq!(*fixture.probe.native_events.lock().unwrap(), before);
    assert_eq!(
        before
            .iter()
            .filter(|event| *event == "native.epoch")
            .count(),
        1
    );
    fixture.finish().await;
}

#[tokio::test]
async fn native_opened_promotion_retains_get_lsn_boundary_across_receipt_loss_and_later_traffic() {
    let fixture = native_fixture().await;
    fixture.probe.provider_progress.store(17, Ordering::Release);
    fixture.submit(&fixture.goal).await.unwrap();
    fixture.achieved(&fixture.goal).await;
    let fixture = reopen_joined_native(fixture).await;
    let mut get_lsn = query(&fixture, 2, 2);
    get_lsn.scope.configuration.previous = Some(fixture.goal.scope.configuration.current.clone());
    fixture.submit(&get_lsn).await.unwrap();
    fixture.achieved(&get_lsn).await;
    fixture.probe.provider_progress.store(91, Ordering::Release);
    let mut promote = get_lsn.clone();
    promote.operation_id = OperationId::new("selected-opened-promotion");
    promote.revision = 3;
    promote.desired = ReplicaGoalKind::Reconfigure {
        role: ReplicaRole::Primary,
        catch_up: Some(PublicCatchUpMode::WriteQuorum),
    };
    let actions = crate::host::public_lifecycle::program::operation_instructions(&promote.intent());
    let boundary_index = actions
        .iter()
        .position(|instruction| *instruction == I::NativeComplete)
        .unwrap();
    let cut = Arc::new(PublicOperationCut::for_instruction(
        &promote,
        "NativeComplete",
        PublicCutPosition::CallbackSuccess,
        boundary_index,
    ));
    fixture.runtime.set_public_operation_cut(Some(cut.clone()));
    fixture.submit(&promote).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), cut.entered.notified())
        .await
        .unwrap();
    let selected = fixture.store.load_state().await.unwrap();
    let record = &selected
        .public_v1_operation_state
        .as_ref()
        .unwrap()
        .operations[&promote.operation_id];
    assert_eq!(record.lifecycle.in_flight, Some(I::NativeComplete));
    assert!(!record.ordinary.as_ref().unwrap().accepted);
    let captured = record
        .ordinary
        .as_ref()
        .unwrap()
        .native_input
        .as_ref()
        .unwrap();
    assert_eq!(
        captured.promotion.as_ref().unwrap().input.operation_id,
        get_lsn.operation_id
    );
    assert_eq!(
        captured.promotion.as_ref().unwrap().progress,
        ProgressQueryResult::Value(17)
    );
    assert!(
        !record
            .lifecycle
            .outcomes
            .iter()
            .any(|outcome| matches!(outcome, O::NativeLifecycle(_)))
    );
    let calls = fixture.probe.native_events.lock().unwrap().clone();
    fixture.submit(&promote).await.unwrap();
    assert_eq!(*fixture.probe.native_events.lock().unwrap(), calls);
    cut.release_all();
    let snapshot = fixture.achieved(&promote).await;
    let facts = snapshot.native_lifecycle.unwrap();
    assert_eq!(facts.state, NativeReplicaState::Ready);
    assert_eq!(facts.deactivation.as_ref().unwrap().catch_up_lsn, 17);
    assert_eq!(
        fixture
            .probe
            .provider_data_loss_calls
            .load(Ordering::Acquire),
        0
    );
    assert!(!calls.iter().any(|event| event == "native.data_loss"));
    fixture.acknowledge(&promote).await;
    fixture.runtime.set_public_operation_cut(None);
    let mut current = promote.clone();
    current.operation_id = OperationId::new("normal-current-end");
    current.revision = 4;
    current.scope.configuration.previous = None;
    fixture.submit(&current).await.unwrap();
    fixture.achieved(&current).await;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if fixture.snapshot().await.write_status == AccessStatus::Granted {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let writer = fixture.probe.native_writer.lock().unwrap().clone().unwrap();
    let write = writer
        .begin_replicate(bytes::Bytes::from_static(b"after-captured-promotion"))
        .unwrap();
    assert_eq!(write.lsn, 18);
    write.completion.await.unwrap();
    let sampled = query(&fixture, 5, 2);
    fixture.submit(&sampled).await.unwrap();
    let snapshot = fixture.achieved(&sampled).await;
    assert_eq!(
        snapshot.native_status.unwrap().progress,
        ProgressQueryResult::Value(18)
    );
    assert_eq!(
        snapshot
            .native_lifecycle
            .unwrap()
            .deactivation
            .unwrap()
            .catch_up_lsn,
        17
    );
    let state = fixture.store.load_state().await.unwrap();
    let record =
        &state.public_v1_operation_state.as_ref().unwrap().operations[&promote.operation_id];
    let O::NativeLifecycle(stored) = record.lifecycle.outcomes.last().unwrap() else {
        panic!("persisted boundary")
    };
    assert_eq!(stored.deactivation.as_ref().unwrap().catch_up_lsn, 17);
    let binding = PublicV1LifecycleBinding {
        identity: fixture.goal.identity.clone(),
        resource_uid: fixture.goal.scope.history.resource_uid.clone(),
        spec_generation: 1,
        state_persistence: StatePersistence::Persisted,
    };
    let path = fixture._root.path().join("agent.sqlite3");
    let bytes = std::fs::read(&path).unwrap();
    let reopened = SqliteStore::open_replacement_bound_existing(&path, None, &binding).unwrap();
    assert_eq!(reopened.load_state().await.unwrap(), state);
    assert_eq!(std::fs::read(path).unwrap(), bytes);
    let mut crossed = sampled.scope.clone();
    crossed.creation_id = ReplicatorCreationId::new("recreated-not-warm");
    assert!(crate::host::native_history::status_for(&state, &crossed).is_none());
    assert!(crate::host::native_history::lifecycle_for(&state, &crossed).is_none());
    fixture.finish().await;
}

#[tokio::test]
async fn native_new_idle_inherits_only_actual_ready_source_history_without_copy_or_ready() {
    let source = native_fixture().await;
    let target = Fixture::new(2, false).await;
    target
        .probe
        .use_default_factory
        .store(true, Ordering::Release);
    source.link(&target);
    target.link(&source);
    source.submit(&source.goal).await.unwrap();
    source.achieved(&source.goal).await;
    target.submit(&target.goal).await.unwrap();
    let snapshot = target.achieved(&target.goal).await;
    let facts = snapshot.native_lifecycle.unwrap();
    assert_eq!(facts.state, NativeReplicaState::InBuild);
    assert_eq!(facts.native_role, ReplicaRole::IdleSecondary);
    assert_eq!(
        facts.deactivation.unwrap(),
        NativeDeactivationInfo {
            primary_fence: Epoch::new(0, 1),
            catch_up_lsn: 0,
        }
    );
    assert_ne!(snapshot.write_status, AccessStatus::Granted);
    assert!(snapshot.remote_operations.is_empty());
    let state = target.store.load_state().await.unwrap();
    let selected = state.public_v1_operation_state.as_ref().unwrap().operations
        [&target.goal.operation_id]
        .ordinary
        .as_ref()
        .unwrap()
        .native_input
        .as_ref()
        .unwrap();
    assert_eq!(
        selected.source.as_ref().unwrap().scope.process_session_id,
        source.goal.scope.process_session_id
    );
    assert_eq!(
        selected.source.as_ref().unwrap().state,
        NativeReplicaState::Ready
    );
    assert!(
        !target
            .probe
            .native_events
            .lock()
            .unwrap()
            .iter()
            .any(|event| event == "native.build")
    );
    source.finish().await;
    target.finish().await;
}

#[tokio::test]
async fn native_inbuild_query_executes_actual_epoch_and_keeps_incomplete_copy_failure() {
    let fixture = Fixture::new(2, false).await;
    fixture
        .probe
        .use_default_factory
        .store(true, Ordering::Release);
    fixture.submit(&fixture.goal).await.unwrap();
    let prepared = fixture.achieved(&fixture.goal).await;
    assert_eq!(
        prepared.native_lifecycle.unwrap().state,
        NativeReplicaState::InBuild
    );
    fixture.probe.native_events.lock().unwrap().clear();
    let goal = query(&fixture, 2, 1);
    fixture.submit(&goal).await.unwrap();
    fixture.failed(&goal).await;
    let state = fixture.store.load_state().await.unwrap();
    let record = &state.public_v1_operation_state.as_ref().unwrap().operations[&goal.operation_id];
    let O::NativeStatus(status) = record.lifecycle.outcomes.last().unwrap() else {
        panic!("failed actual epoch")
    };
    assert_eq!(status.input.state, NativeReplicaState::InBuild);
    assert!(matches!(status.epoch, NativeInspectionEpoch::Failed(_)));
    assert!(matches!(
        status.progress,
        ProgressQueryResult::NotQueried(_)
    ));
    assert_eq!(
        *fixture.probe.native_events.lock().unwrap(),
        ["native.epoch"]
    );
    fixture.finish().await;
}

#[tokio::test]
async fn native_opened_active_role_does_not_apply_epoch_or_skip_failed_program() {
    let source = native_fixture().await;
    source.submit(&source.goal).await.unwrap();
    source.achieved(&source.goal).await;
    let fixture = Fixture::new(2, false).await;
    fixture
        .probe
        .use_default_factory
        .store(true, Ordering::Release);
    fixture.link(&source);
    source.link(&fixture);
    let mut prepare = fixture.goal.clone();
    prepare.scope.configuration =
        native_pair_configuration(&source.snapshot().await.publication.unwrap(), 0);
    prepare.desired = ReplicaGoalKind::Prepare {
        role: ReplicaRole::ActiveSecondary,
        rebuild: None,
    };
    let actions = crate::host::public_lifecycle::program::operation_instructions(&prepare.intent());
    let epoch_index = actions
        .iter()
        .position(|instruction| *instruction == I::ProgramEpoch)
        .unwrap();
    let cut = Arc::new(PublicOperationCut::for_instruction(
        &prepare,
        "ProgramEpoch",
        PublicCutPosition::InstructionSelected,
        epoch_index,
    ));
    fixture.runtime.set_public_operation_cut(Some(cut.clone()));
    fixture.submit(&prepare).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), cut.entered.notified())
        .await
        .unwrap();
    let callbacks = fixture.runtime.public_lifecycle_callbacks().unwrap();
    assert!(
        !callbacks
            .native
            .as_ref()
            .unwrap()
            .applied(prepare.scope.epoch)
    );
    let state = fixture.store.load_state().await.unwrap();
    let record =
        &state.public_v1_operation_state.as_ref().unwrap().operations[&prepare.operation_id];
    assert_eq!(record.lifecycle.in_flight, Some(I::ProgramEpoch));
    assert!(matches!(
        record.lifecycle.outcomes.last(),
        Some(O::ApplicationRole(_))
    ));
    assert_eq!(
        record
            .ordinary
            .as_ref()
            .unwrap()
            .native_input
            .as_ref()
            .unwrap()
            .lifecycle
            .applied_primary_fence,
        None
    );
    assert_eq!(
        *fixture.probe.native_events.lock().unwrap(),
        [
            "native.open",
            "native.role:ActiveSecondary",
            "application.role:ActiveSecondary"
        ]
    );
    cut.release_all();
    fixture.failed(&prepare).await;
    assert!(
        !callbacks
            .native
            .as_ref()
            .unwrap()
            .applied(prepare.scope.epoch)
    );
    assert_eq!(
        fixture.probe.native_events.lock().unwrap().last().unwrap(),
        "native.epoch"
    );
    let failed = fixture.store.load_state().await.unwrap();
    assert_eq!(
        failed
            .public_v1_operation_state
            .as_ref()
            .unwrap()
            .operations[&prepare.operation_id]
            .lifecycle
            .in_flight,
        Some(I::ProgramEpoch)
    );
    assert!(fixture.submit(&prepare).await.is_err());
    assert_eq!(fixture.store.load_state().await.unwrap(), failed);
    assert!(fixture.snapshot().await.native_lifecycle.is_none());
    source.finish().await;
    fixture.finish().await;
}

#[tokio::test]
async fn native_opened_active_role_only_following_query_executes_actual_epoch_and_fails() {
    let fixture = Fixture::new(2, false).await;
    fixture
        .probe
        .use_default_factory
        .store(true, Ordering::Release);
    let opened = query(&fixture, 1, 1);
    fixture.submit(&opened).await.unwrap();
    fixture.achieved(&opened).await;
    let callbacks = fixture.runtime.public_lifecycle_callbacks().unwrap();
    // Isolate the actual role-only path: no completed preparation or epoch
    // receipt is claimed by these direct native/application callbacks.
    callbacks
        .replicator
        .change_role(opened.scope.epoch, ReplicaRole::ActiveSecondary)
        .await
        .unwrap();
    callbacks
        .application
        .change_role(ReplicaRole::ActiveSecondary)
        .await
        .unwrap();
    callbacks
        .native
        .as_ref()
        .unwrap()
        .application_role(ReplicaRole::ActiveSecondary);
    assert!(
        !callbacks
            .native
            .as_ref()
            .unwrap()
            .applied(opened.scope.epoch)
    );
    fixture.probe.native_events.lock().unwrap().clear();
    let inspect = query(&fixture, 2, 1);
    fixture.submit(&inspect).await.unwrap();
    fixture.failed(&inspect).await;
    let state = fixture.store.load_state().await.unwrap();
    let record =
        &state.public_v1_operation_state.as_ref().unwrap().operations[&inspect.operation_id];
    let O::NativeStatus(status) = record.lifecycle.outcomes.last().unwrap() else {
        panic!("actual failed ActiveSecondary inspection must be retained")
    };
    assert_eq!(status.input.native_role, ReplicaRole::ActiveSecondary);
    assert_eq!(status.input.state, NativeReplicaState::InBuild);
    assert_eq!(status.input.applied_primary_fence, None);
    assert!(matches!(status.epoch, NativeInspectionEpoch::Failed(_)));
    assert!(matches!(
        status.progress,
        ProgressQueryResult::NotQueried(_)
    ));
    assert!(matches!(
        status.capability,
        ProgressQueryResult::NotQueried(_)
    ));
    assert_eq!(
        *fixture.probe.native_events.lock().unwrap(),
        ["native.epoch"]
    );
    assert!(fixture.snapshot().await.native_lifecycle.is_none());
    assert!(fixture.snapshot().await.native_status.is_none());
    fixture.finish().await;
}

fn native_pair_configuration(
    source: &ReplicaPublication,
    progress: i64,
) -> ReplicaConfigurationScope {
    ReplicaConfigurationScope {
        current: ConfigurationDescriptor::new(
            source.scope.epoch,
            replica(1).replica_id,
            vec![
                ConfigurationMember {
                    identity: replica(1),
                    role: ReplicaRole::Primary,
                },
                ConfigurationMember {
                    identity: replica(2),
                    role: ReplicaRole::ActiveSecondary,
                },
            ],
            2,
        ),
        previous: Some(source.scope.configuration.current.clone()),
        replicas: vec![ReplicaPeerDescription {
            identity: replica(1),
            replication_address: source.replication_address.clone(),
            process_session_id: source.scope.process_session_id.clone(),
            storage_incarnation: source.scope.storage.clone(),
            role: ReplicaRole::Primary,
            current_progress: progress,
            catch_up_capability: 0,
            is_up: true,
            is_ready: true,
            must_catchup: false,
        }],
    }
}

async fn copied_native_pair() -> (Fixture, Fixture, ReplicaConfigurationScope) {
    let source = native_fixture().await;
    let target = Fixture::new(2, false).await;
    target
        .probe
        .use_default_factory
        .store(true, Ordering::Release);
    source.link(&target);
    target.link(&source);
    source.submit(&source.goal).await.unwrap();
    source.achieved(&source.goal).await;
    let mut serving = source.goal.clone();
    serving.operation_id = OperationId::new("copy-source-serving");
    serving.revision = 2;
    serving.desired = ReplicaGoalKind::Reconfigure {
        role: ReplicaRole::Primary,
        catch_up: Some(PublicCatchUpMode::WriteQuorum),
    };
    source.submit(&serving).await.unwrap();
    source.achieved(&serving).await;
    tokio::time::timeout(Duration::from_secs(5), async {
        while source.snapshot().await.write_status != AccessStatus::Granted {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let writer = source.probe.native_writer.lock().unwrap().clone().unwrap();
    for expected in 1..=3 {
        let write = writer
            .begin_replicate(bytes::Bytes::from_static(b"native-copy-prefix"))
            .unwrap();
        assert_eq!(write.lsn, expected);
        write.completion.await.unwrap();
    }
    target.submit(&target.goal).await.unwrap();
    target.achieved(&target.goal).await;
    let source_publication = source.snapshot().await.publication.unwrap();
    let target_publication = target.snapshot().await.publication.unwrap();
    let mut build = serving.clone();
    build.operation_id = OperationId::new("actual-native-idle-build");
    build.revision = 3;
    build.desired = ReplicaGoalKind::Build {
        remote: Box::new(RemoteReplicaSelection {
            original_member: None,
            source: source_publication.clone(),
            target: target_publication.clone(),
            target_control_address: target.address.clone(),
            generation: 1,
        }),
    };
    let receiver = target.probe.native_writer.lock().unwrap().clone().unwrap();
    let mut copy = receiver.copy_operations().unwrap();
    source.submit(&build).await.unwrap();
    loop {
        let operation = tokio::time::timeout(Duration::from_secs(5), copy.get_operation())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(
            operation.kind,
            crate::replicator::stream::OperationKind::Copy
        );
        assert_eq!(operation.epoch, Epoch::default());
        let last = operation.end_of_stream;
        if last {
            assert_eq!(operation.lsn, 1, "empty snapshot copy sequence");
        }
        operation.acknowledge().unwrap();
        if last {
            break;
        }
    }
    let built = source.achieved(&build).await;
    assert!(built.remote_operations.iter().any(|remote| {
        remote.operation_id == build.operation_id && remote.state == RemoteReplicaState::Ready
    }));
    source.acknowledge(&build).await;
    let pair = native_pair_configuration(&source_publication, 3);
    (source, target, pair)
}

#[tokio::test]
async fn native_copied_idle_activation_requires_explicit_epoch_before_ready_history() {
    let (source, target, pair) = copied_native_pair().await;
    let mut active = target.goal.clone();
    active.operation_id = OperationId::new("copied-idle-active");
    active.revision = 2;
    active.scope.configuration = pair;
    active.desired = ReplicaGoalKind::Reconfigure {
        role: ReplicaRole::ActiveSecondary,
        catch_up: None,
    };
    let actions = crate::host::public_lifecycle::program::operation_instructions(&active.intent());
    let epoch_index = actions
        .iter()
        .position(|instruction| *instruction == I::ProgramEpoch)
        .unwrap();
    let cut = Arc::new(PublicOperationCut::for_instruction(
        &active,
        "ProgramEpoch",
        PublicCutPosition::InstructionSelected,
        epoch_index,
    ));
    target.runtime.set_public_operation_cut(Some(cut.clone()));
    target.probe.native_events.lock().unwrap().clear();
    target.submit(&active).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), cut.entered.notified())
        .await
        .unwrap();
    let callbacks = target.runtime.public_lifecycle_callbacks().unwrap();
    assert!(
        !callbacks
            .native
            .as_ref()
            .unwrap()
            .applied(active.scope.epoch)
    );
    assert_eq!(callbacks.replicator.current_progress().await.unwrap(), 3);
    let state = target.store.load_state().await.unwrap();
    let input = state.public_v1_operation_state.as_ref().unwrap().operations[&active.operation_id]
        .ordinary
        .as_ref()
        .unwrap()
        .native_input
        .as_ref()
        .unwrap();
    assert_eq!(input.lifecycle.native_role, ReplicaRole::IdleSecondary);
    assert_eq!(input.lifecycle.applied_primary_fence, None);
    assert_eq!(
        input.source.as_ref().unwrap().native_role,
        ReplicaRole::Primary
    );
    cut.release_all();
    let snapshot = target.achieved(&active).await;
    let facts = snapshot.native_lifecycle.unwrap();
    assert_eq!(facts.state, NativeReplicaState::Ready);
    assert_eq!(facts.native_role, ReplicaRole::ActiveSecondary);
    assert_eq!(facts.application_role, Some(ReplicaRole::ActiveSecondary));
    assert_eq!(facts.applied_primary_fence, Some(active.scope.epoch));
    assert_eq!(
        facts.deactivation,
        source
            .snapshot()
            .await
            .native_lifecycle
            .unwrap()
            .deactivation
    );
    let events = target.probe.native_events.lock().unwrap().clone();
    let role = events
        .iter()
        .position(|event| event == "native.role:ActiveSecondary")
        .unwrap();
    let epoch = events
        .iter()
        .position(|event| event == "native.epoch")
        .unwrap();
    let final_status = events
        .iter()
        .rposition(|event| event == "native.progress")
        .unwrap();
    assert!(role < epoch && epoch < final_status);
    assert!(!events.iter().any(|event| event == "provider.progress"));
    target.assert_reopens().await;
    target.runtime.set_public_operation_cut(None);
    target.probe.native_events.lock().unwrap().clear();
    let mut inspect = query(&target, 3, 1);
    inspect.scope.configuration = active.scope.configuration.clone();
    inspect.scope.configuration.replicas.clear();
    target.submit(&inspect).await.unwrap();
    let status = target.achieved(&inspect).await.native_status.unwrap();
    assert_eq!(status.input.native_role, ReplicaRole::ActiveSecondary);
    assert_eq!(status.epoch, NativeInspectionEpoch::AlreadyApplied);
    assert_eq!(status.progress, ProgressQueryResult::Value(3));
    assert_eq!(
        *target.probe.native_events.lock().unwrap(),
        ["native.progress", "native.capability"]
    );
    source.finish().await;
    target.finish().await;
}

#[tokio::test]
async fn native_source_handoff_selects_local_input_while_successor_is_actual_secondary() {
    let (source, target, pair) = copied_native_pair().await;
    let mut active = target.goal.clone();
    active.operation_id = OperationId::new("successor-still-secondary");
    active.revision = 2;
    active.scope.configuration = pair.clone();
    active.desired = ReplicaGoalKind::Reconfigure {
        role: ReplicaRole::ActiveSecondary,
        catch_up: None,
    };
    let target_publication = target.snapshot().await.publication.unwrap();
    let mut admit = source.goal.clone();
    admit.operation_id = OperationId::new("source-real-copied-member-configuration");
    admit.revision = 4;
    admit.scope.configuration = pair;
    admit.scope.configuration.replicas = vec![ReplicaPeerDescription {
        identity: replica(2),
        replication_address: target_publication.replication_address.clone(),
        process_session_id: target_publication.scope.process_session_id.clone(),
        storage_incarnation: target_publication.scope.storage.clone(),
        role: ReplicaRole::ActiveSecondary,
        current_progress: 3,
        catch_up_capability: 0,
        is_up: true,
        is_ready: true,
        must_catchup: false,
    }];
    admit.desired = ReplicaGoalKind::Reconfigure {
        role: ReplicaRole::Primary,
        catch_up: None,
    };
    source.submit(&admit).await.unwrap();
    source.achieved(&admit).await;
    target.submit(&active).await.unwrap();
    let successor = target.achieved(&active).await;
    let successor_facts = successor.native_lifecycle.as_ref().unwrap();
    assert_eq!(successor_facts.native_role, ReplicaRole::ActiveSecondary);
    assert_eq!(
        successor_facts.application_role,
        Some(ReplicaRole::ActiveSecondary)
    );
    assert_eq!(successor_facts.state, NativeReplicaState::Ready);
    let mut handoff = admit.clone();
    handoff.operation_id = OperationId::new("normal-source-with-secondary-successor");
    handoff.revision = 5;
    handoff.scope.epoch = Epoch::new(0, 2);
    handoff.scope.configuration.previous = Some(admit.scope.configuration.current.clone());
    handoff.scope.configuration.current = ConfigurationDescriptor::new(
        handoff.scope.epoch,
        replica(2).replica_id,
        vec![
            ConfigurationMember {
                identity: replica(1),
                role: ReplicaRole::ActiveSecondary,
            },
            ConfigurationMember {
                identity: replica(2),
                role: ReplicaRole::Primary,
            },
        ],
        2,
    );
    handoff.scope.configuration.replicas[0].role = ReplicaRole::Primary;
    handoff.desired = ReplicaGoalKind::Reconfigure {
        role: ReplicaRole::ActiveSecondary,
        catch_up: Some(PublicCatchUpMode::All),
    };
    assert!(crate::host::public_lifecycle::program::ordinary_handoff(&handoff).is_some());
    let cut = Arc::new(PublicOperationCut::for_instruction(
        &handoff,
        "ProgramEpoch",
        PublicCutPosition::CallbackSuccess,
        1,
    ));
    source.runtime.set_public_operation_cut(Some(cut.clone()));
    source.probe.native_events.lock().unwrap().clear();
    source.submit(&handoff).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), cut.entered.notified())
        .await
        .unwrap();
    let state = source.store.load_state().await.unwrap();
    let record =
        &state.public_v1_operation_state.as_ref().unwrap().operations[&handoff.operation_id];
    let input = record
        .ordinary
        .as_ref()
        .unwrap()
        .native_input
        .as_ref()
        .unwrap();
    assert!(input.source.is_none());
    assert_eq!(input.lifecycle.native_role, ReplicaRole::Primary);
    assert_eq!(input.lifecycle.scope, handoff.scope);
    assert_eq!(record.lifecycle.in_flight, Some(I::ProgramEpoch));
    assert_eq!(record.lifecycle.outcomes, [O::Done]);
    assert!(!record.ordinary.as_ref().unwrap().accepted);
    assert_ne!(source.snapshot().await.write_status, AccessStatus::Granted);
    let still_secondary = target.snapshot().await.native_lifecycle.unwrap();
    assert_eq!(&still_secondary, successor_facts);
    assert!(
        source
            .runtime
            .public_lifecycle_callbacks()
            .unwrap()
            .native
            .unwrap()
            .applied(handoff.scope.epoch)
    );
    assert_eq!(
        *source.probe.native_events.lock().unwrap(),
        [
            "native.epoch",
            "provider.epoch:Epoch { data_loss_number: 0, configuration_number: 2 }"
        ]
    );
    // Bound this witness before remote configuration/swap integration. Revoke
    // the real creation, then release and join its held operation.
    source.runtime.withdraw_ordinary();
    cut.release_all();
    source.finish().await;
    target.finish().await;
}

#[tokio::test]
async fn native_warm_secondary_preparation_does_not_require_future_primary_history() {
    let (source, target, pair) = copied_native_pair().await;
    let mut active = target.goal.clone();
    active.operation_id = OperationId::new("warm-before-election");
    active.revision = 2;
    active.scope.configuration = pair;
    active.desired = ReplicaGoalKind::Reconfigure {
        role: ReplicaRole::ActiveSecondary,
        catch_up: None,
    };
    target.submit(&active).await.unwrap();
    let ready = target.achieved(&active).await.native_lifecycle.unwrap();
    let source_before = source.snapshot().await.native_lifecycle.unwrap();
    let mut preparation = active.clone();
    preparation.operation_id = OperationId::new("warm-election-dependency");
    preparation.revision = 3;
    preparation.scope.epoch = Epoch::new(0, 2);
    preparation.scope.configuration.previous = Some(active.scope.configuration.current.clone());
    preparation.scope.configuration.current.epoch = preparation.scope.epoch;
    preparation.scope.configuration.current.configuration_id =
        preparation.scope.configuration.current.expected_id();
    let actions =
        crate::host::public_lifecycle::program::operation_instructions(&preparation.intent());
    let index = actions
        .iter()
        .position(|instruction| *instruction == I::ProgramEpoch)
        .unwrap();
    let cut = Arc::new(PublicOperationCut::for_instruction(
        &preparation,
        "ProgramEpoch",
        PublicCutPosition::InstructionSelected,
        index,
    ));
    target.runtime.set_public_operation_cut(Some(cut.clone()));
    target.submit(&preparation).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), cut.entered.notified())
        .await
        .unwrap();
    let state = target.store.load_state().await.unwrap();
    let selected = state.public_v1_operation_state.as_ref().unwrap().operations
        [&preparation.operation_id]
        .ordinary
        .as_ref()
        .unwrap()
        .native_input
        .as_ref()
        .unwrap();
    assert_eq!(selected.lifecycle.state, NativeReplicaState::Ready);
    assert_eq!(selected.lifecycle.deactivation, ready.deactivation);
    assert!(selected.source.is_none());
    assert_eq!(
        source.snapshot().await.native_lifecycle,
        Some(source_before)
    );
    assert!(
        !state.public_v1_operation_state.as_ref().unwrap().operations[&preparation.operation_id]
            .ordinary
            .as_ref()
            .unwrap()
            .accepted
    );
    target.runtime.withdraw_ordinary();
    cut.release_all();
    target.finish().await;
    source.finish().await;
}

#[tokio::test]
async fn native_history_inheritance_still_rejects_secondary_source_and_crossed_configuration() {
    let (source, target, pair) = copied_native_pair().await;
    let mut active = target.goal.clone();
    active.operation_id = OperationId::new("actual-secondary-not-history-primary");
    active.revision = 2;
    active.scope.configuration = pair.clone();
    active.desired = ReplicaGoalKind::Reconfigure {
        role: ReplicaRole::ActiveSecondary,
        catch_up: None,
    };
    target.submit(&active).await.unwrap();
    let secondary = target.achieved(&active).await.native_lifecycle.unwrap();
    assert_eq!(secondary.state, NativeReplicaState::Ready);
    assert_eq!(secondary.native_role, ReplicaRole::ActiveSecondary);
    for (id, peer, configuration) in [
        (3, &source, pair.current),
        (
            4,
            &target,
            ConfigurationDescriptor::new(
                target.goal.scope.epoch,
                replica(2).replica_id,
                vec![ConfigurationMember {
                    identity: replica(2),
                    role: ReplicaRole::Primary,
                }],
                1,
            ),
        ),
    ] {
        let candidate = Fixture::new(id, false).await;
        candidate
            .probe
            .use_default_factory
            .store(true, Ordering::Release);
        candidate.link(peer);
        // Deliberately crossed negative inputs, not an actual Primary/config
        // publication: both peer snapshots still come from real native owners.
        let mut goal = candidate.goal.clone();
        goal.scope.configuration.current = configuration;
        candidate.submit(&goal).await.unwrap();
        let rejected = candidate.failed(&goal).await;
        assert!(rejected.goals.iter().any(|entry| {
            entry.goal == goal
                && matches!(&entry.status, ReplicaGoalStatus::Failed(error)
                if error.contains("source history crossed its actual Ready owner/configuration"))
        }));
        let state = candidate.store.load_state().await.unwrap();
        assert!(
            state.public_v1_operation_state.as_ref().unwrap().operations[&goal.operation_id]
                .ordinary
                .as_ref()
                .unwrap()
                .native_input
                .is_none()
        );
        assert_eq!(
            *candidate.probe.native_events.lock().unwrap(),
            ["native.open"]
        );
        assert!(rejected.native_lifecycle.is_none());
        assert_ne!(rejected.write_status, AccessStatus::Granted);
        candidate.finish().await;
    }
    source.finish().await;
    target.finish().await;
}

#[tokio::test]
async fn native_missing_history_old_matching_record_remains_missing_and_input_is_immutable() {
    let fixture = native_fixture().await;
    fixture
        .store
        .complete_application_initialization()
        .await
        .unwrap();
    let goal = query(&fixture, 1, 1);
    fixture.submit(&goal).await.unwrap();
    fixture.achieved(&goal).await;
    let state = fixture.store.load_state().await.unwrap();
    let record = &state.public_v1_operation_state.as_ref().unwrap().operations[&goal.operation_id];
    let mut legacy = serde_json::to_value(record).unwrap();
    legacy["ordinary"]
        .as_object_mut()
        .unwrap()
        .remove("native_input");
    let mut legacy: crate::host::state::PublicOperationRecord =
        serde_json::from_value(legacy).unwrap();
    assert!(legacy.ordinary.as_ref().unwrap().native_input.is_none());
    legacy.ordinary.as_mut().unwrap().actions =
        crate::host::public_lifecycle::program::legacy_ordinary_instructions(&goal);
    legacy.lifecycle.outcomes.truncate(2);
    for attempt in legacy.attempt_results.values_mut() {
        attempt.lifecycle = legacy.lifecycle.clone();
    }
    let mut old_state = state.clone();
    old_state
        .public_v1_operation_state
        .as_mut()
        .unwrap()
        .operations
        .insert(goal.operation_id.clone(), legacy.clone());
    let binding = PublicV1LifecycleBinding {
        identity: goal.identity.clone(),
        resource_uid: goal.scope.history.resource_uid.clone(),
        spec_generation: 1,
        state_persistence: StatePersistence::Persisted,
    };
    let path = fixture._root.path().join("pre-component23.sqlite3");
    let mut initial = old_state.clone();
    initial.public_v1_operation_state = None;
    let created =
        SqliteStore::create_replacement_authorized(&path, initial, binding.clone()).unwrap();
    let connection = rusqlite::Connection::open(&path).unwrap();
    let mut bytes = serde_json::to_value(&old_state).unwrap();
    bytes["publicV1OperationState"]["operations"][goal.operation_id.as_str()]["ordinary"]
        .as_object_mut()
        .unwrap()
        .remove("native_input");
    connection
        .execute(
            "UPDATE agent_state SET state_json = ?1 WHERE singleton = 1",
            [serde_json::to_string(&bytes).unwrap()],
        )
        .unwrap();
    drop(connection);
    drop(created);
    let before = std::fs::read(&path).unwrap();
    let reopened = SqliteStore::open_replacement_bound_existing(&path, None, &binding).unwrap();
    let restored = reopened.load_state().await.unwrap();
    assert_eq!(restored, old_state);
    assert_eq!(
        crate::host::ordinary_goal::freeze(&restored, &goal)
            .unwrap()
            .actions,
        legacy.ordinary.unwrap().actions
    );
    assert!(crate::host::native_history::status_for(&restored, &goal.scope).is_none());
    assert!(crate::host::native_history::lifecycle_for(&restored, &goal.scope).is_none());
    assert_eq!(std::fs::read(path).unwrap(), before);
    assert!(
        crate::host::ordinary_goal::freeze(&state, &goal)
            .unwrap()
            .native_input
            .is_some()
    );
    let mut input = record
        .ordinary
        .as_ref()
        .unwrap()
        .native_input
        .clone()
        .unwrap();
    input.lifecycle.scope.creation_id = ReplicatorCreationId::new("crossed");
    assert!(
        fixture
            .store
            .select_native_input(&goal.intent(), &input)
            .await
            .is_err()
    );
    assert_eq!(fixture.store.load_state().await.unwrap(), state);
    assert!(fixture.snapshot().await.native_lifecycle.is_none());
    fixture.finish().await;
}

#[tokio::test]
async fn native_source_demotion_captures_final_status_not_end_wait_or_later_provider_scalar() {
    let fixture = native_fixture().await;
    fixture.submit(&fixture.goal).await.unwrap();
    fixture.achieved(&fixture.goal).await;
    let mut serving = fixture.goal.clone();
    serving.operation_id = OperationId::new("source-local-quorum");
    serving.revision = 2;
    serving.desired = ReplicaGoalKind::Reconfigure {
        role: ReplicaRole::Primary,
        catch_up: Some(PublicCatchUpMode::WriteQuorum),
    };
    fixture.submit(&serving).await.unwrap();
    fixture.achieved(&serving).await;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if fixture.snapshot().await.write_status == AccessStatus::Granted {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let writer = fixture.probe.native_writer.lock().unwrap().clone().unwrap();
    for expected in 1..=3 {
        let write = writer
            .begin_replicate(bytes::Bytes::from_static(b"before-write-revoke"))
            .unwrap();
        assert_eq!(write.lsn, expected);
        write.completion.await.unwrap();
    }
    let callbacks = fixture.runtime.public_lifecycle_callbacks().unwrap();
    fixture.probe.native_events.lock().unwrap().clear();
    // Exercise the real native producer without inventing an active remote
    // session to satisfy the still-separate original-member projection work.
    let wait: () = callbacks
        .primary
        .wait_for_catch_up_quorum(ReplicaSetQuorumMode::All)
        .await
        .unwrap();
    assert_eq!(wait, (), "EndWait does not return an LSN");
    (callbacks.revoke_access)(false).unwrap();
    let epoch = Epoch::new(0, 2);
    callbacks.replicator.update_epoch(epoch).await.unwrap();
    callbacks
        .replicator
        .change_role(epoch, ReplicaRole::ActiveSecondary)
        .await
        .unwrap();
    callbacks
        .application
        .change_role(ReplicaRole::ActiveSecondary)
        .await
        .unwrap();
    callbacks
        .native
        .as_ref()
        .unwrap()
        .application_role(ReplicaRole::ActiveSecondary);
    let boundary = crate::host::native_history::demotion_status(epoch, &callbacks)
        .await
        .unwrap();
    assert_eq!(boundary, 3);
    assert!(
        writer
            .begin_replicate(bytes::Bytes::from_static(b"after-revoke"))
            .is_err()
    );
    fixture
        .probe
        .provider_progress
        .store(999, Ordering::Release);
    let events = fixture.probe.native_events.lock().unwrap().clone();
    let demotion = events
        .iter()
        .position(|event| event == "native.role:ActiveSecondary")
        .unwrap();
    let application = events
        .iter()
        .position(|event| event == "application.role:ActiveSecondary")
        .unwrap();
    let status = events
        .iter()
        .position(|event| event == "native.progress")
        .unwrap();
    assert!(demotion < application && application < status);
    assert!(!events.iter().any(|event| event == "provider.progress"));
    assert_eq!(
        crate::host::native_history::demotion_status(epoch, &callbacks)
            .await
            .unwrap(),
        boundary
    );
    assert!(
        fixture.snapshot().await.native_lifecycle.is_none(),
        "direct native effects do not fabricate a journal completion"
    );
    fixture.finish().await;
}
