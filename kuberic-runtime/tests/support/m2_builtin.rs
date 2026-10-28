use super::*;
use bytes::Bytes;
use futures::StreamExt;
use kuberic_runtime::replicator::stream::{OperationStream, ReceivedOperation};
use kuberic_runtime::replicator::{StateReplicator, TestingTcpRoute, TestingTcpRouteKind as Cut};
use std::collections::BTreeMap;
use tokio::sync::Semaphore;

struct EngineService {
    interfaces: OnceLock<ReplicatorInterfaces>,
    context: Vec<Bytes>,
    items: Mutex<Vec<Bytes>>,
    observed_context: Mutex<Vec<Bytes>>,
    acquisitions: AtomicUsize,
    context_calls: AtomicUsize,
    context_entered: Semaphore,
    context_release: Semaphore,
    copy_entered: Semaphore,
    copy_release: Semaphore,
    close_calls: AtomicUsize,
    block_role: AtomicBool,
    role_entered: Semaphore,
    role_release: Semaphore,
}

#[async_trait::async_trait]
impl StateProvider for EngineService {
    async fn update_epoch(&self, _: Epoch, _: i64) -> Result<()> {
        Ok(())
    }
    async fn last_committed_lsn(&self) -> Result<i64> {
        Ok(0)
    }
    async fn get_copy_context(&self) -> Result<OperationDataStream> {
        self.context_calls.fetch_add(1, Ordering::SeqCst);
        self.context_entered.add_permits(1);
        self.context_release.acquire().await.unwrap().forget();
        Ok(Box::pin(futures::stream::iter(
            self.context.clone().into_iter().map(Ok),
        )))
    }
    async fn get_copy_state(
        &self,
        up_to: i64,
        mut context: OperationDataStream,
    ) -> Result<OperationDataStream> {
        assert_eq!(
            up_to, 0,
            "copy captures the actual start-minus-one boundary"
        );
        self.acquisitions.fetch_add(1, Ordering::SeqCst);
        self.copy_entered.add_permits(1);
        self.copy_release.acquire().await.unwrap().forget();
        let mut received = Vec::new();
        while let Some(item) = context.next().await {
            received.push(item?);
        }
        *self.observed_context.lock().unwrap() = received;
        Ok(Box::pin(futures::stream::iter(
            self.items.lock().unwrap().clone().into_iter().map(Ok),
        )))
    }
    async fn on_data_loss(&self) -> Result<bool> {
        Ok(false)
    }
}

#[async_trait::async_trait]
impl StatefulServiceReplica for EngineService {
    async fn open(self: Arc<Self>, context: OpenContext) -> Result<Arc<dyn Replicator>> {
        let interfaces = context
            .partition
            .with_builtin_replicator()
            .create_replicator(
                Some(self.clone()),
                Some(ReplicatorSettings {
                    replication_address: "http://127.0.0.1:0".into(),
                }),
            )
            .await?;
        let replicator = interfaces.replicator();
        assert!(self.interfaces.set(interfaces).is_ok());
        Ok(replicator)
    }
    async fn change_role(&self, _: ReplicaRole) -> Result<RoleChange> {
        if self.block_role.load(Ordering::SeqCst) {
            self.role_entered.add_permits(1);
            self.role_release.acquire().await.unwrap().forget();
        }
        Ok(RoleChange {
            service_address: None,
        })
    }
    async fn close(&self) -> Result<()> {
        self.close_calls.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    fn abort(&self) {}
}

#[async_trait::async_trait]
impl Sample for EngineService {
    fn fresh(_: &Path) -> bool {
        true
    }
    fn create(_: &Path) -> Arc<Self> {
        Arc::new(Self {
            interfaces: OnceLock::new(),
            context: vec![
                Bytes::from_static(b"target-first"),
                Bytes::new(),
                Bytes::from_static(b"target-last"),
            ],
            items: Mutex::new(vec![Bytes::from_static(b"snapshot"), Bytes::new()]),
            observed_context: Mutex::new(Vec::new()),
            acquisitions: AtomicUsize::new(0),
            context_calls: AtomicUsize::new(0),
            context_entered: Semaphore::new(0),
            context_release: Semaphore::new(1),
            copy_entered: Semaphore::new(0),
            copy_release: Semaphore::new(1),
            close_calls: AtomicUsize::new(0),
            block_role: AtomicBool::new(false),
            role_entered: Semaphore::new(0),
            role_release: Semaphore::new(0),
        })
    }
    async fn drain(&self) -> Result<()> {
        if let Some(interfaces) = self.interfaces.get() {
            interfaces.replicator().close().await
        } else {
            Ok(())
        }
    }
}

impl EngineService {
    fn state(&self) -> Arc<dyn StateReplicator> {
        self.interfaces.get().unwrap().state_replicator().unwrap()
    }
    #[allow(
        clippy::disallowed_types,
        reason = "fault setup/replay on the same ordinarily created native engine"
    )]
    fn primary(&self) -> Arc<dyn PrimaryReplicator> {
        self.interfaces.get().unwrap().primary_replicator().unwrap()
    }
    fn control(&self) -> Arc<dyn Replicator> {
        self.interfaces.get().unwrap().replicator()
    }
}

#[derive(Default)]
pub(super) struct ResolverMap(pub(super) Mutex<BTreeMap<ReplicaId, String>>);
impl ReplicaEndpointResolver for ResolverMap {
    fn control_endpoint(&self, identity: &ReplicaIdentity) -> String {
        self.0
            .lock()
            .unwrap()
            .get(&identity.replica_id)
            .cloned()
            .unwrap_or_default()
    }
    fn replication_endpoint(&self, _: &ReplicaIdentity) -> String {
        String::new()
    }
}

struct Hook {
    held: Mutex<Option<Cut>>,
    entered: Semaphore,
    release: Semaphore,
    lost_retire: AtomicBool,
    calls: Mutex<Vec<Cut>>,
    fail: Mutex<Option<Cut>>,
}
impl Hook {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            held: Mutex::new(None),
            entered: Semaphore::new(0),
            release: Semaphore::new(0),
            lost_retire: AtomicBool::new(false),
            calls: Mutex::new(Vec::new()),
            fail: Mutex::new(None),
        })
    }
    fn hold(&self, cut: Cut) {
        self.entered.forget_permits(usize::MAX);
        *self.held.lock().unwrap() = Some(cut);
    }
    fn open(&self) {
        *self.held.lock().unwrap() = None;
        self.release.add_permits(1024);
    }
    fn register(self: &Arc<Self>, instance: String) -> TestingTcpRoute {
        let hook = self.clone();
        TestingTcpRoute::register_with_kind(instance, move |kind, _, endpoint| {
            let hook = hook.clone();
            Box::pin(async move {
                hook.calls.lock().unwrap().push(kind);
                if *hook.fail.lock().unwrap() == Some(kind) {
                    return Err(std::io::Error::other("failed actual transport descendant"));
                }
                let held = *hook.held.lock().unwrap() == Some(kind);
                if held {
                    hook.entered.add_permits(1);
                    hook.release.acquire().await.unwrap().forget();
                }
                if kind == Cut::EngineRetire && hook.lost_retire.load(Ordering::SeqCst) {
                    // The actual dial goes to an unbound port: no target handler runs.
                    Ok("http://127.0.0.1:1".into())
                } else {
                    Ok(endpoint)
                }
            })
        })
        .unwrap()
    }
}

struct Pair {
    source: Node<EngineService>,
    target: Node<EngineService>,
    source_goal: ReplicaGoal,
    target_goal: ReplicaGoal,
    source_hook: Arc<Hook>,
    target_hook: Arc<Hook>,
    source_route: TestingTcpRoute,
    target_route: TestingTcpRoute,
    resolver: Arc<ResolverMap>,
    _root: tempfile::TempDir,
}

async fn entered(semaphore: &Semaphore) {
    tokio::time::timeout(Duration::from_secs(10), semaphore.acquire())
        .await
        .unwrap()
        .unwrap()
        .forget();
}

async fn until(mut check: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(10), async {
        while !check() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

pub(super) async fn snapshot<A: Sample>(node: &Node<A>) -> ReplicaAgentSnapshot {
    snapshot_sampling(node, false).await
}

pub(super) async fn snapshot_sampling<A: Sample>(
    node: &Node<A>,
    sample_native_progress: bool,
) -> ReplicaAgentSnapshot {
    let mut client =
        proto::agent_control_client::AgentControlClient::connect(node.control_address.clone())
            .await
            .unwrap();
    let response = client
        .get_snapshot(authorized(proto::ReplicaSnapshotRequest {
            protocol_version: ORDINARY_ACTION_FORMAT,
            resource_uid: node.history.resource_uid.to_string(),
            target: Some(node.identity.clone().into()),
            expected_process_session_id: node.session.to_string(),
            sample_native_progress,
        }))
        .await
        .unwrap()
        .into_inner();
    let ReplicaAgentQueryResult::Initialized(snapshot) =
        serde_json::from_slice(&response.canonical_json).unwrap()
    else {
        panic!("initialized ordinary host required")
    };
    *snapshot
}

pub(super) async fn submit<A: Sample>(
    node: &Node<A>,
    goal: &ReplicaGoal,
) -> std::result::Result<(), tonic::Status> {
    let mut client =
        proto::agent_control_client::AgentControlClient::connect(node.control_address.clone())
            .await
            .unwrap();
    client
        .submit_goal(authorized(proto::ReplicaGoalRequest {
            protocol_version: ORDINARY_ACTION_FORMAT,
            canonical_json: serde_json::to_vec(goal).unwrap(),
        }))
        .await
        .map(|_| ())
}

pub(super) async fn settled<A: Sample>(node: &Node<A>, goal: &ReplicaGoal) -> ReplicaGoalStatus {
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let snapshot = snapshot(node).await;
            if let Some(observation) = snapshot
                .goals
                .iter()
                .find(|observation| observation.goal == *goal)
                && observation.status != ReplicaGoalStatus::Pending
            {
                return observation.status.clone();
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("ordinary goal did not settle: {goal:?}"))
}

impl Pair {
    async fn new() -> Self {
        Self::new_sized(2).await
    }

    async fn new_sized(replicas: u32) -> Self {
        let root = tempfile::tempdir().unwrap();
        let source_hook = Hook::new();
        let target_hook = Hook::new();
        let source_route = source_hook.register("pod-1".into());
        let target_route = target_hook.register("pod-2".into());
        let resolver = Arc::new(ResolverMap::default());
        let source = Node::start_resolved(
            &root.path().join("source"),
            1,
            0,
            replicas,
            resolver.clone(),
        )
        .await;
        let target = Node::start_resolved(
            &root.path().join("target"),
            2,
            0,
            replicas,
            resolver.clone(),
        )
        .await;
        resolver.0.lock().unwrap().extend([
            (source.identity.replica_id, source.control_address.clone()),
            (target.identity.replica_id, target.control_address.clone()),
        ]);
        let configuration = ReplicaConfigurationScope {
            current: ConfigurationDescriptor::new(
                Epoch::new(0, 1),
                source.identity.replica_id,
                vec![ConfigurationMember {
                    identity: source.identity.clone(),
                    role: ReplicaRole::Primary,
                }],
                1,
            ),
            previous: None,
            replicas: Vec::new(),
        };
        let mut goals = Vec::new();
        for (node, role) in [
            (&source, ReplicaRole::Primary),
            (&target, ReplicaRole::IdleSecondary),
        ] {
            let goal = ReplicaGoal {
                format: ORDINARY_ACTION_FORMAT,
                identity: node.preview.clone(),
                operation_id: OperationId::new("prepare"),
                revision: 1,
                scope: ReplicaScope {
                    history: node.history.clone(),
                    storage: node.storage.clone(),
                    process_session_id: node.session.clone(),
                    creation_id: snapshot(node).await.creation_id,
                    epoch: Epoch::new(0, 1),
                    configuration: configuration.clone(),
                },
                desired: ReplicaGoalKind::Prepare {
                    role,
                    rebuild: None,
                },
            };
            submit(node, &goal).await.unwrap();
            assert_eq!(settled(node, &goal).await, ReplicaGoalStatus::Achieved);
            let observation = snapshot(node).await;
            assert!(observation.provider_writer_held);
            assert_eq!(
                observation.publication.unwrap().stage,
                ReplicaPublicationStage::AchievedScope
            );
            assert_ne!(observation.write_status, AccessStatus::Granted);
            goals.push(goal);
        }
        let target_goal = goals.pop().unwrap();
        let source_goal = goals.pop().unwrap();
        Self {
            source,
            target,
            source_goal,
            target_goal,
            source_hook,
            target_hook,
            source_route,
            target_route,
            resolver,
            _root: root,
        }
    }

    async fn build(&self) -> ReplicaGoal {
        let mut goal = self.source_goal.clone();
        goal.operation_id = OperationId::new("build");
        goal.revision = 2;
        goal.desired = ReplicaGoalKind::Build {
            remote: Box::new(RemoteReplicaSelection {
                original_member: None,
                source: snapshot(&self.source).await.publication.unwrap(),
                target: snapshot(&self.target).await.publication.unwrap(),
                target_control_address: self.target.control_address.clone(),
                generation: 1,
            }),
        };
        submit(&self.source, &goal).await.unwrap();
        goal
    }

    async fn copy(&self) -> Vec<ReceivedOperation> {
        let mut stream = self.target.service.state().copy_operations().unwrap();
        let mut operations = Vec::new();
        loop {
            let operation = next(&mut stream).await;
            let last = operation.end_of_stream;
            operation.acknowledge().unwrap();
            operations.push(operation);
            if last {
                break;
            }
        }
        operations
    }

    fn binding(&self) -> serde_json::Value {
        self.source_route.inspect_native().unwrap()["sessions"]["entries"][0]["binding"].clone()
    }

    async fn remove(&self) -> Result<()> {
        self.source
            .service
            .primary()
            .remove_replica(self.target.identity.replica_id)
            .await
    }

    async fn admit(&self, build: &ReplicaGoal) {
        let mut client = proto::agent_control_client::AgentControlClient::connect(
            self.source.control_address.clone(),
        )
        .await
        .unwrap();
        client
            .acknowledge_goal(authorized(proto::ReplicaGoalRequest {
                protocol_version: ORDINARY_ACTION_FORMAT,
                canonical_json: serde_json::to_vec(&ReplicaGoalAcknowledgement {
                    goal: build.clone(),
                    logically_retired: false,
                })
                .unwrap(),
            }))
            .await
            .unwrap();
        let publication = snapshot(&self.target).await.publication.unwrap();
        let current = ConfigurationDescriptor::new(
            self.source_goal.scope.epoch,
            self.source.identity.replica_id,
            vec![
                ConfigurationMember {
                    identity: self.source.identity.clone(),
                    role: ReplicaRole::Primary,
                },
                ConfigurationMember {
                    identity: self.target.identity.clone(),
                    role: ReplicaRole::ActiveSecondary,
                },
            ],
            2,
        );
        let mut scope = ReplicaConfigurationScope {
            current,
            previous: Some(self.source_goal.scope.configuration.current.clone()),
            replicas: vec![ReplicaPeerDescription {
                identity: self.target.identity.clone(),
                replication_address: publication.replication_address,
                process_session_id: self.target.session.clone(),
                storage_incarnation: self.target.storage.clone(),
                role: ReplicaRole::ActiveSecondary,
                current_progress: 0,
                catch_up_capability: 0,
                is_up: true,
                is_ready: true,
                must_catchup: false,
            }],
        };
        let source_publication = snapshot(&self.source).await.publication.unwrap();
        let primary_peer = ReplicaPeerDescription {
            identity: self.source.identity.clone(),
            replication_address: source_publication.replication_address,
            process_session_id: self.source.session.clone(),
            storage_incarnation: self.source.storage.clone(),
            role: ReplicaRole::Primary,
            current_progress: 0,
            catch_up_capability: 0,
            is_up: true,
            is_ready: true,
            must_catchup: false,
        };
        for (node, base, revision, role) in [
            (&self.source, &self.source_goal, 3, ReplicaRole::Primary),
            (
                &self.target,
                &self.target_goal,
                2,
                ReplicaRole::ActiveSecondary,
            ),
        ] {
            let mut goal = base.clone();
            goal.operation_id = OperationId::new("joint");
            goal.revision = revision;
            goal.scope.configuration = scope.clone();
            if role == ReplicaRole::ActiveSecondary {
                goal.scope.configuration.replicas = vec![primary_peer.clone()];
            }
            goal.desired = ReplicaGoalKind::Reconfigure {
                role,
                catch_up: None,
            };
            submit(node, &goal).await.unwrap();
            assert_eq!(settled(node, &goal).await, ReplicaGoalStatus::Achieved);
        }
        scope.previous = None;
        for (node, base, revision, role) in [
            (&self.source, &self.source_goal, 4, ReplicaRole::Primary),
            (
                &self.target,
                &self.target_goal,
                3,
                ReplicaRole::ActiveSecondary,
            ),
        ] {
            let mut goal = base.clone();
            goal.operation_id = OperationId::new("current");
            goal.revision = revision;
            goal.scope.configuration = scope.clone();
            if role == ReplicaRole::ActiveSecondary {
                goal.scope.configuration.replicas = vec![primary_peer.clone()];
            }
            goal.desired = ReplicaGoalKind::Reconfigure {
                role,
                catch_up: (role == ReplicaRole::Primary).then_some(PublicCatchUpMode::WriteQuorum),
            };
            submit(node, &goal).await.unwrap();
            assert_eq!(settled(node, &goal).await, ReplicaGoalStatus::Achieved);
        }
        assert_eq!(
            snapshot(&self.source).await.write_status,
            AccessStatus::Granted
        );
    }

    async fn finish(mut self) {
        self.source_hook.open();
        self.target_hook.open();
        self.source.service.copy_release.add_permits(1024);
        self.target.service.context_release.add_permits(1024);
        self.target.service.role_release.add_permits(1024);
        self.source.service.control().abort();
        self.target.service.control().abort();
        for node in [&self.source, &self.target] {
            let control = node.service.control();
            let first = control.close().await.map_err(|error| error.to_string());
            assert_eq!(
                control.close().await.map_err(|error| error.to_string()),
                first
            );
        }
        assert_eq!(self.source_route.inspect_native().unwrap()["tasks"], 0);
        assert_eq!(self.target_route.inspect_native().unwrap()["tasks"], 0);
        self.source.running.shutdown();
        self.target.running.shutdown();
        let source = self.source.running.wait().await;
        let target = self.target.running.wait().await;
        if let Err(error) = source {
            assert!(error.to_string().contains("Close failed"), "{error}");
        }
        if let Err(error) = target {
            assert!(error.to_string().contains("Close failed"), "{error}");
        }
    }
}

async fn next(stream: &mut OperationStream) -> ReceivedOperation {
    tokio::time::timeout(Duration::from_secs(10), stream.get_operation())
        .await
        .unwrap()
        .unwrap()
        .unwrap()
}

pub(super) async fn context_and_ack_boundaries() {
    let pair = Pair::new().await;
    let build = pair.build().await;
    let mut stream = pair.target.service.state().copy_operations().unwrap();
    let first = next(&mut stream).await;
    assert_eq!(first.data, Bytes::from_static(b"snapshot"));
    assert!(!first.end_of_stream);
    assert_eq!(pair.source.service.acquisitions.load(Ordering::SeqCst), 1);
    assert_eq!(pair.target.service.context_calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        *pair.source.service.observed_context.lock().unwrap(),
        pair.target.service.context
    );
    first.acknowledge().unwrap();
    assert!(first.clone().acknowledge().is_err());
    let mut second_handle = pair.target.service.state().copy_operations().unwrap();
    let empty = next(&mut second_handle).await;
    assert!(empty.data.is_empty() && !empty.end_of_stream);
    empty.acknowledge().unwrap();
    let eof = next(&mut stream).await;
    assert!(eof.end_of_stream && eof.data.is_empty());
    tokio::time::sleep(Duration::from_millis(40)).await;
    assert!(
        snapshot(&pair.source)
            .await
            .remote_operations
            .iter()
            .all(|op| op.state != RemoteReplicaState::Ready)
    );
    eof.acknowledge().unwrap();
    assert_eq!(
        settled(&pair.source, &build).await,
        ReplicaGoalStatus::Achieved
    );
    assert!(
        snapshot(&pair.source)
            .await
            .remote_operations
            .iter()
            .any(|op| op.state == RemoteReplicaState::Ready)
    );
    assert!(first.acknowledge().is_err());
    assert_eq!(
        snapshot(&pair.target).await.publication.unwrap().role,
        ReplicaRole::IdleSecondary
    );
    pair.admit(&build).await;
    let mut replication = pair
        .target
        .service
        .state()
        .replication_operations()
        .unwrap();
    let write = pair
        .source
        .service
        .state()
        .begin_replicate(Bytes::from_static(b"replicated"))
        .unwrap();
    let lsn = write.lsn;
    let completion = tokio::spawn(write.completion);
    let operation = next(&mut replication).await;
    assert_eq!(operation.lsn, lsn);
    assert_eq!(operation.data, Bytes::from_static(b"replicated"));
    tokio::time::sleep(Duration::from_millis(40)).await;
    assert!(
        !completion.is_finished(),
        "receive is not application/durable ACK credit"
    );
    operation.acknowledge().unwrap();
    tokio::time::timeout(Duration::from_secs(10), completion)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(operation.clone().acknowledge().is_err());
    let mut repeated = pair
        .target
        .service
        .state()
        .replication_operations()
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(40), repeated.get_operation())
            .await
            .is_err(),
        "a fresh stream handle replayed already acknowledged work"
    );
    assert_ne!(
        snapshot(&pair.target).await.write_status,
        AccessStatus::Granted
    );
    assert!(pair.target_route.inspect_native().unwrap()["failure"].is_null());
    pair.finish().await;
}

async fn rejected_application_copy<A: Sample>(
    source: Node<A>,
    target: Node<ExternalService>,
    source_root: &Path,
    target_root: &Path,
    source_route: TestingTcpRoute,
    target_route: TestingTcpRoute,
    expected: &str,
) {
    let bytes = std::fs::read(target_root.join("application/state.json")).unwrap();
    let source_snapshot = snapshot(&source).await;
    let target_snapshot = snapshot(&target).await;
    let publication = source_snapshot.publication.unwrap();
    let build = ReplicaGoal {
        format: ORDINARY_ACTION_FORMAT,
        identity: source.preview.clone(),
        operation_id: OperationId::new("rejected-real-application-copy"),
        revision: source_snapshot
            .goals
            .iter()
            .map(|entry| entry.goal.revision)
            .max()
            .unwrap()
            + 1,
        scope: publication.scope.clone(),
        desired: ReplicaGoalKind::Build {
            remote: Box::new(RemoteReplicaSelection {
                original_member: None,
                source: publication,
                target: target_snapshot.publication.unwrap(),
                target_control_address: target.control_address.clone(),
                generation: 1,
            }),
        },
    };
    submit(&source, &build).await.unwrap();
    let rejected = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if snapshot(&target).await.publication.is_none() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await;
    if let Err(error) = rejected {
        let source_snapshot = snapshot(&source).await;
        let target_snapshot = snapshot(&target).await;
        panic!(
            "application copy rejection did not withdraw: {error}; source={source_snapshot:?}; target={target_snapshot:?}; source-native={}; target-native={}",
            source_route.inspect_native().unwrap(),
            target_route.inspect_native().unwrap(),
        );
    }
    assert_eq!(
        std::fs::read(target_root.join("application/state.json")).unwrap(),
        bytes
    );
    let fenced = snapshot(&target).await;
    assert!(fenced.publication.is_none() && fenced.provider_writer_held);
    assert_ne!(fenced.read_status, AccessStatus::Granted);
    assert_ne!(fenced.write_status, AccessStatus::Granted);
    assert!(
        snapshot(&source)
            .await
            .remote_operations
            .iter()
            .all(|remote| remote.state != RemoteReplicaState::Ready)
    );
    let mut successor = target_snapshot.goals.last().unwrap().goal.clone();
    successor.operation_id = OperationId::new("forbidden-receiver-reuse");
    successor.revision += 1;
    assert!(submit(&target, &successor).await.is_err());
    assert!(
        target
            .service
            .begin_write(Bytes::from_static(b"late"))
            .await
            .is_err()
    );
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(error) = target.service.stream_failure().await {
                assert!(error.contains(expected), "{error}");
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the actual failed consumer outcome remains owned before Abort");
    let target_service = target.service.clone();
    let target_handle = target.running.handle();
    let error = target
        .stop()
        .await
        .expect("failed application Close must remain negative")
        .to_string();
    assert!(error.contains(expected), "{error}");
    assert_eq!(target_handle.public_v1_provider_writer_count(), 1);
    assert_eq!(target_route.inspect_native().unwrap()["tasks"], 0);
    let repeated = target_service
        .shutdown_and_drain()
        .await
        .unwrap_err()
        .to_string();
    assert!(repeated.contains(expected), "{repeated}");
    assert_eq!(
        std::fs::read(target_root.join("application/state.json")).unwrap(),
        bytes
    );
    let source_handle = source.running.handle();
    assert!(source.stop().await.is_none());
    assert_eq!(source_handle.public_v1_provider_writer_count(), 0);
    assert_eq!(source_route.inspect_native().unwrap()["tasks"], 0);
    let store = kuberic_runtime::testing::sqlite_store::SqliteStore::open_public_v1(
        kuberic_runtime::testing::sqlite_store::SqliteStore::metadata_database_path(source_root),
        &build.identity,
    )
    .unwrap();
    let record = store
        .public_v1_operation_snapshot(&build.operation_id)
        .await
        .unwrap();
    use kuberic_runtime::testing::sqlite_store::PublicV1OperationSnapshotDisposition as D;
    assert!(
        matches!(
            record.disposition,
            Some(D::Failed(_) | D::Cancelled | D::Ambiguous(_))
        ),
        "application failure became a successful Build receipt: {record:?}"
    );
}

pub(super) async fn application_copy_failures() {
    let root = tempfile::tempdir().unwrap();
    let source_root = root.path().join("source");
    let target_root = root.path().join("target");
    let mut source = Node::<EngineService>::start(&source_root, 1, 0).await;
    let mut target = Node::<ExternalService>::start(&target_root, 2, 0).await;
    let source_route = Hook::new().register(source.identity.instance_id.to_string());
    let target_route = Hook::new().register(target.identity.instance_id.to_string());
    source.open(ReplicaRole::Primary, Epoch::new(0, 1)).await;
    target
        .open(ReplicaRole::IdleSecondary, Epoch::new(0, 1))
        .await;
    rejected_application_copy(
        source,
        target,
        &source_root,
        &target_root,
        source_route,
        target_route,
        "expected value",
    )
    .await;

    let root = tempfile::tempdir().unwrap();
    let source_root = root.path().join("source");
    let target_root = root.path().join("target");
    let mut predecessor =
        Node::<ExternalService>::start(&root.path().join("predecessor"), 1, 0).await;
    let mut retained = Node::<ExternalService>::start(&target_root, 2, 0).await;
    predecessor
        .open(ReplicaRole::Primary, Epoch::new(0, 1))
        .await;
    let singleton = predecessor.single(Epoch::new(0, 1)).await;
    retained
        .open(ReplicaRole::IdleSecondary, Epoch::new(0, 1))
        .await;
    predecessor
        .build(&mut retained, Epoch::new(0, 1), &singleton)
        .await;
    predecessor
        .service
        .begin_write(Bytes::from_static(b"retained-target"))
        .await
        .unwrap()
        .wait()
        .await
        .unwrap();
    assert_eq!(
        ExternalService::read_persisted_snapshot(&target_root.join("application"))
            .unwrap()
            .values,
        vec![Bytes::from_static(b"retained-target")],
    );
    predecessor.close().await;
    retained.close().await;
    let mut source = Node::<ExternalService>::start_sized(&source_root, 1, 0, 1).await;
    let mut target = Node::<ExternalService>::start(&target_root, 2, 0).await;
    let source_route = Hook::new().register(source.identity.instance_id.to_string());
    let target_route = Hook::new().register(target.identity.instance_id.to_string());
    source.open(ReplicaRole::Primary, Epoch::new(0, 1)).await;
    source.single(Epoch::new(0, 1)).await;
    source
        .service
        .begin_write(Bytes::from_static(b"source-prefix"))
        .await
        .unwrap()
        .wait()
        .await
        .unwrap();
    target
        .open(ReplicaRole::IdleSecondary, Epoch::new(0, 1))
        .await;
    rejected_application_copy(
        source,
        target,
        &source_root,
        &target_root,
        source_route,
        target_route,
        "copy conflicts with acknowledged service prefix",
    )
    .await;
}

pub(super) async fn exact_replay_and_retirement() {
    let pair = Pair::new().await;
    let build = pair.build().await;
    pair.copy().await;
    assert_eq!(
        settled(&pair.source, &build).await,
        ReplicaGoalStatus::Achieved
    );
    let binding = pair.binding();
    submit(&pair.source, &build).await.unwrap();
    assert_eq!(
        settled(&pair.source, &build).await,
        ReplicaGoalStatus::Achieved
    );
    assert_eq!(pair.binding(), binding);
    control(&pair, binding.clone(), "Reserve").await.unwrap();
    let mut information = ReplicaInformation::idle_build(
        pair.target.identity.clone(),
        binding["target"]["replicationAddress"]
            .as_str()
            .unwrap()
            .into(),
    );
    information.process_session_id = pair.target.session.clone();
    information.storage_incarnation = Some(pair.target.storage.clone());
    pair.source
        .service
        .primary()
        .build_replica(information)
        .await
        .unwrap();
    assert_eq!(pair.source.service.acquisitions.load(Ordering::SeqCst), 1);
    pair.remove().await.unwrap();
    assert_eq!(
        pair.source_route.inspect_native().unwrap()["sessions"]["entries"][0]["phase"],
        "Retired"
    );
    assert!(control(&pair, binding.clone(), "Reserve").await.is_err());
    assert!(control(&pair, binding.clone(), "Ack").await.is_err());
    assert!(control(&pair, binding, "Complete").await.is_err());
    pair.finish().await;
}

pub(super) async fn delayed_first_establishment() {
    let pair = Pair::new().await;
    pair.source_hook.hold(Cut::EngineReserve);
    let build = pair.build().await;
    entered(&pair.source_hook.entered).await;
    let binding = pair.binding();
    pair.remove().await.unwrap();
    assert_eq!(
        pair.source_route.inspect_native().unwrap()["lifecycle"][1],
        "primary"
    );
    assert_eq!(
        pair.source
            .service
            .control()
            .current_progress()
            .await
            .unwrap(),
        0
    );
    pair.source_hook.open();
    assert!(matches!(
        settled(&pair.source, &build).await,
        ReplicaGoalStatus::Failed(_) | ReplicaGoalStatus::Uncertain(_)
    ));
    assert_eq!(pair.target.service.context_calls.load(Ordering::SeqCst), 0);
    assert_eq!(pair.source.service.acquisitions.load(Ordering::SeqCst), 0);
    assert!(control(&pair, binding, "Reserve").await.is_err());
    pair.finish().await;
}

pub(super) async fn lost_cancel() {
    let pair = Pair::new().await;
    pair.source_hook.hold(Cut::BuildResult);
    pair.target.service.context_release.forget_permits(1);
    let build = pair.build().await;
    entered(&pair.target.service.context_entered).await;
    pair.source_hook.lost_retire.store(true, Ordering::SeqCst);
    assert!(pair.remove().await.is_err());
    entered(&pair.source_hook.entered).await;
    assert_eq!(
        pair.source_route.inspect_native().unwrap()["lifecycle"][1],
        "primary"
    );
    assert_eq!(
        pair.source
            .service
            .control()
            .current_progress()
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        pair.source_route.inspect_native().unwrap()["sessions"]["closed"],
        false
    );
    let before = pair.target_route.inspect_native().unwrap();
    assert_eq!(before["sessions"]["entries"][0]["phase"], "Active");
    assert!(snapshot(&pair.target).await.provider_writer_held);
    pair.target.service.context_release.add_permits(1);
    until(|| {
        pair.target_route.inspect_native().unwrap()["sessions"]["entries"][0]["phase"] == "Retiring"
    })
    .await;
    let mut successor = pair.binding();
    successor["nonce"] = uuid::Uuid::new_v4().to_string().into();
    assert!(control(&pair, successor, "Reserve").await.is_err());
    assert_eq!(pair.target.service.context_calls.load(Ordering::SeqCst), 1);
    assert!(snapshot(&pair.target).await.provider_writer_held);
    assert!(
        snapshot(&pair.source)
            .await
            .remote_operations
            .iter()
            .all(|op| op.state != RemoteReplicaState::Ready)
    );
    pair.source_hook.lost_retire.store(false, Ordering::SeqCst);
    assert_eq!(
        pair.source
            .service
            .control()
            .current_progress()
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        pair.source_route.inspect_native().unwrap()["sessions"]["closed"],
        false
    );
    pair.source_hook.open();
    assert!(matches!(
        settled(&pair.source, &build).await,
        ReplicaGoalStatus::Failed(_) | ReplicaGoalStatus::Uncertain(_)
    ));
    pair.finish().await;
}

pub(super) async fn effect_fences() {
    for cut in [
        Cut::BeforeDispatch,
        Cut::AfterContext,
        Cut::BeforeDelivery,
        Cut::BeforeCredit,
    ] {
        let pair = Pair::new().await;
        pair.source_hook.lost_retire.store(true, Ordering::SeqCst);
        pair.target_hook.hold(cut);
        let build = pair.build().await;
        let delivery = if cut == Cut::BeforeDelivery {
            let mut stream = pair.target.service.state().copy_operations().unwrap();
            Some(tokio::spawn(async move { stream.get_operation().await }))
        } else {
            None
        };
        if cut == Cut::BeforeCredit {
            let mut stream = pair.target.service.state().copy_operations().unwrap();
            next(&mut stream).await.acknowledge().unwrap();
        }
        entered(&pair.target_hook.entered).await;
        assert!(pair.remove().await.is_err());
        pair.target_hook.open();
        if let Some(delivery) = delivery {
            assert!(
                delivery.await.unwrap().is_err(),
                "no stale application delivery"
            );
        }
        until(|| {
            pair.target_route.inspect_native().unwrap()["sessions"]["entries"][0]["phase"]
                == "Retiring"
        })
        .await;
        if cut == Cut::BeforeDispatch {
            assert_eq!(pair.target.service.context_calls.load(Ordering::SeqCst), 0);
        }
        assert!(
            snapshot(&pair.source)
                .await
                .remote_operations
                .iter()
                .all(|op| op.state != RemoteReplicaState::Ready)
        );
        let mut successor = pair.binding();
        successor["nonce"] = uuid::Uuid::new_v4().to_string().into();
        assert!(control(&pair, successor, "Reserve").await.is_err());
        pair.source_hook.lost_retire.store(false, Ordering::SeqCst);
        assert!(matches!(
            settled(&pair.source, &build).await,
            ReplicaGoalStatus::Failed(_) | ReplicaGoalStatus::Uncertain(_)
        ));
        pair.finish().await;
    }
    let pair = Pair::new().await;
    pair.source.service.copy_release.forget_permits(1);
    let build = pair.build().await;
    entered(&pair.source.service.copy_entered).await;
    let primary = pair.source.service.primary();
    let remove = tokio::spawn(async move { primary.remove_replica(ReplicaId::new(2)).await });
    until(|| {
        pair.source_route.inspect_native().unwrap()["sessions"]["entries"][0]["admitted"] == false
    })
    .await;
    let control = pair.source.service.control();
    control.abort();
    let close = tokio::spawn(async move { control.close().await });
    tokio::time::sleep(Duration::from_millis(40)).await;
    assert!(
        !close.is_finished(),
        "issued opaque source acquisition is a retained join"
    );
    assert!(snapshot(&pair.source).await.provider_writer_held);
    pair.source.service.copy_release.add_permits(1);
    let _ = tokio::time::timeout(Duration::from_secs(10), remove)
        .await
        .unwrap()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), close)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(matches!(
        settled(&pair.source, &build).await,
        ReplicaGoalStatus::Failed(_) | ReplicaGoalStatus::Uncertain(_)
    ));
    assert!(
        snapshot(&pair.source)
            .await
            .remote_operations
            .iter()
            .all(|op| op.state != RemoteReplicaState::Ready)
    );
    assert_eq!(pair.source.service.acquisitions.load(Ordering::SeqCst), 1);
    pair.finish().await;
}

pub(super) async fn tombstone_bounds() {
    let pair = Pair::new().await;
    let build = pair.build().await;
    pair.copy().await;
    assert_eq!(
        settled(&pair.source, &build).await,
        ReplicaGoalStatus::Achieved
    );
    let binding = pair.binding();
    for _ in 1..256 {
        let mut tombstone = binding.clone();
        tombstone["nonce"] = uuid::Uuid::new_v4().to_string().into();
        control(&pair, tombstone, "Retire").await.unwrap();
    }
    let before = pair.target_route.inspect_native().unwrap()["sessions"].clone();
    assert_eq!(before["entries"].as_array().unwrap().len(), 256);
    let mut excess = binding.clone();
    excess["nonce"] = uuid::Uuid::new_v4().to_string().into();
    assert!(control(&pair, excess.clone(), "Retire").await.is_err());
    assert!(control(&pair, excess, "Reserve").await.is_err());
    assert_eq!(
        pair.target_route.inspect_native().unwrap()["sessions"],
        before
    );
    control(&pair, binding.clone(), "Complete").await.unwrap();
    pair.remove().await.unwrap();
    assert!(control(&pair, binding, "Reserve").await.is_err());
    pair.target.service.control().abort();
    assert_eq!(
        pair.target_route.inspect_native().unwrap()["sessions"]["entries"]
            .as_array()
            .unwrap()
            .len(),
        256
    );
    pair.finish().await;
}

pub(super) async fn pending_final_credit() {
    let pair = Pair::new().await;
    let build = pair.build().await;
    let mut stream = pair.target.service.state().copy_operations().unwrap();
    next(&mut stream).await.acknowledge().unwrap();
    next(&mut stream).await.acknowledge().unwrap();
    let eof = next(&mut stream).await;
    assert!(eof.end_of_stream);
    pair.target.service.block_role.store(true, Ordering::SeqCst);
    let mut reconfigure = pair.target_goal.clone();
    reconfigure.operation_id = OperationId::new("held-role-before-achieved");
    reconfigure.revision = 2;
    reconfigure.desired = ReplicaGoalKind::Reconfigure {
        role: ReplicaRole::IdleSecondary,
        catch_up: None,
    };
    submit(&pair.target, &reconfigure).await.unwrap();
    entered(&pair.target.service.role_entered).await;
    assert_eq!(
        snapshot(&pair.target).await.publication.unwrap().stage,
        ReplicaPublicationStage::DiscoveryPending
    );
    eof.acknowledge().unwrap();
    // The native final ACK is not a two-ended achieved receipt.
    assert!(matches!(
        settled(&pair.source, &build).await,
        ReplicaGoalStatus::Failed(_) | ReplicaGoalStatus::Uncertain(_)
    ));
    assert!(
        snapshot(&pair.source)
            .await
            .remote_operations
            .iter()
            .all(|op| op.state != RemoteReplicaState::Ready)
    );
    pair.target.service.role_release.add_permits(1);
    assert_eq!(
        settled(&pair.target, &reconfigure).await,
        ReplicaGoalStatus::Achieved
    );
    assert!(submit(&pair.source, &build).await.is_err());
    pair.finish().await;
}

async fn control(
    pair: &Pair,
    binding: serde_json::Value,
    command: &str,
) -> std::result::Result<(), tonic::Status> {
    control_at(&pair.binding(), binding, command).await
}

pub(super) async fn control_at(
    destination_binding: &serde_json::Value,
    binding: serde_json::Value,
    command: &str,
) -> std::result::Result<(), tonic::Status> {
    exchange_at(destination_binding, binding, command)
        .await
        .map(|_| ())
}

fn control_frame(binding: &serde_json::Value, command: &str) -> native_wire::Envelope {
    use native_wire::{Endpoint, EngineSessionControl, Envelope, envelope::Body};
    use sha2::{Digest, Sha256};
    fn endpoint(publication: &serde_json::Value) -> Endpoint {
        let storage: ReplicaStorageIncarnationContext =
            serde_json::from_value(publication["scope"]["storage"].clone()).unwrap();
        let resource = publication["scope"]["history"]["resourceUid"]
            .as_str()
            .unwrap();
        let json = serde_json::json!({
            "identity": storage.replica,
            "processSessionId": publication["scope"]["processSessionId"],
            "storage": storage,
            "creationId": publication["scope"]["creationId"],
            "address": publication["replicationAddress"],
        });
        fn guid(domain: &[u8], input: &[u8]) -> Bytes {
            let mut hash = Sha256::new();
            hash.update((domain.len() as u64).to_be_bytes());
            hash.update(domain);
            hash.update(input);
            Bytes::copy_from_slice(&hash.finalize()[..16])
        }
        // Endpoint serialization is a Rust struct, whose declaration order is canonical.
        #[derive(serde::Serialize)]
        #[serde(rename_all = "camelCase")]
        struct Identity<'a> {
            identity: &'a ReplicaIdentity,
            process_session_id: &'a serde_json::Value,
            storage: &'a ReplicaStorageIncarnationContext,
            creation_id: &'a serde_json::Value,
            address: &'a serde_json::Value,
        }
        let bytes = serde_json::to_vec(&Identity {
            identity: &storage.replica,
            process_session_id: &json["processSessionId"],
            storage: &storage,
            creation_id: &json["creationId"],
            address: &json["address"],
        })
        .unwrap();
        Endpoint {
            partition: guid(b"kuberic-v1-engine-partition", resource.as_bytes()),
            replica: storage.replica.replica_id.value(),
            incarnation: guid(b"kuberic-v1-engine-endpoint", &bytes),
        }
    }
    let reverse = command == "Confirm";
    let (from, to) = if reverse {
        ("target", "source")
    } else {
        ("source", "target")
    };
    let source = endpoint(&binding[from]);
    let destination = endpoint(&binding[to]);
    let epoch = native_wire::Epoch {
        data_loss: binding["source"]["scope"]["epoch"]["dataLossNumber"]
            .as_i64()
            .unwrap(),
        configuration: binding["source"]["scope"]["epoch"]["configurationNumber"]
            .as_i64()
            .unwrap(),
    };
    let stale = native_wire::Epoch {
        configuration: epoch.configuration - 1,
        ..epoch
    };
    let metadata = Some(native_wire::Metadata {
        sequence: 1,
        operation_type: 1,
        atomic_group: -1,
    });
    let body = match command {
        "Ack" => Body::ReplicationAck(native_wire::ReplicationAck {
            replication_received: 999,
            replication_applied: 999,
            copy_received: 999,
            copy_applied: 999,
            error: 0,
        }),
        "BodyStart" => Body::StartCopy(native_wire::StartCopy {
            epoch: Some(stale),
            replica: destination.replica,
            replication_start: 1,
        }),
        "BodyCopy" => Body::CopyOperation(native_wire::CopyOperation {
            epoch: Some(stale),
            metadata,
            replica: destination.replica,
            is_last: false,
            buffers: Some(native_wire::Buffers {
                segments: vec![Bytes::from_static(b"stale-copy")],
            }),
        }),
        "BodyReplicationSending" | "BodyReplicationOriginal" => {
            Body::ReplicationOperation(native_wire::ReplicationOperation {
                metadata,
                original_epoch: Some(if command == "BodyReplicationOriginal" {
                    native_wire::Epoch {
                        configuration: epoch.configuration + 1,
                        ..epoch
                    }
                } else {
                    epoch
                }),
                sending_epoch: Some(if command == "BodyReplicationSending" {
                    stale
                } else {
                    epoch
                }),
                buffers: Some(native_wire::Buffers {
                    segments: vec![Bytes::from_static(b"stale-replication")],
                }),
                first_sequence: 1,
                last_sequence: 1,
                last_in_batch: 1,
                buffer_counts: vec![1],
                completed_hint: 0,
            })
        }
        _ => Body::EngineSessionControl(EngineSessionControl {
            canonical_json: serde_json::to_vec(
                &serde_json::json!({"binding": binding, "command": command}),
            )
            .unwrap()
            .into(),
        }),
    };
    Envelope {
        version: 1,
        message_id: uuid::Uuid::new_v4().to_string(),
        source: Some(source),
        destination: Some(destination),
        sender_address: binding[from]["replicationAddress"].as_str().unwrap().into(),
        body: Some(body),
    }
}

async fn exchange_at(
    destination_binding: &serde_json::Value,
    binding: serde_json::Value,
    command: &str,
) -> std::result::Result<Option<native_wire::Envelope>, tonic::Status> {
    let frame = control_frame(&binding, command);
    let to = if command == "Confirm" {
        "source"
    } else {
        "target"
    };
    let mut request = authorized(futures::stream::iter([frame]));
    if command != "Ack" && !command.starts_with("Body") {
        request
            .metadata_mut()
            .insert("kuberic-engine-control", "1".parse().unwrap());
    }
    request.metadata_mut().insert_bin(
        "kuberic-engine-session-bin",
        tonic::metadata::MetadataValue::from_bytes(&serde_json::to_vec(&binding).unwrap()),
    );
    let address = destination_binding[to]["replicationAddress"]
        .as_str()
        .unwrap()
        .to_owned();
    let mut client = native_wire::carrier_client::CarrierClient::connect(address)
        .await
        .unwrap();
    let mut response = client.exchange(request).await?.into_inner();
    response.message().await
}

pub(super) async fn crossed_identity() {
    let pair = Pair::new().await;
    let build = pair.build().await;
    pair.copy().await;
    assert_eq!(
        settled(&pair.source, &build).await,
        ReplicaGoalStatus::Achieved
    );
    let binding = pair.binding();
    control(&pair, binding.clone(), "Complete").await.unwrap();
    let receiver = pair.target_route.inspect_native().unwrap()["sessions"].clone();
    for side in ["source", "target"] {
        for dimension in [
            "processSessionId",
            "creationId",
            "storageGeneration",
            "podUid",
            "pvcUid",
            "history",
            "resource",
            "protocol",
            "dataLoss",
            "endpoint",
            "epoch",
            "epochDataLoss",
            "configuration",
            "stage",
            "role",
            "replica",
            "instance",
            "agentGeneration",
        ] {
            let mut crossed = binding.clone();
            match dimension {
                "processSessionId" | "creationId" => {
                    crossed[side]["scope"][dimension] = "stale".into()
                }
                "storageGeneration" => {
                    crossed[side]["scope"]["storage"][dimension] = "stale".into()
                }
                "podUid" | "pvcUid" => {
                    let key = if dimension == "podUid" {
                        "pod_uid"
                    } else {
                        "pvc_uid"
                    };
                    assert!(!crossed[side]["scope"]["storage"]["storage"][key].is_null());
                    crossed[side]["scope"]["storage"]["storage"][key] = "stale".into();
                }
                "history" => crossed[side]["scope"]["history"]["historyId"] = "stale".into(),
                "resource" => crossed[side]["scope"]["history"]["resourceUid"] = "stale".into(),
                "protocol" => crossed[side]["scope"]["history"]["protocolGeneration"] = 99.into(),
                "dataLoss" => crossed[side]["scope"]["history"]["dataLossNumber"] = 99.into(),
                "endpoint" => crossed[side]["replicationAddress"] = "http://127.0.0.1:1".into(),
                "epoch" => crossed[side]["scope"]["epoch"]["configurationNumber"] = 99.into(),
                "epochDataLoss" => crossed[side]["scope"]["epoch"]["dataLossNumber"] = 99.into(),
                "configuration" => {
                    crossed[side]["scope"]["configuration"]["current"]["configurationId"] =
                        "stale".into()
                }
                "stage" => crossed[side]["stage"] = "DiscoveryPending".into(),
                "role" => crossed[side]["role"] = "none".into(),
                "replica" => crossed[side]["scope"]["storage"]["replica"]["replicaId"] = 99.into(),
                "instance" => {
                    crossed[side]["scope"]["storage"]["replica"]["instanceId"] = "stale".into()
                }
                _ => {
                    crossed[side]["scope"]["storage"]["replica"]["agentGeneration"] = "stale".into()
                }
            }
            assert!(
                control(&pair, crossed.clone(), "Reserve").await.is_err(),
                "{side}/{dimension}"
            );
            assert!(
                control(&pair, crossed, "Ack").await.is_err(),
                "ACK {side}/{dimension}"
            );
            assert_eq!(pair.binding(), binding);
            assert_eq!(
                pair.target_route.inspect_native().unwrap()["sessions"],
                receiver
            );
        }
    }
    // Unknown and retired nonces cannot use exact endpoint routing as ACK authority.
    let mut stale = binding.clone();
    stale["nonce"] = uuid::Uuid::new_v4().to_string().into();
    assert!(control(&pair, stale.clone(), "Complete").await.is_err());
    assert!(control(&pair, stale, "Ack").await.is_err());
    assert_eq!(
        pair.target
            .service
            .control()
            .current_progress()
            .await
            .unwrap(),
        0
    );
    assert!(pair.target_route.inspect_native().unwrap()["failure"].is_null());
    assert_eq!(pair.source.service.acquisitions.load(Ordering::SeqCst), 1);
    pair.finish().await;
}

pub(super) async fn bound_body_epochs() {
    for command in [
        "BodyStart",
        "BodyCopy",
        "BodyReplicationSending",
        "BodyReplicationOriginal",
    ] {
        let pair = Pair::new().await;
        let replication = command.starts_with("BodyReplication");
        if command == "BodyStart" {
            // Sessions are active, but the real source has not connected/sent StartCopy.
            pair.source_hook.hold(Cut::Build);
        } else if command == "BodyCopy" {
            // Legitimate StartCopy/context run; no legitimate copy data can arrive.
            pair.source.service.copy_release.forget_permits(1);
        }
        let build = pair.build().await;
        if command == "BodyStart" {
            entered(&pair.source_hook.entered).await;
        } else if command == "BodyCopy" {
            entered(&pair.source.service.copy_entered).await;
        } else {
            let copied = pair.copy().await;
            *pair.target.service.items.lock().unwrap() = copied
                .into_iter()
                .filter(|operation| !operation.end_of_stream)
                .map(|operation| operation.data)
                .collect();
            assert_eq!(
                settled(&pair.source, &build).await,
                ReplicaGoalStatus::Achieved
            );
            pair.admit(&build).await;
        }
        let mut copy = pair.target.service.state().copy_operations().unwrap();
        let mut operations = pair
            .target
            .service
            .state()
            .replication_operations()
            .unwrap();
        if replication {
            let write = pair
                .source
                .service
                .state()
                .begin_replicate(Bytes::from_static(b"admitted-prefix"))
                .unwrap();
            let completion = tokio::spawn(write.completion);
            let operation = next(&mut operations).await;
            assert_eq!(operation.data, Bytes::from_static(b"admitted-prefix"));
            pair.target
                .service
                .items
                .lock()
                .unwrap()
                .push(operation.data.clone());
            operation.acknowledge().unwrap();
            tokio::time::timeout(Duration::from_secs(10), completion)
                .await
                .unwrap()
                .unwrap()
                .unwrap();
        }
        let binding = pair.binding();
        let (sender, incoming) = tokio::sync::mpsc::channel(1);
        let mut request = authorized(tokio_stream::wrappers::ReceiverStream::new(incoming));
        request.metadata_mut().insert_bin(
            "kuberic-engine-session-bin",
            tonic::metadata::MetadataValue::from_bytes(&serde_json::to_vec(&binding).unwrap()),
        );
        let mut client = native_wire::carrier_client::CarrierClient::connect(
            binding["target"]["replicationAddress"]
                .as_str()
                .unwrap()
                .to_owned(),
        )
        .await
        .unwrap();
        let mut response = client.exchange(request).await.unwrap().into_inner();
        let epochs = if command == "BodyReplicationOriginal" {
            vec![(0, 2), (1, 1)]
        } else {
            vec![(0, 0), (0, 2), (1, 1)]
        };
        for (data_loss, configuration) in epochs {
            let mut frame = control_frame(&binding, command);
            let epoch = native_wire::Epoch {
                data_loss,
                configuration,
            };
            match frame.body.as_mut().unwrap() {
                native_wire::envelope::Body::StartCopy(body) => body.epoch = Some(epoch),
                native_wire::envelope::Body::CopyOperation(body) => body.epoch = Some(epoch),
                native_wire::envelope::Body::ReplicationOperation(body) => {
                    body.metadata.as_mut().unwrap().sequence = 2;
                    body.first_sequence = 2;
                    body.last_sequence = 2;
                    body.last_in_batch = 2;
                    if command == "BodyReplicationSending" {
                        body.sending_epoch = Some(epoch);
                    } else {
                        body.original_epoch = Some(epoch);
                    }
                }
                _ => unreachable!(),
            }
            let id = frame.message_id.clone();
            let before = pair.target_route.inspect_native().unwrap();
            assert_eq!(before["receivers"].as_array().unwrap().len(), 1);
            assert_eq!(
                before["receivers"][0]["replication"].is_null(),
                command == "BodyStart"
            );
            if replication {
                assert_eq!(
                    snapshot(&pair.target).await.publication.unwrap().role,
                    ReplicaRole::ActiveSecondary
                );
                assert_eq!(before["receivers"][0]["replication"]["received"], 1);
                assert_eq!(before["receivers"][0]["replication"]["applied"], 1);
            } else {
                assert_eq!(before["receivers"][0]["copy"]["received"], 0);
                assert_eq!(before["receivers"][0]["copy"]["applied"], 0);
                if command == "BodyCopy" {
                    assert_eq!(before["receivers"][0]["replication"]["received"], 0);
                    assert_eq!(before["receivers"][0]["replication"]["applied"], 0);
                }
            }
            let source_data = pair.source.service.items.lock().unwrap().clone();
            let target_data = pair.target.service.items.lock().unwrap().clone();
            let context = pair.target.service.context_calls.load(Ordering::Acquire);
            let acquisitions = pair.source.service.acquisitions.load(Ordering::Acquire);
            assert_eq!(context, usize::from(command != "BodyStart"));
            assert_eq!(acquisitions, usize::from(command != "BodyStart"));
            let progress = if replication {
                Some(
                    pair.target
                        .service
                        .control()
                        .current_progress()
                        .await
                        .unwrap(),
                )
            } else {
                None
            };
            sender.send(frame).await.unwrap();
            // Correlate the real retained ingress task, not RPC EOF or ACK credit.
            until(|| {
                pair.target_route.inspect_native().unwrap()["ingressObservations"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|entry| entry[0] == id)
            })
            .await;
            let observed = pair.target_route.inspect_native().unwrap();
            assert_eq!(
                observed["receivers"], before["receivers"],
                "{command}: native receive/apply mutation"
            );
            let (no_replication, no_copy) = tokio::join!(
                tokio::time::timeout(Duration::from_millis(50), operations.get_operation()),
                tokio::time::timeout(Duration::from_millis(50), copy.get_operation()),
            );
            assert!(
                no_replication.is_err(),
                "{command}: unexpected replication delivery"
            );
            if replication {
                assert!(
                    matches!(no_copy, Ok(Ok(None))),
                    "{command}: closed copy receiver changed"
                );
            } else {
                assert!(no_copy.is_err(), "{command}: unexpected copy delivery");
            }
            assert_eq!(*pair.source.service.items.lock().unwrap(), source_data);
            assert_eq!(*pair.target.service.items.lock().unwrap(), target_data);
            assert_eq!(
                pair.target.service.context_calls.load(Ordering::Acquire),
                context
            );
            assert_eq!(
                pair.source.service.acquisitions.load(Ordering::Acquire),
                acquisitions
            );
            if let Some(progress) = progress {
                assert_eq!(
                    pair.target
                        .service
                        .control()
                        .current_progress()
                        .await
                        .unwrap(),
                    progress
                );
            }
            assert_eq!(
                observed["nativeCredits"], before["nativeCredits"],
                "{command}: stale body earned credit"
            );
            assert_eq!(
                observed["sessions"], before["sessions"],
                "{command}: changed session ownership"
            );
            let observation = observed["ingressObservations"]
                .as_array()
                .unwrap()
                .iter()
                .find(|entry| entry[0] == id)
                .unwrap();
            assert_eq!(
                observation[1], "epochRejected",
                "{command}: validator did not reject before dispatch"
            );
            let no_credit = async {
                while let Some(reply) = response.message().await.unwrap() {
                    if let Some(native_wire::envelope::Body::ReplicationAck(ack)) = reply.body {
                        let receiver = &before["receivers"][0];
                        assert!(
                            ack.replication_received
                                <= receiver["replication"]["received"].as_i64().unwrap_or(-1)
                        );
                        assert!(
                            ack.replication_applied
                                <= receiver["replication"]["applied"].as_i64().unwrap_or(-1)
                        );
                        assert!(
                            ack.copy_received
                                <= receiver["copy"]["received"].as_i64().unwrap_or(-1)
                        );
                        assert!(
                            ack.copy_applied <= receiver["copy"]["applied"].as_i64().unwrap_or(-1)
                        );
                        assert_eq!(ack.error, 0);
                    }
                }
            };
            let _ = tokio::time::timeout(Duration::from_millis(50), no_credit).await;
            assert!(
                !sender.is_closed(),
                "{command}: input closed before assertions"
            );
            let after = pair.target_route.inspect_native().unwrap();
            assert_eq!(after["receivers"], before["receivers"]);
            assert_eq!(after["nativeCredits"], before["nativeCredits"]);
            println!(
                "{command} epoch=({data_loss},{configuration}) frame={id}: observed epochRejected; real receivers/data/ACK credit unchanged before teardown"
            );
        }
        drop(sender);
        drop(response);
        pair.source_hook.open();
        pair.source.service.copy_release.add_permits(1);
        if !replication {
            let copied = pair.copy().await;
            let data: Vec<_> = copied
                .into_iter()
                .filter(|operation| !operation.end_of_stream)
                .map(|operation| operation.data)
                .collect();
            assert_eq!(data, *pair.source.service.items.lock().unwrap());
            *pair.target.service.items.lock().unwrap() = data;
        }
        assert_eq!(pair.target.service.context_calls.load(Ordering::Acquire), 1);
        assert_eq!(pair.source.service.acquisitions.load(Ordering::Acquire), 1);
        if !replication {
            assert_eq!(
                settled(&pair.source, &build).await,
                ReplicaGoalStatus::Achieved
            );
        }
        pair.finish().await;
    }
}

pub(super) async fn retained_joins() {
    let pair = Pair::new().await;
    pair.target.service.context_release.forget_permits(1);
    let _build = pair.build().await;
    entered(&pair.target.service.context_entered).await;
    let control = pair.target.service.control();
    let mut abort = pair.target_goal.clone();
    abort.operation_id = OperationId::new("abort-retaining-provider");
    abort.revision = 2;
    abort.desired = ReplicaGoalKind::Abort;
    submit(&pair.target, &abort).await.unwrap();
    until(|| pair.target_route.inspect_native().unwrap()["sessions"]["closed"] == true).await;
    assert!(snapshot(&pair.target).await.publication.is_none());
    assert!(snapshot(&pair.target).await.provider_writer_held);
    let mut successor = pair.target_goal.clone();
    successor.operation_id = OperationId::new("forbidden-successor");
    successor.revision = 3;
    successor.scope.creation_id = ReplicatorCreationId::new("late-successor");
    assert!(submit(&pair.target, &successor).await.is_err());
    let observer = tokio::spawn({
        let control = control.clone();
        async move { control.close().await }
    });
    tokio::time::sleep(Duration::from_millis(40)).await;
    assert!(!observer.is_finished());
    observer.abort();
    assert!(observer.await.unwrap_err().is_cancelled());
    let retained = tokio::spawn({
        let control = control.clone();
        async move { control.close().await }
    });
    assert!(snapshot(&pair.target).await.provider_writer_held);
    pair.target.service.context_release.add_permits(1);
    tokio::time::timeout(Duration::from_secs(10), retained)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    control.close().await.unwrap();
    assert_eq!(
        settled(&pair.target, &abort).await,
        ReplicaGoalStatus::Achieved
    );
    assert!(!snapshot(&pair.target).await.provider_writer_held);
    assert_eq!(pair.target.service.close_calls.load(Ordering::SeqCst), 1);
    assert_eq!(pair.target_route.inspect_native().unwrap()["tasks"], 0);
    pair.finish().await;

    let pair = Pair::new().await;
    let target = snapshot(&pair.target).await;
    TestingTcpRoute::lose_native_driver(&target.process_session_id, &target.creation_id)
        .await
        .unwrap();
    let control = pair.target.service.control();
    control.abort();
    let first = control.close().await.unwrap_err().to_string();
    assert_eq!(control.close().await.unwrap_err().to_string(), first);
    assert!(snapshot(&pair.target).await.provider_writer_held);
    assert_eq!(pair.target_route.inspect_native().unwrap()["tasks"], 0);
    pair.finish().await;

    let pair = Pair::new().await;
    let _build = pair.build().await;
    let mut stream = pair.target.service.state().copy_operations().unwrap();
    let operation = next(&mut stream).await;
    *pair.target_hook.fail.lock().unwrap() = Some(Cut::BeforeCredit);
    operation.acknowledge().unwrap();
    until(|| !pair.target_route.inspect_native().unwrap()["failure"].is_null()).await;
    let control = pair.target.service.control();
    control.abort();
    let first = control.close().await.unwrap_err().to_string();
    assert!(first.contains("failed actual transport descendant"));
    assert_eq!(control.close().await.unwrap_err().to_string(), first);
    assert!(snapshot(&pair.target).await.provider_writer_held);
    assert_eq!(pair.target_route.inspect_native().unwrap()["tasks"], 0);
    pair.finish().await;
}

pub(super) async fn lost_retire_response() {
    let pair = Pair::new().await;
    let build = pair.build().await;
    pair.copy().await;
    assert_eq!(
        settled(&pair.source, &build).await,
        ReplicaGoalStatus::Achieved
    );
    pair.target_hook.hold(Cut::RetireResponse);
    let primary = pair.source.service.primary();
    let remove = tokio::spawn(async move { primary.remove_replica(ReplicaId::new(2)).await });
    entered(&pair.target_hook.entered).await;
    // The target native owner is joined, but the real retained control reply is held.
    assert_eq!(
        pair.target_route.inspect_native().unwrap()["sessions"]["entries"][0]["phase"],
        "Retired"
    );
    assert!(
        tokio::time::timeout(Duration::from_secs(7), remove)
            .await
            .unwrap()
            .unwrap()
            .is_err()
    );
    assert_eq!(
        pair.source
            .service
            .control()
            .current_progress()
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        pair.source_route.inspect_native().unwrap()["sessions"]["entries"][0]["phase"],
        "Retiring"
    );
    let mut abort = pair.target_goal.clone();
    abort.operation_id = OperationId::new("abort-held-retire-response");
    abort.revision = 2;
    abort.desired = ReplicaGoalKind::Abort;
    submit(&pair.target, &abort).await.unwrap();
    until(|| pair.target_route.inspect_native().unwrap()["sessions"]["closed"] == true).await;
    assert!(snapshot(&pair.target).await.provider_writer_held);
    assert!(snapshot(&pair.target).await.publication.is_none());
    assert!(
        snapshot(&pair.target)
            .await
            .goals
            .iter()
            .any(|observation| {
                observation.goal == abort && observation.status == ReplicaGoalStatus::Pending
            })
    );
    pair.target_hook.open();
    assert_eq!(
        settled(&pair.target, &abort).await,
        ReplicaGoalStatus::Achieved
    );
    assert!(!snapshot(&pair.target).await.provider_writer_held);
    pair.finish().await;
}

pub(super) async fn burst_ack(retire: bool) {
    let pair = Arc::new(Pair::new().await);
    *pair.source.service.items.lock().unwrap() = (0..48)
        .map(|index| Bytes::from(format!("native-{index}")))
        .collect();
    let build = pair.build().await;
    let mut stream = pair.target.service.state().copy_operations().unwrap();
    let mut delivered = Vec::new();
    for _ in 0..48 {
        delivered.push(next(&mut stream).await);
    }
    assert!(
        tokio::time::timeout(Duration::from_millis(30), stream.get_operation())
            .await
            .is_err()
    );
    assert_eq!(
        pair.target_route.inspect_native().unwrap()["ackCapacity"],
        0
    );
    pair.source_hook.hold(Cut::ConfirmReceived);
    let mut held_confirmations = Vec::new();
    for _ in 0..32 {
        let pair = pair.clone();
        held_confirmations.push(tokio::spawn(async move {
            control(&pair, pair.binding(), "Confirm").await
        }));
    }
    until(|| pair.source_route.inspect_native().unwrap()["controlsAvailable"] == 0).await;
    let tasks_before = pair.target_route.inspect_native().unwrap()["tasks"]
        .as_u64()
        .unwrap();
    for operation in &delivered {
        operation.acknowledge().unwrap();
        assert!(matches!(
            operation.clone().acknowledge(),
            Err(RuntimeError::InvalidReplication(_))
        ));
    }
    entered(&pair.source_hook.entered).await;
    tokio::time::sleep(Duration::from_millis(40)).await;
    let held = pair.target_route.inspect_native().unwrap();
    assert!(held["tasks"].as_u64().unwrap() <= tasks_before + 1);
    assert!(held["failure"].is_null());
    assert_eq!(held["sessions"]["entries"][0]["phase"], "Active");
    assert_eq!(
        pair.source_route.inspect_native().unwrap()["controlsAvailable"],
        0
    );
    if retire {
        pair.source_hook.lost_retire.store(true, Ordering::SeqCst);
        assert!(pair.remove().await.is_err());
    }
    pair.source_hook.open();
    for confirmation in held_confirmations {
        let result = confirmation.await.unwrap();
        if !retire {
            result.unwrap();
        }
    }
    if retire {
        until(|| {
            pair.target_route.inspect_native().unwrap()["sessions"]["entries"][0]["phase"]
                == "Retiring"
        })
        .await;
        assert!(
            snapshot(&pair.source)
                .await
                .remote_operations
                .iter()
                .all(|op| op.state != RemoteReplicaState::Ready)
        );
        pair.source_hook.lost_retire.store(false, Ordering::SeqCst);
    } else {
        let eof = next(&mut stream).await;
        assert!(eof.end_of_stream);
        eof.acknowledge().unwrap();
        assert_eq!(
            settled(&pair.source, &build).await,
            ReplicaGoalStatus::Achieved
        );
        for operation in delivered {
            assert!(operation.acknowledge().is_err());
        }
        assert!(pair.target_route.inspect_native().unwrap()["failure"].is_null());
        pair.target.service.control().abort();
        pair.target.service.control().close().await.unwrap();
        pair.target.service.control().close().await.unwrap();
    }
    Arc::try_unwrap(pair).ok().unwrap().finish().await;
}

pub(super) async fn target_retire_ack(fail: bool) {
    let pair = Pair::new().await;
    pair.source_hook.hold(Cut::BuildResult);
    pair.target_hook.hold(Cut::BeforeCredit);
    let build = pair.build().await;
    let mut stream = pair.target.service.state().copy_operations().unwrap();
    let first = next(&mut stream).await;
    let second = next(&mut stream).await;
    first.acknowledge().unwrap();
    entered(&pair.target_hook.entered).await;
    second.acknowledge().unwrap();
    assert_eq!(pair.target_route.inspect_native().unwrap()["ackPending"], 2);
    let primary = pair.source.service.primary();
    let remove = tokio::spawn(async move { primary.remove_replica(ReplicaId::new(2)).await });
    until(|| {
        pair.target_route.inspect_native().unwrap()["sessions"]["entries"][0]["phase"] == "Retiring"
    })
    .await;
    assert!(!remove.is_finished());
    assert_eq!(pair.target_route.inspect_native().unwrap()["ackPending"], 2);
    assert_eq!(
        pair.target_route.inspect_native().unwrap()["nativeCredits"],
        0
    );
    assert_eq!(
        pair.source_route.inspect_native().unwrap()["sessions"]["entries"][0]["phase"],
        "Retiring"
    );
    assert!(first.acknowledge().is_err());
    assert!(snapshot(&pair.target).await.provider_writer_held);
    assert!(
        tokio::time::timeout(Duration::from_secs(7), remove)
            .await
            .unwrap()
            .unwrap()
            .is_err(),
        "source observer times out, not the retained target join"
    );
    assert_eq!(pair.target_route.inspect_native().unwrap()["ackPending"], 2);
    assert_eq!(
        pair.target_route.inspect_native().unwrap()["sessions"]["entries"][0]["phase"],
        "Retiring"
    );
    assert_eq!(
        pair.source_route.inspect_native().unwrap()["sessions"]["entries"][0]["phase"],
        "Retiring"
    );
    if fail {
        *pair.target_hook.fail.lock().unwrap() = Some(Cut::BeforeCredit);
    }
    pair.target_hook.open();
    until(|| pair.target_route.inspect_native().unwrap()["ackPending"] == 0).await;
    assert_eq!(
        pair.target_route.inspect_native().unwrap()["nativeCredits"],
        0
    );
    if fail {
        until(|| !pair.target_route.inspect_native().unwrap()["failure"].is_null()).await;
        let binding = pair.binding();
        assert!(control(&pair, binding, "Retire").await.is_err());
        assert_eq!(
            pair.target_route.inspect_native().unwrap()["sessions"]["entries"][0]["phase"],
            "Retiring"
        );
        let close = pair.target.service.control();
        let first = close.close().await.unwrap_err().to_string();
        assert!(
            first.contains("failed actual transport descendant"),
            "{first}"
        );
        assert_eq!(close.close().await.unwrap_err().to_string(), first);
        assert!(snapshot(&pair.target).await.provider_writer_held);
    } else {
        until(|| {
            pair.source_route.inspect_native().unwrap()["sessions"]["entries"][0]["phase"]
                == "Retired"
        })
        .await;
        assert_eq!(
            pair.target_route.inspect_native().unwrap()["sessions"]["entries"][0]["phase"],
            "Retired"
        );
        assert!(pair.target_route.inspect_native().unwrap()["failure"].is_null());
        pair.target.service.control().close().await.unwrap();
        pair.target.service.control().close().await.unwrap();
    }
    pair.source_hook.open();
    assert!(matches!(
        settled(&pair.source, &build).await,
        ReplicaGoalStatus::Failed(_) | ReplicaGoalStatus::Uncertain(_)
    ));
    assert!(
        snapshot(&pair.source)
            .await
            .remote_operations
            .iter()
            .all(|operation| operation.state != RemoteReplicaState::Ready)
    );
    pair.finish().await;
}

async fn acknowledge_goal(node: &Node<EngineService>, goal: &ReplicaGoal) {
    let mut client =
        proto::agent_control_client::AgentControlClient::connect(node.control_address.clone())
            .await
            .unwrap();
    client
        .acknowledge_goal(authorized(proto::ReplicaGoalRequest {
            protocol_version: ORDINARY_ACTION_FORMAT,
            canonical_json: serde_json::to_vec(&ReplicaGoalAcknowledgement {
                goal: goal.clone(),
                logically_retired: false,
            })
            .unwrap(),
        }))
        .await
        .unwrap();
}

async fn group_configuration(
    nodes: &[&Node<EngineService>],
    goals: &mut [ReplicaGoal],
    current: &ConfigurationDescriptor,
    previous: Option<&ConfigurationDescriptor>,
) {
    let mut peers = Vec::new();
    for node in nodes {
        let publication = snapshot(node).await.publication.unwrap();
        peers.push(ReplicaPeerDescription {
            identity: node.identity.clone(),
            replication_address: publication.replication_address,
            process_session_id: node.session.clone(),
            storage_incarnation: node.storage.clone(),
            role: if node.identity.replica_id == current.primary_id {
                ReplicaRole::Primary
            } else {
                ReplicaRole::ActiveSecondary
            },
            current_progress: 0,
            catch_up_capability: 0,
            is_up: true,
            is_ready: true,
            must_catchup: false,
        });
    }
    // Ordinary controller order: source joint, target joint, source current
    // with real WriteQuorum catch-up, then target current.
    for (node, goal) in nodes.iter().zip(goals.iter_mut()) {
        if !current
            .members
            .iter()
            .any(|member| member.identity == node.identity)
        {
            continue;
        }
        goal.operation_id = OperationId::new(format!("membership-{}", goal.revision + 1));
        goal.revision += 1;
        goal.scope.configuration = ReplicaConfigurationScope {
            current: current.clone(),
            previous: previous.cloned(),
            replicas: peers
                .iter()
                .filter(|peer| {
                    peer.identity != node.identity
                        && current
                            .members
                            .iter()
                            .any(|member| member.identity == peer.identity)
                })
                .cloned()
                .collect(),
        };
        goal.desired = ReplicaGoalKind::Reconfigure {
            role: if node.identity.replica_id == current.primary_id {
                ReplicaRole::Primary
            } else {
                ReplicaRole::ActiveSecondary
            },
            catch_up: (previous.is_none() && node.identity.replica_id == current.primary_id)
                .then_some(PublicCatchUpMode::WriteQuorum),
        };
        submit(node, goal).await.unwrap_or_else(|error| {
            panic!(
                "membership for replica {} at revision {} rejected: {error}",
                node.identity.replica_id, goal.revision
            )
        });
        assert_eq!(settled(node, goal).await, ReplicaGoalStatus::Achieved);
    }
}

pub(super) async fn multi_secondary() {
    for progressive in [false, true] {
        multi_secondary_case(progressive).await;
    }
}

async fn multi_secondary_case(progressive: bool) {
    let pair = Pair::new_sized(3).await;
    let extra_hook = Hook::new();
    let extra_route = extra_hook.register("pod-3".into());
    let mut extra = Node::<EngineService>::start_resolved(
        &pair._root.path().join("third"),
        3,
        0,
        3,
        pair.resolver.clone(),
    )
    .await;
    pair.resolver
        .0
        .lock()
        .unwrap()
        .insert(extra.identity.replica_id, extra.control_address.clone());
    let mut extra_goal = pair.target_goal.clone();
    extra_goal.identity = extra.preview.clone();
    extra_goal.scope.storage = extra.storage.clone();
    extra_goal.scope.process_session_id = extra.session.clone();
    extra_goal.scope.creation_id = snapshot(&extra).await.creation_id;
    submit(&extra, &extra_goal).await.unwrap();
    assert_eq!(
        settled(&extra, &extra_goal).await,
        ReplicaGoalStatus::Achieved
    );
    let first_build = pair.build().await;
    pair.copy().await;
    assert_eq!(
        settled(&pair.source, &first_build).await,
        ReplicaGoalStatus::Achieved
    );
    acknowledge_goal(&pair.source, &first_build).await;
    let original = pair.source_goal.scope.configuration.current.clone();
    let nodes = [&pair.source, &pair.target, &extra];
    let descriptor = |members: usize| {
        ConfigurationDescriptor::new(
            original.epoch,
            original.primary_id,
            nodes[..members]
                .iter()
                .enumerate()
                .map(|(index, node)| ConfigurationMember {
                    identity: node.identity.clone(),
                    role: if index == 0 {
                        ReplicaRole::Primary
                    } else {
                        ReplicaRole::ActiveSecondary
                    },
                })
                .collect(),
            2,
        )
    };
    let mut goals = [first_build, pair.target_goal.clone(), extra_goal];
    let mut previous = original.clone();
    if progressive {
        let intermediate = descriptor(2);
        group_configuration(&nodes, &mut goals, &intermediate, Some(&previous)).await;
        group_configuration(&nodes, &mut goals, &intermediate, None).await;
        previous = intermediate;
        // Prepare the next idle against the source's achieved scope, as the
        // ordinary controller does before a new Build. No old Ready is reused.
        let mut prepared = goals[2].clone();
        prepared.operation_id = OperationId::new("prepare-third-against-current");
        prepared.revision += 1;
        prepared.scope.configuration = goals[0].scope.configuration.clone();
        prepared.desired = ReplicaGoalKind::Reconfigure {
            role: ReplicaRole::IdleSecondary,
            catch_up: None,
        };
        submit(&extra, &prepared).await.unwrap();
        assert_eq!(
            settled(&extra, &prepared).await,
            ReplicaGoalStatus::Achieved
        );
        goals[2] = prepared;
    }
    pair.source.service.copy_release.add_permits(1);
    let mut second_build = goals[0].clone();
    second_build.operation_id = OperationId::new("build-third");
    second_build.revision += 1;
    second_build.desired = ReplicaGoalKind::Build {
        remote: Box::new(RemoteReplicaSelection {
            original_member: None,
            source: snapshot(&pair.source).await.publication.unwrap(),
            target: snapshot(&extra).await.publication.unwrap(),
            target_control_address: extra.control_address.clone(),
            generation: 1,
        }),
    };
    submit(&pair.source, &second_build).await.unwrap();
    let mut copy = extra.service.state().copy_operations().unwrap();
    loop {
        let operation = next(&mut copy).await;
        operation.acknowledge().unwrap();
        if operation.end_of_stream {
            break;
        }
    }
    assert_eq!(
        settled(&pair.source, &second_build).await,
        ReplicaGoalStatus::Achieved
    );
    acknowledge_goal(&pair.source, &second_build).await;
    assert_eq!(pair.source.service.acquisitions.load(Ordering::SeqCst), 2);
    assert_eq!(
        *pair.source.service.observed_context.lock().unwrap(),
        extra.service.context
    );
    let bindings = pair.source_route.inspect_native().unwrap()["sessions"]["entries"].clone();
    goals[0] = second_build;
    let current = descriptor(3);
    group_configuration(&nodes, &mut goals, &current, Some(&previous)).await;
    extra_hook.open();
    group_configuration(&nodes, &mut goals, &current, None).await;
    assert_eq!(
        snapshot(&pair.source).await.write_status,
        AccessStatus::Granted
    );
    assert_eq!(
        pair.source_route.inspect_native().unwrap()["sessions"]["entries"],
        bindings
    );
    let source_before_remove = pair.source_route.inspect_native().unwrap();
    let target_before_remove = pair.target_route.inspect_native().unwrap();
    let extra_before_remove = extra_route.inspect_native().unwrap();
    assert!(
        pair.source
            .service
            .primary()
            .remove_replica(extra.identity.replica_id)
            .await
            .is_err(),
        "idle-only Remove must reject a configured voter"
    );
    let source_after_remove = pair.source_route.inspect_native().unwrap();
    assert_eq!(
        source_after_remove["sessions"],
        source_before_remove["sessions"]
    );
    assert_eq!(source_after_remove["peers"], source_before_remove["peers"]);
    assert_eq!(
        source_after_remove["production"]["peers"],
        source_before_remove["production"]["peers"]
    );
    assert_eq!(
        pair.target_route.inspect_native().unwrap()["sessions"],
        target_before_remove["sessions"]
    );
    let extra_after_remove = extra_route.inspect_native().unwrap();
    assert_eq!(
        extra_after_remove["sessions"],
        extra_before_remove["sessions"]
    );
    assert_eq!(extra_after_remove["peers"], extra_before_remove["peers"]);
    assert!(extra_after_remove["failure"].is_null());
    let mut left = pair
        .target
        .service
        .state()
        .replication_operations()
        .unwrap();
    let mut right = extra.service.state().replication_operations().unwrap();
    let write = pair
        .source
        .service
        .state()
        .begin_replicate(Bytes::from_static(b"joint-current"))
        .unwrap();
    let completion = tokio::spawn(write.completion);
    let left_operation = next(&mut left).await;
    let right_operation = next(&mut right).await;
    assert_eq!(left_operation.lsn, write.lsn);
    assert_eq!(right_operation.lsn, write.lsn);
    assert_eq!(right_operation.data, Bytes::from_static(b"joint-current"));
    tokio::time::sleep(Duration::from_millis(40)).await;
    assert!(
        !completion.is_finished(),
        "idle receipt cannot replace voting application ACK"
    );
    left_operation.acknowledge().unwrap();
    tokio::time::timeout(Duration::from_secs(10), completion)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let primary = pair.source.service.primary();
    let catchup = tokio::spawn(async move {
        primary
            .wait_for_catch_up_quorum(ReplicaSetQuorumMode::All)
            .await
    });
    tokio::time::sleep(Duration::from_millis(40)).await;
    assert!(
        !catchup.is_finished(),
        "All catch-up must include the other real secondary"
    );
    right_operation.acknowledge().unwrap();
    tokio::time::timeout(Duration::from_secs(10), catchup)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    for route in [&pair.source_route, &pair.target_route, &extra_route] {
        assert!(route.inspect_native().unwrap()["failure"].is_null());
        assert!(
            route.inspect_native().unwrap()["sessions"]["entries"]
                .as_array()
                .unwrap()
                .iter()
                .all(|entry| entry["admitted"] == true)
        );
    }
    // Reject a real native membership callback with crossed peer identity.
    let mut wrong =
        ReplicaInformation::idle_build(extra.identity.clone(), "http://127.0.0.1:1".into());
    wrong.process_session_id = extra.session.clone();
    wrong.storage_incarnation = Some(extra.storage.clone());
    wrong.role = ReplicaRole::ActiveSecondary;
    assert!(
        pair.source
            .service
            .primary()
            .update_current_replica_set_configuration(ReplicaSetConfiguration {
                configuration: current.clone(),
                replicas: vec![wrong]
            })
            .await
            .is_err()
    );
    let mut unbuilt = extra.identity.clone();
    unbuilt.replica_id = ReplicaId::new(99);
    unbuilt.instance_id = ReplicaInstanceId::new("unbuilt");
    let mut incompatible_members = current.members.clone();
    incompatible_members.push(ConfigurationMember {
        identity: unbuilt,
        role: ReplicaRole::ActiveSecondary,
    });
    let installed_peers = goals[0]
        .scope
        .configuration
        .replicas
        .iter()
        .map(|peer| ReplicaInformation {
            identity: peer.identity.clone(),
            replication_address: peer.replication_address.clone(),
            process_session_id: peer.process_session_id.clone(),
            role: peer.role,
            current_progress: peer.current_progress,
            catch_up_capability: peer.catch_up_capability,
            must_catchup: peer.must_catchup,
            storage_incarnation: Some(peer.storage_incarnation.clone()),
        })
        .collect();
    assert!(
        pair.source
            .service
            .primary()
            .update_current_replica_set_configuration(ReplicaSetConfiguration {
                configuration: ConfigurationDescriptor::new(
                    current.epoch,
                    current.primary_id,
                    incompatible_members,
                    current.write_quorum
                ),
                replicas: installed_peers,
            })
            .await
            .is_err(),
        "an unbuilt member cannot become installed native membership"
    );
    let binding =
        extra_route.inspect_native().unwrap()["sessions"]["entries"][0]["binding"].clone();
    control_at(&binding, binding.clone(), "Confirm")
        .await
        .unwrap();
    let mut stale = binding.clone();
    stale["target"]["scope"]["processSessionId"] = "stale-process".into();
    assert!(control_at(&binding, stale, "Confirm").await.is_err());
    let mut incompatible = binding.clone();
    incompatible["source"]["scope"]["configuration"]["current"]["writeQuorum"] = 3.into();
    assert!(control_at(&binding, incompatible, "Confirm").await.is_err());
    assert_eq!(
        extra_route.inspect_native().unwrap()["sessions"]["entries"][0]["binding"],
        binding
    );
    extra.service.control().abort();
    extra.service.control().close().await.unwrap();
    assert_eq!(extra_route.inspect_native().unwrap()["tasks"], 0);
    extra.running.shutdown();
    extra.running.wait().await.unwrap();
    pair.finish().await;
}

pub(super) async fn idle_remove_lost_observer() {
    let pair = Pair::new().await;
    pair.source_hook.hold(Cut::BuildResult);
    pair.target_hook.hold(Cut::BeforeCredit);
    let build = pair.build().await;
    let mut stream = pair.target.service.state().copy_operations().unwrap();
    let first = next(&mut stream).await;
    let second = next(&mut stream).await;
    first.acknowledge().unwrap();
    entered(&pair.target_hook.entered).await;
    second.acknowledge().unwrap();
    let primary = pair.source.service.primary();
    let remove = tokio::spawn(async move { primary.remove_replica(ReplicaId::new(2)).await });
    until(|| {
        pair.target_route.inspect_native().unwrap()["sessions"]["entries"][0]["phase"] == "Retiring"
    })
    .await;
    assert!(!remove.is_finished());
    remove.abort();
    assert!(remove.await.unwrap_err().is_cancelled());
    assert_eq!(
        pair.source_route.inspect_native().unwrap()["sessions"]["entries"][0]["phase"],
        "Retiring"
    );
    assert_eq!(pair.target_route.inspect_native().unwrap()["ackPending"], 2);
    assert_eq!(
        pair.target_route.inspect_native().unwrap()["nativeCredits"],
        0
    );
    assert!(first.acknowledge().is_err());
    pair.target_hook.open();
    until(|| {
        pair.source_route.inspect_native().unwrap()["sessions"]["entries"][0]["phase"] == "Retired"
    })
    .await;
    assert_eq!(
        pair.target_route.inspect_native().unwrap()["sessions"]["entries"][0]["phase"],
        "Retired"
    );
    assert_eq!(pair.target_route.inspect_native().unwrap()["ackPending"], 0);
    assert_eq!(
        pair.target_route.inspect_native().unwrap()["nativeCredits"],
        0
    );
    pair.source_hook.open();
    assert!(matches!(
        settled(&pair.source, &build).await,
        ReplicaGoalStatus::Failed(_) | ReplicaGoalStatus::Uncertain(_)
    ));
    pair.finish().await;
}

pub(super) async fn remove_adoption_orderings() {
    for adoption_first in [false, true] {
        let pair = Pair::new().await;
        let build = pair.build().await;
        pair.copy().await;
        assert_eq!(
            settled(&pair.source, &build).await,
            ReplicaGoalStatus::Achieved
        );
        let binding = pair.binding();
        let target = snapshot(&pair.target).await.publication.unwrap();
        let configuration = ReplicaSetConfiguration {
            configuration: ConfigurationDescriptor::new(
                pair.source_goal.scope.epoch,
                pair.source.identity.replica_id,
                vec![
                    ConfigurationMember {
                        identity: pair.source.identity.clone(),
                        role: ReplicaRole::Primary,
                    },
                    ConfigurationMember {
                        identity: pair.target.identity.clone(),
                        role: ReplicaRole::ActiveSecondary,
                    },
                ],
                2,
            ),
            replicas: vec![ReplicaInformation {
                identity: pair.target.identity.clone(),
                process_session_id: pair.target.session.clone(),
                replication_address: target.replication_address,
                storage_incarnation: Some(pair.target.storage.clone()),
                role: ReplicaRole::ActiveSecondary,
                current_progress: 0,
                catch_up_capability: 0,
                must_catchup: false,
            }],
        };
        pair.source_hook.hold(if adoption_first {
            Cut::ConfigurationAdmitted
        } else {
            Cut::ConfigurationValidated
        });
        let primary = pair.source.service.primary();
        let previous = ReplicaSetConfiguration {
            configuration: pair.source_goal.scope.configuration.current.clone(),
            replicas: Vec::new(),
        };
        let adoption = tokio::spawn(async move {
            primary
                .update_catch_up_replica_set_configuration(configuration, previous)
                .await
        });
        entered(&pair.source_hook.entered).await;
        let source = pair.source_route.inspect_native().unwrap();
        let target = pair.target_route.inspect_native().unwrap();
        if adoption_first {
            assert!(pair.remove().await.is_err());
            let after = pair.source_route.inspect_native().unwrap();
            assert_eq!(after["sessions"], source["sessions"]);
            assert_eq!(after["peers"], source["peers"]);
            assert_eq!(after["production"]["peers"], source["production"]["peers"]);
            assert_eq!(
                pair.target_route.inspect_native().unwrap()["sessions"],
                target["sessions"]
            );
            pair.source_hook.open();
            adoption.await.unwrap().unwrap();
        } else {
            pair.source_hook.hold(Cut::RemoveAdmitted);
            let primary = pair.source.service.primary();
            let remove =
                tokio::spawn(async move { primary.remove_replica(ReplicaId::new(2)).await });
            entered(&pair.source_hook.entered).await;
            // Native removal fenced this peer, but metadata and the remote
            // receiver are deliberately still present. Release the older
            // configuration waiter first; actor-side resolution must reject it.
            assert_eq!(
                pair.source_route.inspect_native().unwrap()["production"]["peers"],
                source["production"]["peers"]
            );
            assert_eq!(
                pair.target_route.inspect_native().unwrap()["sessions"],
                target["sessions"]
            );
            pair.source_hook.release.add_permits(1);
            assert!(adoption.await.unwrap().is_err());
            assert!(!remove.is_finished());
            pair.source_hook.open();
            remove.await.unwrap().unwrap();
            assert_eq!(
                pair.target_route.inspect_native().unwrap()["sessions"]["entries"][0]["phase"],
                "Retired"
            );
            assert!(
                control_at(&binding, binding.clone(), "Confirm")
                    .await
                    .is_err()
            );
        }
        if adoption_first {
            pair.admit(&build).await;
            let mut operations = pair
                .target
                .service
                .state()
                .replication_operations()
                .unwrap();
            let write = pair
                .source
                .service
                .state()
                .begin_replicate(Bytes::from_static(b"after-rejected-remove"))
                .unwrap();
            let completion = tokio::spawn(write.completion);
            let operation = next(&mut operations).await;
            assert_eq!(operation.data, Bytes::from_static(b"after-rejected-remove"));
            assert_eq!(operation.lsn, write.lsn);
            assert!(!completion.is_finished());
            operation.acknowledge().unwrap();
            tokio::time::timeout(Duration::from_secs(10), completion)
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            pair.source
                .service
                .primary()
                .wait_for_catch_up_quorum(ReplicaSetQuorumMode::All)
                .await
                .unwrap();
        }
        pair.finish().await;
    }
}
