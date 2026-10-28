use super::*;
use crate::cluster_api::{AgentRpcError, ClusterApi};
use crate::observation::RawObservation;
use kuberic_runtime::application::{OpenContext, OperationDataStream, RoleChange};
use kuberic_runtime::control::proto;
use kuberic_runtime::protocol::public_operations::*;
use kuberic_runtime::protocol::types::*;
use kuberic_runtime::replicator::*;
use kuberic_runtime::{RuntimeError, StateProvider, StatefulServiceReplica};
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Semaphore;

struct Engine {
    context: StdMutex<Option<ReplicatorFactoryContext>>,
    default_factory: AtomicBool,
    root: PathBuf,
    role_hold: AtomicBool,
    role_release: Semaphore,
    role_calls: AtomicUsize,
    role_failure: AtomicBool,
    build_hold: AtomicBool,
    late_build_success: AtomicBool,
    build_release: Semaphore,
    builds: AtomicUsize,
    removes: AtomicUsize,
    opens: AtomicUsize,
    progress: AtomicUsize,
    aborts: AtomicUsize,
    close_hold: AtomicBool,
    close_failure: AtomicBool,
    close_release: Semaphore,
    closes: AtomicUsize,
    service_close_hold: AtomicBool,
    service_close_release: Semaphore,
    service_closes: AtomicUsize,
    aborted: AtomicBool,
    engine_stop: watch::Sender<bool>,
    engine_root: Mutex<Option<JoinHandle<kuberic_runtime::Result<()>>>>,
    service_stop: watch::Sender<bool>,
    service_root: Mutex<Option<JoinHandle<()>>>,
    engine_joined: AtomicBool,
    service_joined: AtomicBool,
    trace: StdMutex<Vec<&'static str>>,
}

#[tokio::test]
async fn ordinary_finalization_retains_routes_until_actual_close_and_descendant_joins() {
    let applications = Applications::new(false);
    let mut harness = Harness::new(
        applications.clone(),
        NAMESPACE.into(),
        NAME.into(),
        "ordinary-cleanup-ownership".into(),
        Duration::from_secs(30),
        Duration::from_secs(10),
        1,
    )
    .await
    .unwrap();
    harness.ready_members(1).await.unwrap();
    let snapshot = harness.ordinary_snapshots().await.unwrap().remove(0);
    let uid = snapshot.storage.replica.instance_id.to_string();
    let publication = snapshot.publication.unwrap();
    let network = harness.network.clone();
    let routed = network
        .route("cleanup-observer", &uid, &publication.replication_address)
        .await
        .unwrap();
    assert_eq!(request(&routed, 0).await, 1);
    let engine = applications.engine(1);
    engine.close_hold.store(true, Ordering::Release);
    engine.service_close_hold.store(true, Ordering::Release);
    let finish = tokio::spawn(harness.finish(Ok(())));
    tokio::time::timeout(Duration::from_secs(5), async {
        while engine.closes.load(Ordering::Acquire) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(!finish.is_finished());
    assert_eq!(
        network.diagnostics()["closed"],
        false,
        "normal finalization cannot destroy routes before ordinary Close joins"
    );
    assert_eq!(
        request(&routed, 0).await,
        0,
        "the fenced engine still owns its real socket"
    );
    assert!(
        TestingTcpRoute::register(uid.clone(), |_, endpoint| {
            Box::pin(async move { Ok(endpoint) })
        })
        .is_err()
    );
    engine.close_release.add_permits(1);
    tokio::time::timeout(Duration::from_secs(5), async {
        while engine.service_closes.load(Ordering::Acquire) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(!finish.is_finished());
    assert_eq!(network.diagnostics()["closed"], false);
    assert!(
        network
            .physical_route("cleanup-observer", &publication.replication_address)
            .await
            .is_some()
    );
    assert!(engine.engine_joined.load(Ordering::Acquire));
    assert!(!engine.service_joined.load(Ordering::Acquire));
    engine.service_close_release.add_permits(1);
    finish.await.unwrap().unwrap();
    assert!(engine.service_joined.load(Ordering::Acquire));
    network.assert_drained().await.unwrap();
}

#[tokio::test]
async fn ordinary_finalization_timeout_keeps_routes_and_late_close_failure_owned() {
    let applications = Applications::new(false);
    let mut harness = Harness::new(
        applications.clone(),
        NAMESPACE.into(),
        NAME.into(),
        "ordinary-cleanup-timeout".into(),
        Duration::from_secs(30),
        Duration::from_millis(200),
        1,
    )
    .await
    .unwrap();
    harness.ready_members(1).await.unwrap();
    let snapshot = harness.ordinary_snapshots().await.unwrap().remove(0);
    let uid = snapshot.storage.replica.instance_id.to_string();
    let publication = snapshot.publication.unwrap();
    let network = harness.network.clone();
    let routed = network
        .route("cleanup-observer", &uid, &publication.replication_address)
        .await
        .unwrap();
    let engine = applications.engine(1);
    engine.close_hold.store(true, Ordering::Release);
    engine.close_failure.store(true, Ordering::Release);
    let error = harness
        .finish(Err(anyhow!("original scenario failure")))
        .await
        .unwrap_err();
    assert!(format!("{error:?}").contains("original scenario failure"));
    assert_eq!(network.diagnostics()["closed"], false);
    assert_eq!(request(&routed, 0).await, 0);
    assert!(
        TestingTcpRoute::register(uid, |_, endpoint| { Box::pin(async move { Ok(endpoint) }) })
            .is_err()
    );
    engine.close_release.add_permits(1);
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if network.diagnostics()["closed"] == true {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(engine.engine_joined.load(Ordering::Acquire));
    assert!(engine.service_joined.load(Ordering::Acquire));
    let failure = network.assert_drained().await.unwrap_err();
    assert!(format!("{failure:#}").contains("ordinary fixture Close failed"));
}

#[tokio::test]
async fn production_ordinary_builtin_copies_actual_application_bytes_before_admission() {
    let mut harness = Harness::new(
        super::super::tests::EmptyFactory,
        NAMESPACE.into(),
        NAME.into(),
        "ordinary-builtin".into(),
        Duration::from_secs(30),
        Duration::from_secs(10),
        3,
    )
    .await
    .unwrap();
    let result = async {
        assert_eq!(harness.ready_members(3).await?, 1);
        let snapshots = harness.ordinary_snapshots().await?;
        assert_eq!(snapshots.len(), 3);
        for snapshot in snapshots {
            let publication = snapshot.publication.unwrap();
            assert_eq!(publication.stage, ReplicaPublicationStage::AchievedScope);
            assert!(publication.scope.configuration.previous.is_none());
            assert_eq!(publication.scope.configuration.current.members.len(), 3);
            let id = snapshot.storage.replica.replica_id.value();
            if id != 1 {
                let root = harness
                    .data_root(snapshot.storage.replica.instance_id.as_str())
                    .await?;
                assert_eq!(
                    std::fs::read(root.join("application/copied-state"))?,
                    b"empty-provider-v1"
                );
                assert_ne!(snapshot.read_status, AccessStatus::Granted);
                assert_ne!(snapshot.write_status, AccessStatus::Granted);
            } else {
                assert_eq!(snapshot.write_status, AccessStatus::Granted);
                assert_eq!(snapshot.remote_operations.len(), 2);
                assert!(
                    snapshot
                        .remote_operations
                        .iter()
                        .all(|remote| remote.accepted)
                );
            }
        }
        Ok::<_, anyhow::Error>(())
    }
    .await;
    harness.finish(result).await.unwrap();
}

impl Engine {
    fn new(root: PathBuf, held: bool) -> Arc<Self> {
        Arc::new(Self {
            context: StdMutex::new(None),
            default_factory: AtomicBool::new(false),
            root,
            role_hold: AtomicBool::new(held),
            role_release: Semaphore::new(0),
            role_calls: AtomicUsize::new(0),
            role_failure: AtomicBool::new(false),
            build_hold: AtomicBool::new(true),
            build_release: Semaphore::new(0),
            builds: AtomicUsize::new(0),
            late_build_success: AtomicBool::new(false),
            removes: AtomicUsize::new(0),
            opens: AtomicUsize::new(0),
            progress: AtomicUsize::new(0),
            aborts: AtomicUsize::new(0),
            close_hold: AtomicBool::new(false),
            close_failure: AtomicBool::new(false),
            close_release: Semaphore::new(0),
            closes: AtomicUsize::new(0),
            service_close_hold: AtomicBool::new(false),
            service_close_release: Semaphore::new(0),
            service_closes: AtomicUsize::new(0),
            aborted: AtomicBool::new(false),
            engine_stop: watch::channel(false).0,
            engine_root: Mutex::new(None),
            service_stop: watch::channel(false).0,
            service_root: Mutex::new(None),
            engine_joined: AtomicBool::new(false),
            service_joined: AtomicBool::new(false),
            trace: StdMutex::new(Vec::new()),
        })
    }

    fn context(&self) -> ReplicatorFactoryContext {
        self.context.lock().unwrap().clone().unwrap()
    }
    fn release(&self) {
        self.role_hold.store(false, Ordering::Release);
        self.role_release.add_permits(16);
        self.build_hold.store(false, Ordering::Release);
        self.build_release.add_permits(16);
        self.close_hold.store(false, Ordering::Release);
        self.close_release.add_permits(16);
        self.service_close_hold.store(false, Ordering::Release);
        self.service_close_release.add_permits(16);
    }
}

struct Provider;
struct Writer;
#[async_trait::async_trait]
impl StateReplicator for Writer {
    fn begin_replicate(
        &self,
        _: kuberic_runtime::application::OperationData,
    ) -> kuberic_runtime::Result<Replication> {
        Err(RuntimeError::AuthorityNotAdmitted)
    }
    fn replication_operations(
        &self,
    ) -> kuberic_runtime::Result<kuberic_runtime::replicator::stream::OperationStream> {
        Err(RuntimeError::AuthorityNotAdmitted)
    }
    fn copy_operations(
        &self,
    ) -> kuberic_runtime::Result<kuberic_runtime::replicator::stream::OperationStream> {
        Err(RuntimeError::AuthorityNotAdmitted)
    }
    async fn update_replicator_settings(
        &self,
        _: ReplicatorSettings,
    ) -> kuberic_runtime::Result<()> {
        Ok(())
    }
}
#[async_trait::async_trait]
impl StateProvider for Provider {
    async fn update_epoch(&self, _: Epoch, _: i64) -> kuberic_runtime::Result<()> {
        Ok(())
    }
    async fn last_committed_lsn(&self) -> kuberic_runtime::Result<i64> {
        Ok(0)
    }
    async fn get_copy_context(&self) -> kuberic_runtime::Result<OperationDataStream> {
        Ok(Box::pin(futures::stream::empty()))
    }
    async fn get_copy_state(
        &self,
        _: i64,
        _: OperationDataStream,
    ) -> kuberic_runtime::Result<OperationDataStream> {
        Ok(Box::pin(futures::stream::empty()))
    }
    async fn on_data_loss(&self) -> kuberic_runtime::Result<bool> {
        Ok(false)
    }
}

struct OrdinaryFactory(Arc<Engine>);
#[async_trait::async_trait]
impl ReplicatorFactory for OrdinaryFactory {
    async fn create_replicator(
        &self,
        context: ReplicatorFactoryContext,
        _: Option<Arc<dyn StateProvider>>,
        _: ReplicatorSettings,
    ) -> kuberic_runtime::Result<ReplicatorInterfaces> {
        assert!(context.ordinary_hosting());
        *self.0.context.lock().unwrap() = Some(context);
        Ok(ReplicatorInterfaces::primary(
            self.0.clone(),
            Some(Arc::new(Writer)),
            ReplicatorCapabilities::conservative(),
        ))
    }
}

struct Application(Arc<Engine>);
#[async_trait::async_trait]
impl StatefulServiceReplica for Application {
    async fn open(
        self: Arc<Self>,
        context: OpenContext,
    ) -> kuberic_runtime::Result<Arc<dyn Replicator>> {
        if !self.0.root.join("durable-state").exists() {
            std::fs::write(
                self.0.root.join("durable-state"),
                b"acknowledged-application-prefix",
            )
            .unwrap();
        }
        let mut stop = self.0.service_stop.subscribe();
        *self.0.service_root.lock().await = Some(tokio::spawn(async move {
            let _ = stop.wait_for(|stopped| *stopped).await;
        }));
        let partition = if self.0.default_factory.load(Ordering::Acquire) {
            context.partition.with_builtin_replicator()
        } else {
            context
                .partition
                .with_factory(Arc::new(OrdinaryFactory(self.0.clone())))
        };
        let interfaces = partition
            .create_replicator(
                Some(Arc::new(Provider)),
                Some(ReplicatorSettings {
                    replication_address: "http://127.0.0.1:0".into(),
                }),
            )
            .await?;
        Ok(interfaces.replicator())
    }
    async fn change_role(&self, _: ReplicaRole) -> kuberic_runtime::Result<RoleChange> {
        Ok(RoleChange {
            service_address: None,
        })
    }
    async fn close(&self) -> kuberic_runtime::Result<()> {
        self.0.service_closes.fetch_add(1, Ordering::AcqRel);
        self.0.trace.lock().unwrap().push("application-close");
        if self.0.service_close_hold.load(Ordering::Acquire) {
            self.0
                .service_close_release
                .acquire()
                .await
                .unwrap()
                .forget();
        }
        self.0.service_stop.send_replace(true);
        if let Some(root) = self.0.service_root.lock().await.take() {
            root.await.unwrap();
        }
        self.0.service_joined.store(true, Ordering::Release);
        self.0.trace.lock().unwrap().push("application-joined");
        Ok(())
    }
    fn abort(&self) {
        self.0.trace.lock().unwrap().push("application-abort");
    }
}

#[async_trait::async_trait]
impl Replicator for Engine {
    async fn open(&self) -> kuberic_runtime::Result<String> {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(|error| RuntimeError::Application(error.to_string()))?;
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let context = self.context();
        let root = self.root.clone();
        let mut stop = self.engine_stop.subscribe();
        *self.engine_root.lock().await = Some(tokio::spawn(async move {
            loop {
                let (mut stream, _) = tokio::select! {
                    _ = stop.wait_for(|stopped| *stopped) => break,
                    result = listener.accept() => result.map_err(|error| RuntimeError::Application(error.to_string()))?,
                };
                let command = stream
                    .read_u8()
                    .await
                    .map_err(|error| RuntimeError::Application(error.to_string()))?;
                let publication = context.local_publication().await.unwrap_or(None);
                let allowed =
                    match command {
                        0 => publication.is_some(),
                        1 => publication.as_ref().is_some_and(|publication| {
                            publication.stage == ReplicaPublicationStage::AchievedScope
                                && publication.role == ReplicaRole::IdleSecondary
                        }),
                        2 => {
                            context
                                .read_status()
                                .await
                                .unwrap_or(AccessStatus::NotPrimary)
                                == AccessStatus::Granted
                                && context
                                    .write_status()
                                    .await
                                    .unwrap_or(AccessStatus::NotPrimary)
                                    == AccessStatus::Granted
                        }
                        3 => publication.as_ref().is_some_and(|publication| {
                            publication.stage == ReplicaPublicationStage::AchievedScope
                                && matches!(
                                    publication.role,
                                    ReplicaRole::Primary | ReplicaRole::ActiveSecondary
                                )
                                && publication.scope.configuration.current.members.iter().any(
                                    |member| member.identity == publication.scope.storage.replica,
                                )
                        }),
                        _ => false,
                    };
                if command == 1 && allowed {
                    std::fs::write(root.join("copied-state"), b"ordinary-engine-state").unwrap();
                }
                stream
                    .write_u8(u8::from(allowed))
                    .await
                    .map_err(|error| RuntimeError::Application(error.to_string()))?;
            }
            Ok(())
        }));
        self.opens.fetch_add(1, Ordering::AcqRel);
        self.trace.lock().unwrap().push("replicator-open");
        Ok(endpoint)
    }
    async fn change_role(&self, _: Epoch, _: ReplicaRole) -> kuberic_runtime::Result<()> {
        self.role_calls.fetch_add(1, Ordering::AcqRel);
        self.trace.lock().unwrap().push("role-enter");
        if self.role_hold.load(Ordering::Acquire) {
            self.role_release.acquire().await.unwrap().forget();
        }
        if self.role_failure.load(Ordering::Acquire) {
            return Err(RuntimeError::Application(
                "ordinary preparation failure".into(),
            ));
        }
        self.trace.lock().unwrap().push("role-return");
        Ok(())
    }
    async fn update_epoch(&self, _: Epoch) -> kuberic_runtime::Result<()> {
        Ok(())
    }
    async fn close(&self) -> kuberic_runtime::Result<()> {
        self.closes.fetch_add(1, Ordering::AcqRel);
        self.trace.lock().unwrap().push("replicator-close");
        if self.close_hold.load(Ordering::Acquire) {
            self.close_release.acquire().await.unwrap().forget();
        }
        self.engine_stop.send_replace(true);
        if let Some(root) = self.engine_root.lock().await.take() {
            root.await.unwrap()?;
        }
        self.engine_joined.store(true, Ordering::Release);
        self.trace.lock().unwrap().push("replicator-joined");
        if self.close_failure.load(Ordering::Acquire) {
            return Err(RuntimeError::Application(
                "ordinary fixture Close failed".into(),
            ));
        }
        Ok(())
    }
    fn abort(&self) {
        self.aborted.store(true, Ordering::Release);
        self.aborts.fetch_add(1, Ordering::AcqRel);
        self.trace.lock().unwrap().push("replicator-abort");
    }
    async fn current_progress(&self) -> kuberic_runtime::Result<i64> {
        self.progress.fetch_add(1, Ordering::AcqRel);
        std::future::pending().await
    }
    async fn catch_up_capability(&self) -> kuberic_runtime::Result<i64> {
        self.progress.fetch_add(1, Ordering::AcqRel);
        std::future::pending().await
    }
}

#[async_trait::async_trait]
impl PrimaryReplicator for Engine {
    async fn on_data_loss(&self) -> kuberic_runtime::Result<bool> {
        Ok(false)
    }
    async fn update_catch_up_replica_set_configuration(
        &self,
        _: ReplicaSetConfiguration,
        _: ReplicaSetConfiguration,
    ) -> kuberic_runtime::Result<()> {
        self.trace.lock().unwrap().push("joint-configuration");
        Ok(())
    }
    async fn update_current_replica_set_configuration(
        &self,
        _: ReplicaSetConfiguration,
    ) -> kuberic_runtime::Result<()> {
        self.trace.lock().unwrap().push("current-configuration");
        Ok(())
    }
    async fn wait_for_catch_up_quorum(
        &self,
        _: ReplicaSetQuorumMode,
    ) -> kuberic_runtime::Result<()> {
        self.trace.lock().unwrap().push("catch-up");
        Ok(())
    }
    async fn build_replica(&self, replica: ReplicaInformation) -> kuberic_runtime::Result<()> {
        assert_eq!(replica.current_progress, INVALID_LSN);
        self.builds.fetch_add(1, Ordering::AcqRel);
        self.trace.lock().unwrap().push("build-enter");
        if self.build_hold.load(Ordering::Acquire) {
            self.build_release.acquire().await.unwrap().forget();
        }
        if !self.aborted.load(Ordering::Acquire)
            && !self.late_build_success.load(Ordering::Acquire)
            && request(&replica.replication_address, 1).await != 1
        {
            return Err(RuntimeError::Application(
                "ordinary target denied copy".into(),
            ));
        }
        self.trace.lock().unwrap().push("build-return");
        Ok(())
    }
    async fn remove_replica(&self, _: ReplicaId) -> kuberic_runtime::Result<()> {
        self.removes.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }
}

#[derive(Clone)]
struct Applications {
    engines: Arc<StdMutex<BTreeMap<i64, Arc<Engine>>>>,
    held: bool,
}
impl Applications {
    fn new(held: bool) -> Self {
        Self {
            engines: Arc::default(),
            held,
        }
    }
    fn engine(&self, id: i64) -> Arc<Engine> {
        self.engines.lock().unwrap()[&id].clone()
    }
    fn release(&self) {
        for engine in self.engines.lock().unwrap().values() {
            engine.release();
        }
    }
}
#[async_trait::async_trait]
impl ApplicationFactory for Applications {
    type Service = Application;
    async fn create(
        &self,
        context: ReplicaContext,
    ) -> super::super::Result<super::super::CreatedApplication<Application>> {
        let engine = Engine::new(context.application_root().to_owned(), self.held);
        self.engines
            .lock()
            .unwrap()
            .insert(context.identity().replica_id.value(), engine.clone());
        let storage = if context.application_root().join("durable-state").exists() {
            kuberic_runtime::host::ApplicationStorageState::Established
        } else {
            kuberic_runtime::host::ApplicationStorageState::FreshEmpty
        };
        Ok(
            super::super::CreatedApplication::new(Arc::new(Application(engine)), storage)
                .with_storage_paths([context.application_root().to_owned()]),
        )
    }
    async fn shutdown_and_drain(&self, service: Arc<Application>) -> super::super::Result<()> {
        // Teardown must not supply the lifecycle proof tested below.
        if service.0.opens.load(Ordering::Acquire) > 0
            && !(service.0.engine_joined.load(Ordering::Acquire)
                && service.0.service_joined.load(Ordering::Acquire))
        {
            return Err(anyhow!(
                "ordinary lifecycle failed to join actual engine/application descendants"
            )
            .into());
        }
        Ok(())
    }
}

struct ObservedTransport {
    grpc: GrpcAgentApi,
    held_snapshot: StdMutex<Option<ReplicaAgentSnapshot>>,
    lost_discover_reply: AtomicBool,
    discover_submissions: StdMutex<Vec<ReplicaGoal>>,
    lost_build_reply: AtomicBool,
    lose_remove_ack_reply: AtomicBool,
    lost_remove_ack: StdMutex<Option<(ReplicaGoalAcknowledgement, ReplicaAgentSnapshot)>>,
    remove_acknowledgements: AtomicUsize,
    build_submissions: AtomicUsize,
    build_goals: StdMutex<Vec<ReplicaGoal>>,
    build_acknowledgements: StdMutex<Vec<ReplicaGoal>>,
    forbidden: AtomicUsize,
    snapshots: AtomicUsize,
    incompatible_peer: AtomicBool,
}

impl ObservedTransport {
    fn new(lost_reply: bool) -> Arc<Self> {
        Arc::new(Self {
            grpc: GrpcAgentApi::new(Duration::from_secs(3)),
            held_snapshot: StdMutex::new(None),
            lost_discover_reply: AtomicBool::new(false),
            discover_submissions: StdMutex::new(Vec::new()),
            lost_build_reply: AtomicBool::new(lost_reply),
            lose_remove_ack_reply: AtomicBool::new(false),
            lost_remove_ack: StdMutex::new(None),
            remove_acknowledgements: AtomicUsize::new(0),
            build_submissions: AtomicUsize::new(0),
            build_goals: StdMutex::new(Vec::new()),
            build_acknowledgements: StdMutex::new(Vec::new()),
            forbidden: AtomicUsize::new(0),
            snapshots: AtomicUsize::new(0),
            incompatible_peer: AtomicBool::new(false),
        })
    }
}

#[async_trait::async_trait]
impl AgentApi for ObservedTransport {
    async fn get_snapshot(
        &self,
        endpoint: &str,
        token: &str,
        request: proto::ReplicaSnapshotRequest,
    ) -> std::result::Result<proto::ReplicaSnapshotResponse, AgentRpcError> {
        self.snapshots.fetch_add(1, Ordering::AcqRel);
        let incompatible = self.incompatible_peer.load(Ordering::Acquire)
            && request
                .target
                .as_ref()
                .is_some_and(|target| target.replica_id == 2);
        let mut response = self.grpc.get_snapshot(endpoint, token, request).await?;
        if let Some(snapshot) = self.held_snapshot.lock().unwrap().as_ref()
            && serde_json::from_slice::<ReplicaAgentQueryResult>(&response.canonical_json)
                .is_ok_and(|result| {
                    matches!(result, ReplicaAgentQueryResult::Initialized(current)
                    if current.storage == snapshot.storage)
                })
        {
            response.canonical_json = serde_json::to_vec(&ReplicaAgentQueryResult::Initialized(
                Box::new(snapshot.clone()),
            ))
            .unwrap();
        }
        if incompatible {
            response.protocol_version = 14;
        }
        Ok(response)
    }
    async fn submit_goal(
        &self,
        endpoint: &str,
        token: &str,
        request: proto::ReplicaGoalRequest,
    ) -> std::result::Result<proto::ReplicaSnapshotResponse, AgentRpcError> {
        let goal: ReplicaGoal = serde_json::from_slice(&request.canonical_json).unwrap();
        let discover = matches!(goal.desired, ReplicaGoalKind::Discover);
        if discover {
            self.discover_submissions.lock().unwrap().push(goal.clone());
        }
        let build = matches!(goal.desired, ReplicaGoalKind::Build { .. });
        if build {
            self.build_submissions.fetch_add(1, Ordering::AcqRel);
            self.build_goals.lock().unwrap().push(goal.clone());
        }
        let result = self.grpc.submit_goal(endpoint, token, request).await;
        if discover && self.lost_discover_reply.swap(false, Ordering::AcqRel) {
            result?;
            return Err(AgentRpcError::Unavailable(
                "injected lost accepted Discover reply".into(),
            ));
        }
        if build && self.lost_build_reply.swap(false, Ordering::AcqRel) {
            result?;
            return Err(AgentRpcError::Unavailable(
                "injected lost accepted Build reply".into(),
            ));
        }
        result
    }
    async fn acknowledge_goal(
        &self,
        endpoint: &str,
        token: &str,
        request: proto::ReplicaGoalRequest,
    ) -> std::result::Result<proto::ReplicaSnapshotResponse, AgentRpcError> {
        let acknowledgement: ReplicaGoalAcknowledgement =
            serde_json::from_slice(&request.canonical_json).unwrap();
        if matches!(acknowledgement.goal.desired, ReplicaGoalKind::Build { .. }) {
            self.build_acknowledgements
                .lock()
                .unwrap()
                .push(acknowledgement.goal.clone());
        }
        let remove = matches!(acknowledgement.goal.desired, ReplicaGoalKind::Remove { .. });
        if remove {
            self.remove_acknowledgements.fetch_add(1, Ordering::AcqRel);
        }
        let response = self.grpc.acknowledge_goal(endpoint, token, request).await?;
        if remove && self.lose_remove_ack_reply.swap(false, Ordering::AcqRel) {
            *self.lost_remove_ack.lock().unwrap() = Some((
                acknowledgement,
                serde_json::from_slice(&response.canonical_json).unwrap(),
            ));
            return Err(AgentRpcError::Unavailable(
                "injected lost successful Remove acknowledgement reply".into(),
            ));
        }
        Ok(response)
    }
    async fn get_status(
        &self,
        _: &str,
        _: &str,
        _: proto::GetAgentStatusRequest,
    ) -> std::result::Result<proto::AgentStatusReport, AgentRpcError> {
        self.forbidden.fetch_add(1, Ordering::AcqRel);
        panic!("production reconciliation called legacy full GetStatus")
    }
    async fn execute(
        &self,
        endpoint: &str,
        token: &str,
        request: proto::ExecuteCommandRequest,
    ) -> std::result::Result<proto::ExecuteCommandResponse, AgentRpcError> {
        let envelope =
            kuberic_runtime::control::normalize_execute_request(request.clone()).unwrap();
        assert!(
            matches!(
                envelope.command,
                kuberic_runtime::protocol::command::ProtocolCommand::InitializeAgentStore(_)
            ),
            "production dispatched a legacy engine program/grant"
        );
        self.grpc.execute(endpoint, token, request).await
    }
}

async fn request(endpoint: &str, command: u8) -> u8 {
    tokio::time::timeout(Duration::from_secs(5), async {
        let mut stream = TcpStream::connect(endpoint.strip_prefix("http://").unwrap())
            .await
            .unwrap();
        stream.write_u8(command).await.unwrap();
        stream.read_u8().await.unwrap()
    })
    .await
    .unwrap()
}

fn production(api: &TestApi, transport: Arc<ObservedTransport>) -> Arc<Reconciler> {
    Arc::new(Reconciler::new(
        Arc::new(
            KubeClusterApi::new(api.client(), transport, "local-test-token", NAMESPACE).unwrap(),
        ),
        crate::production_evaluation_config(1, 1, 1),
    ))
}

async fn start(
    held: bool,
    replicas: u32,
    lost_reply: bool,
) -> (Harness<Applications>, Applications, Arc<ObservedTransport>) {
    let applications = Applications::new(held);
    let mut harness = Harness::new(
        applications.clone(),
        NAMESPACE.into(),
        NAME.into(),
        "ordinary-independent".into(),
        Duration::from_secs(35),
        Duration::from_secs(10),
        replicas,
    )
    .await
    .unwrap();
    let transport = ObservedTransport::new(lost_reply);
    harness.reconciler = production(&harness.api, transport.clone());
    (harness, applications, transport)
}

async fn observe<F: ApplicationFactory>(
    harness: &Harness<F>,
    transport: Arc<ObservedTransport>,
) -> RawObservation {
    KubeClusterApi::new(
        harness.api.client(),
        transport,
        "local-test-token",
        NAMESPACE,
    )
    .unwrap()
    .observe(NAMESPACE, NAME)
    .await
    .unwrap()
}
fn peers(raw: &RawObservation) -> Vec<ReplicaAgentSnapshot> {
    raw.agents
        .values()
        .filter_map(|observation| match observation {
            crate::observation::RawAgentObservation::ReplicaSnapshot(snapshot) => {
                Some(*snapshot.clone())
            }
            _ => None,
        })
        .collect()
}
async fn until(harness: &mut Harness<Applications>, mut predicate: impl FnMut() -> bool) {
    let result = tokio::time::timeout(Duration::from_secs(15), async {
        while !predicate() {
            harness.step().await.unwrap();
        }
    })
    .await;
    let goals = harness
        .set()
        .await
        .unwrap()
        .status
        .unwrap_or_default()
        .replica_goals
        .into_iter()
        .map(|entry| {
            (
                entry.goal.scope.storage.replica.replica_id,
                entry.goal.revision,
                format!("{:?}", entry.goal.desired)
                    .chars()
                    .take(40)
                    .collect::<String>(),
                entry.outcome.map(|outcome| outcome.status),
            )
        })
        .collect::<Vec<_>>();
    assert!(
        result.is_ok(),
        "production did not progress: {:?}; goals: {:?}",
        harness.last_error,
        goals
    );
}
async fn pump<F: ApplicationFactory>(harness: &mut Harness<F>, passes: usize) {
    for _ in 0..passes {
        harness.step().await.unwrap();
    }
}
async fn close(harness: Harness<Applications>, applications: &Applications) {
    applications.release();
    harness.finish(Ok(())).await.unwrap();
}

#[tokio::test]
async fn production_ordinary_mutual_discovery_opens_every_peer_with_recovery_and_progress_held() {
    let (mut harness, applications, transport) = start(true, 3, false).await;
    until(&mut harness, || {
        let engines = applications.engines.lock().unwrap();
        engines.len() == 3
            && engines
                .values()
                .all(|engine| engine.role_calls.load(Ordering::Acquire) > 0)
    })
    .await;
    pump(&mut harness, 3).await;
    let raw = observe(&harness, transport.clone()).await;
    let snapshots = peers(&raw);
    assert_eq!(snapshots.len(), 3);
    for snapshot in &snapshots {
        let publication = snapshot.publication.as_ref().unwrap();
        assert_eq!(publication.stage, ReplicaPublicationStage::DiscoveryPending);
        assert_eq!(publication.role, ReplicaRole::None);
        assert!(matches!(
            snapshot.progress,
            ProgressQueryResult::NotQueried(_)
        ));
        assert!(matches!(
            snapshot.capability,
            ProgressQueryResult::NotQueried(_)
        ));
        let engine = applications.engine(snapshot.storage.replica.replica_id.value());
        assert_eq!(engine.opens.load(Ordering::Acquire), 1);
        assert_eq!(engine.progress.load(Ordering::Acquire), 0);
        assert_eq!(engine.builds.load(Ordering::Acquire), 0);
        for peer in &snapshots {
            let resolved = engine
                .context()
                .discover_peer(&ReplicaDiscoveryQuery {
                    format: ORDINARY_ACTION_FORMAT,
                    history: snapshot.history.clone(),
                    replica: peer.storage.replica.clone(),
                    process_session_id: Some(peer.process_session_id.clone()),
                })
                .await
                .unwrap()
                .unwrap();
            assert_eq!(resolved, *peer.publication.as_ref().unwrap());
            assert_eq!(request(&resolved.replication_address, 0).await, 1);
            for prohibited in [1, 2, 3] {
                assert_eq!(request(&resolved.replication_address, prohibited).await, 0);
            }
        }
    }
    assert_eq!(transport.forbidden.load(Ordering::Acquire), 0);
    close(harness, &applications).await;
}

#[tokio::test]
async fn production_ordinary_held_build_lost_reply_recreation_exact_ready_then_separate_admission()
{
    let (mut harness, applications, transport) = start(false, 2, true).await;
    until(&mut harness, || {
        applications
            .engines
            .lock()
            .unwrap()
            .get(&1)
            .is_some_and(|engine| engine.builds.load(Ordering::Acquire) == 1)
    })
    .await;
    let retained = harness
        .set()
        .await
        .unwrap()
        .status
        .unwrap()
        .replica_goals
        .into_iter()
        .find(|entry| matches!(entry.goal.desired, ReplicaGoalKind::Build { .. }))
        .unwrap();
    harness.recreate_controller().unwrap();
    harness.reconciler = production(&harness.api, transport.clone());
    pump(&mut harness, 3).await;
    let raw = observe(&harness, transport.clone()).await;
    let snapshots = peers(&raw);
    let source = snapshots
        .iter()
        .find(|peer| peer.storage.replica.replica_id.value() == 1)
        .unwrap();
    assert!(
        source
            .remote_operations
            .iter()
            .any(|remote| remote.operation_id == retained.goal.operation_id
                && remote.state == RemoteReplicaState::InBuild)
    );
    assert!(
        raw.set
            .status
            .as_ref()
            .unwrap()
            .replica_goals
            .contains(&retained)
    );
    assert_eq!(applications.engine(1).builds.load(Ordering::Acquire), 1);
    assert!(transport.build_submissions.load(Ordering::Acquire) > 1);
    let target = snapshots
        .iter()
        .find(|peer| peer.storage.replica.replica_id.value() == 2)
        .unwrap();
    assert_eq!(
        request(&target.publication.as_ref().unwrap().replication_address, 2).await,
        0
    );
    assert!(!applications.engine(2).root.join("copied-state").exists());
    applications.engine(1).build_release.add_permits(1);
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let raw = observe(&harness, transport.clone()).await;
            if peers(&raw).iter().any(|peer| {
                peer.remote_operations
                    .iter()
                    .any(|remote| remote.state == RemoteReplicaState::Ready && !remote.accepted)
            }) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        std::fs::read(applications.engine(2).root.join("copied-state")).unwrap(),
        b"ordinary-engine-state"
    );
    assert_eq!(
        harness
            .set()
            .await
            .unwrap()
            .status
            .unwrap()
            .public_v1_topology
            .unwrap()
            .member_count,
        1
    );
    harness.step().await.unwrap();
    let raw = observe(&harness, transport.clone()).await;
    assert!(peers(&raw).iter().any(|peer| {
        peer.remote_operations
            .iter()
            .any(|remote| remote.state == RemoteReplicaState::Ready && remote.accepted)
    }));
    assert_eq!(
        raw.set
            .status
            .as_ref()
            .unwrap()
            .public_v1_topology
            .as_ref()
            .unwrap()
            .member_count,
        1
    );
    assert_eq!(
        request(&target.publication.as_ref().unwrap().replication_address, 3).await,
        0
    );
    let settled = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            harness.step().await.unwrap();
            let raw = observe(&harness, transport.clone()).await;
            if raw
                .set
                .status
                .as_ref()
                .and_then(|status| status.public_v1_topology.as_ref())
                .is_some_and(|topology| topology.member_count == 2)
            {
                assert!(
                    peers(&raw)
                        .iter()
                        .any(|peer| peer.write_status == AccessStatus::Granted)
                );
                break;
            }
        }
    })
    .await;
    let trace = applications
        .engine(1)
        .trace
        .lock()
        .unwrap()
        .iter()
        .take(24)
        .copied()
        .collect::<Vec<_>>();
    assert!(
        settled.is_ok(),
        "admission did not progress: {:?}; trace: {:?}",
        harness.last_error,
        trace
    );
    assert!(
        applications
            .engine(1)
            .trace
            .lock()
            .unwrap()
            .contains(&"joint-configuration")
    );
    assert!(
        applications
            .engine(1)
            .trace
            .lock()
            .unwrap()
            .contains(&"catch-up")
    );
    assert_eq!(applications.engine(1).builds.load(Ordering::Acquire), 1);
    let persisted = harness.set().await.unwrap().status.unwrap();
    let catch_up_goals = persisted
        .replica_goals
        .iter()
        .filter(|retained| {
            matches!(
                retained.goal.desired,
                ReplicaGoalKind::Reconfigure {
                    catch_up: Some(_),
                    ..
                }
            )
        })
        .collect::<Vec<_>>();
    assert!(!catch_up_goals.is_empty());
    assert!(catch_up_goals.iter().any(|retained| {
        retained.outcome.as_ref().is_some_and(|outcome| {
            outcome.status == ReplicaGoalStatus::Achieved && outcome.goal == retained.goal
        })
    }));
    for retained in catch_up_goals {
        retained.validate().unwrap();
        let value = serde_json::to_value(retained).unwrap();
        assert!(value["goal"]["desired"]["catchUp"].is_string());
        assert!(value["goal"]["desired"].get("catch_up").is_none());
    }
    close(harness, &applications).await;
}

#[tokio::test]
async fn production_ordinary_builtin_factory_opens_real_native_endpoints_without_grants() {
    let (mut harness, applications, transport) = start(false, 2, false).await;
    until(&mut harness, || {
        applications.engines.lock().unwrap().len() == 2
    })
    .await;
    for engine in applications.engines.lock().unwrap().values() {
        engine.default_factory.store(true, Ordering::Release);
    }
    let opened = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            harness.step().await.unwrap();
            let observed = observe(&harness, transport.clone()).await;
            let snapshots = peers(&observed);
            if snapshots.len() == 2
                && snapshots.iter().all(|snapshot| {
                    snapshot.publication.as_ref().is_some_and(|publication| {
                        publication.stage == ReplicaPublicationStage::AchievedScope
                    })
                })
            {
                break;
            }
        }
    })
    .await;
    assert!(
        opened.is_ok(),
        "support did not settle: {:?}",
        harness.last_error
    );
    for snapshot in peers(&observe(&harness, transport.clone()).await) {
        let publication = snapshot.publication.unwrap();
        tokio::net::TcpStream::connect(
            publication
                .replication_address
                .trim_start_matches("http://")
                .trim_end_matches('/'),
        )
        .await
        .unwrap();
        assert!(snapshot.provider_writer_held);
        assert_ne!(snapshot.read_status, AccessStatus::Granted);
        assert_ne!(snapshot.write_status, AccessStatus::Granted);
    }
    for engine in applications.engines.lock().unwrap().values() {
        assert_eq!(engine.opens.load(Ordering::Acquire), 0);
        assert_eq!(engine.builds.load(Ordering::Acquire), 0);
        assert_eq!(engine.progress.load(Ordering::Acquire), 0);
        assert!(engine.context.lock().unwrap().is_none());
    }
    close(harness, &applications).await;
}

async fn wait(mut predicate: impl AsyncFnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while !predicate().await {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn production_ordinary_failed_child_close_keeps_failed_preparation_and_writer_ownership_sticky()
 {
    let (mut harness, applications, transport) = start(true, 2, false).await;
    until(&mut harness, || {
        let engines = applications.engines.lock().unwrap();
        engines.len() == 2
            && engines
                .values()
                .all(|engine| engine.role_calls.load(Ordering::Acquire) > 0)
    })
    .await;
    let target = applications.engine(2);
    target.role_failure.store(true, Ordering::Release);
    target.close_failure.store(true, Ordering::Release);
    target.role_release.add_permits(1);
    wait(async || {
        target.engine_joined.load(Ordering::Acquire)
            && target.service_joined.load(Ordering::Acquire)
    })
    .await;
    pump(&mut harness, 3).await;
    harness.recreate_controller().unwrap();
    harness.reconciler = production(&harness.api, transport.clone());
    pump(&mut harness, 2).await;
    let snapshot = peers(&observe(&harness, transport.clone()).await)
        .into_iter()
        .find(|peer| peer.storage.replica.replica_id.value() == 2)
        .unwrap();
    assert!(snapshot.withdrawn && snapshot.publication.is_none() && snapshot.provider_writer_held);
    assert!(snapshot.goals.iter().any(|observation|
        matches!(&observation.status, ReplicaGoalStatus::Failed(message) if message.contains("preparation failure"))));
    assert!(
        harness
            .set()
            .await
            .unwrap()
            .status
            .unwrap()
            .replica_goals
            .iter()
            .any(|entry| entry.goal.scope.storage == snapshot.storage
                && entry
                    .outcome
                    .as_ref()
                    .is_some_and(|outcome| matches!(outcome.status, ReplicaGoalStatus::Failed(_))))
    );
    assert_eq!(target.closes.load(Ordering::Acquire), 1);
    assert_eq!(target.service_closes.load(Ordering::Acquire), 1);
    assert_eq!(applications.engine(1).builds.load(Ordering::Acquire), 0);
    applications.release();
    let error = harness.finish(Ok(())).await.unwrap_err();
    assert!(format!("{error:#}").contains("ordinary fixture Close failed"));
    assert_eq!(
        target.closes.load(Ordering::Acquire),
        1,
        "sticky Close failure was destructively retried"
    );
}

#[tokio::test]
async fn production_ordinary_mixed_peer_format_rejects_before_fresh_store_resource_or_callback_mutation()
 {
    let (mut harness, applications, transport) = start(true, 2, false).await;
    until(&mut harness, || {
        applications.engines.lock().unwrap().len() == 2
    })
    .await;
    let before = observe(&harness, transport.clone()).await;
    assert!(before.agents.values().all(|agent| matches!(
        agent,
        crate::observation::RawAgentObservation::Uninitialized(_)
    )));
    let resources = snapshot(&harness.api);
    transport.incompatible_peer.store(true, Ordering::Release);
    let error = harness
        .reconciler
        .reconcile(NAMESPACE, NAME)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("incompatible snapshot format"));
    assert_eq!(snapshot(&harness.api), resources);
    transport.incompatible_peer.store(false, Ordering::Release);
    assert!(
        observe(&harness, transport.clone())
            .await
            .agents
            .values()
            .all(|agent| matches!(
                agent,
                crate::observation::RawAgentObservation::Uninitialized(_)
            ))
    );
    for pod in &before.pods {
        assert!(
            !harness
                .data_root(&pod.uid().unwrap())
                .await
                .unwrap()
                .join(".kuberic/agent.sqlite3")
                .exists()
        );
    }
    for engine in applications.engines.lock().unwrap().values() {
        assert_eq!(engine.opens.load(Ordering::Acquire), 0);
        assert_eq!(engine.role_calls.load(Ordering::Acquire), 0);
        assert!(!engine.root.join("durable-state").exists());
    }
    close(harness, &applications).await;
}

async fn scale<F: ApplicationFactory>(harness: &Harness<F>, replicas: u32) {
    let api = Api::<KubericSet>::namespaced(harness.api.client(), NAMESPACE);
    let mut set = api.get(NAME).await.unwrap();
    set.spec.replicas = replicas;
    api.replace(NAME, &PostParams::default(), &set)
        .await
        .unwrap();
}

async fn restart_target(harness: &mut Harness<Applications>, applications: &Applications) {
    restart_replica(harness, applications, 2).await;
}

async fn restart_replica(
    harness: &mut Harness<Applications>,
    applications: &Applications,
    id: i64,
) {
    let old = applications.engine(id);
    let uid = old.context().identity().unwrap().instance_id.to_string();
    harness.stop(&uid).await.unwrap();
    harness.supervisor.lock().await.stopped.remove(&uid);
    wait(async || {
        let new = applications.engine(id);
        !Arc::ptr_eq(&old, &new)
            && harness
                .supervisor
                .lock()
                .await
                .hosts
                .get(&uid)
                .is_some_and(|host| host.running.is_some())
    })
    .await;
    assert!(
        old.engine_joined.load(Ordering::Acquire) && old.service_joined.load(Ordering::Acquire)
    );
}

#[tokio::test]
async fn production_ordinary_closed_predecessor_cannot_block_exact_singleton_reopen_activation() {
    closed_predecessor_singleton_reopen(false).await;
}

#[tokio::test]
async fn production_ordinary_closed_singleton_reopen_after_observation_cutoff_preserves_activation()
{
    closed_predecessor_singleton_reopen(true).await;
}

async fn closed_predecessor_singleton_reopen(after_cutoff: bool) {
    let (mut harness, applications, transport) = start(false, 1, false).await;
    assert_eq!(harness.ready_members(1).await.unwrap(), 1);
    let old = applications.engine(1);
    let before = peers(&observe(&harness, transport.clone()).await).remove(0);
    let bytes = std::fs::read(old.root.join("durable-state")).unwrap();
    let terminal = ReplicaGoal {
        format: ORDINARY_ACTION_FORMAT,
        identity: before.identity.clone(),
        operation_id: OperationId::new("exact-predecessor-close"),
        revision: before
            .goals
            .iter()
            .map(|entry| entry.goal.revision)
            .max()
            .unwrap()
            + 1,
        scope: before.publication.as_ref().unwrap().scope.clone(),
        desired: ReplicaGoalKind::Close,
    };
    let api = Api::<KubericSet>::namespaced(harness.api.client(), NAMESPACE);
    let mut set = api.get(NAME).await.unwrap();
    set.status
        .as_mut()
        .unwrap()
        .replica_goals
        .push(crate::crd::RetainedReplicaGoal::new(terminal.clone()).unwrap());
    api.replace_status(NAME, &PostParams::default(), &set)
        .await
        .unwrap();
    until(&mut harness, || {
        old.engine_joined.load(Ordering::Acquire) && old.service_joined.load(Ordering::Acquire)
    })
    .await;
    pump(&mut harness, 3).await;
    assert_eq!(old.closes.load(Ordering::Acquire), 1);
    if after_cutoff {
        loop {
            let raw = observe(&harness, transport.clone()).await;
            let unavailable = raw
                .set
                .status
                .as_ref()
                .and_then(|status| status.public_v1_topology.as_ref())
                .and_then(|topology| topology.unavailable_since_unix_seconds)
                .is_some_and(|since| {
                    raw.now_unix_seconds.saturating_sub(since)
                        >= i64::try_from(raw.set.spec.failover_delay_seconds).unwrap()
                });
            if unavailable {
                eprintln!(
                    "closed singleton cutoff evidence: status={:?}; observed agents={:?}",
                    raw.set.status, raw.agents
                );
                break;
            }
            harness.step().await.unwrap();
        }
    }
    restart_replica(&mut harness, &applications, 1).await;
    assert_eq!(harness.ready_members(1).await.unwrap(), 1);
    let after = peers(&observe(&harness, transport.clone()).await).remove(0);
    let status = harness.set().await.unwrap().status.unwrap();
    assert_eq!(after.storage, before.storage);
    assert_ne!(after.process_session_id, before.process_session_id);
    assert_ne!(after.creation_id, before.creation_id);
    assert_eq!(after.write_status, AccessStatus::Granted);
    let session = &status.public_v1_topology.unwrap().sessions[0];
    assert_eq!(session.process_session_id, after.process_session_id);
    assert_eq!(session.replicator_creation_id, after.creation_id);
    assert!(status.replica_goals.iter().any(|entry| {
        entry.goal == terminal
            && entry
                .outcome
                .as_ref()
                .is_some_and(|outcome| outcome.status == ReplicaGoalStatus::Achieved)
    }));
    assert!(status.replica_goals.iter().any(|entry| {
        entry.goal.scope.process_session_id == after.process_session_id
            && entry.goal.scope.creation_id == after.creation_id
            && matches!(
                entry.goal.desired,
                ReplicaGoalKind::Reconfigure {
                    role: ReplicaRole::Primary,
                    catch_up: Some(PublicCatchUpMode::WriteQuorum)
                }
            )
            && entry
                .outcome
                .as_ref()
                .is_some_and(|outcome| outcome.status == ReplicaGoalStatus::Achieved)
    }));
    assert_eq!(applications.engine(1).opens.load(Ordering::Acquire), 1);
    assert_eq!(
        std::fs::read(old.root.join("durable-state")).unwrap(),
        bytes
    );
    close(harness, &applications).await;
}

async fn builtin_singleton() -> (
    Harness<super::super::tests::EmptyFactory>,
    Arc<ObservedTransport>,
) {
    let mut harness = Harness::new(
        super::super::tests::EmptyFactory,
        NAMESPACE.into(),
        NAME.into(),
        "ordinary-builtin-growth".into(),
        Duration::from_secs(35),
        Duration::from_secs(10),
        1,
    )
    .await
    .unwrap();
    let transport = ObservedTransport::new(false);
    harness.reconciler = production(&harness.api, transport.clone());
    assert_eq!(harness.ready_members(1).await.unwrap(), 1);
    (harness, transport)
}

async fn retain_terminal<F: ApplicationFactory>(
    harness: &Harness<F>,
    peer: &ReplicaAgentSnapshot,
    desired: ReplicaGoalKind,
) -> ReplicaGoal {
    let api = Api::<KubericSet>::namespaced(harness.api.client(), NAMESPACE);
    let mut set = api.get(NAME).await.unwrap();
    let goal = ReplicaGoal {
        format: ORDINARY_ACTION_FORMAT,
        identity: peer.identity.clone(),
        operation_id: OperationId::new(format!("growth-terminal-{}", peer.process_session_id)),
        revision: peer
            .goals
            .iter()
            .map(|entry| entry.goal.revision)
            .chain(
                set.status
                    .as_ref()
                    .unwrap()
                    .replica_goals
                    .iter()
                    .filter(|entry| entry.goal.scope.storage == peer.storage)
                    .map(|entry| entry.goal.revision),
            )
            .max()
            .unwrap()
            + 1,
        scope: peer.publication.as_ref().unwrap().scope.clone(),
        desired,
    };
    set.status
        .as_mut()
        .unwrap()
        .replica_goals
        .push(crate::crd::RetainedReplicaGoal::new(goal.clone()).unwrap());
    api.replace_status(NAME, &PostParams::default(), &set)
        .await
        .unwrap();
    goal
}

async fn retain_fresh_build<F: ApplicationFactory>(harness: &mut Harness<F>) -> ReplicaGoal {
    scale(harness, 2).await;
    loop {
        if let Some(entry) = harness
            .set()
            .await
            .unwrap()
            .status
            .unwrap()
            .replica_goals
            .into_iter()
            .find(|entry| matches!(entry.goal.desired, ReplicaGoalKind::Build { .. }))
        {
            assert!(entry.outcome.is_none());
            return entry.goal;
        }
        harness.step().await.unwrap();
    }
}

#[tokio::test]
async fn production_ordinary_predecessor_close_allows_one_exact_build_and_actual_native_copy() {
    let (mut harness, transport) = builtin_singleton().await;
    let before = peers(&observe(&harness, transport.clone()).await).remove(0);
    let terminal = retain_terminal(&harness, &before, ReplicaGoalKind::Close).await;
    loop {
        harness.step().await.unwrap();
        if harness
            .set()
            .await
            .unwrap()
            .status
            .unwrap()
            .replica_goals
            .iter()
            .any(|entry| {
                entry.goal == terminal
                    && entry
                        .outcome
                        .as_ref()
                        .is_some_and(|outcome| outcome.status == ReplicaGoalStatus::Achieved)
            })
        {
            break;
        }
    }
    let closed = peers(&observe(&harness, transport.clone()).await).remove(0);
    assert!(closed.withdrawn);
    assert!(!closed.provider_writer_held);
    let uid = before.storage.replica.instance_id.to_string();
    harness.stop(&uid).await.unwrap();
    harness.supervisor.lock().await.stopped.remove(&uid);
    wait(async || {
        harness
            .supervisor
            .lock()
            .await
            .hosts
            .get(&uid)
            .is_some_and(|host| host.running.is_some())
    })
    .await;
    assert_eq!(harness.ready_members(1).await.unwrap(), 1);
    let reopened = peers(&observe(&harness, transport.clone()).await).remove(0);
    assert_eq!(reopened.storage, before.storage);
    assert_eq!(reopened.history, before.history);
    assert_ne!(reopened.process_session_id, before.process_session_id);
    assert_ne!(reopened.creation_id, before.creation_id);
    assert_eq!(reopened.write_status, AccessStatus::Granted);
    assert!(
        reopened
            .goals
            .iter()
            .any(|entry| { entry.goal == terminal && entry.status == ReplicaGoalStatus::Achieved })
    );

    let build = retain_fresh_build(&mut harness).await;
    assert!(transport.build_goals.lock().unwrap().is_empty());
    let ReplicaGoalKind::Build { remote } = &build.desired else {
        unreachable!()
    };
    assert_eq!(build.scope.process_session_id, reopened.process_session_id);
    assert_eq!(build.scope.creation_id, reopened.creation_id);
    let target = peers(&observe(&harness, transport.clone()).await)
        .into_iter()
        .find(|peer| peer.storage == remote.target.scope.storage)
        .unwrap();
    assert_eq!(target.publication.as_ref().unwrap(), &remote.target);
    assert_eq!(remote.target.role, ReplicaRole::IdleSecondary);
    let copied = harness
        .data_root(target.storage.replica.instance_id.as_str())
        .await
        .unwrap()
        .join("application/copied-state");
    assert!(!copied.exists());
    harness.step().await.unwrap();
    wait(async || {
        peers(&observe(&harness, transport.clone()).await)
            .iter()
            .any(|peer| {
                peer.remote_operations.iter().any(|operation| {
                    operation.operation_id == build.operation_id
                        && operation.selection == **remote
                        && operation.state == RemoteReplicaState::Ready
                        && !operation.accepted
                })
            })
    })
    .await;
    assert_eq!(std::fs::read(&copied).unwrap(), b"empty-provider-v1");
    assert_eq!(*transport.build_goals.lock().unwrap(), vec![build.clone()]);
    assert!(transport.build_acknowledgements.lock().unwrap().is_empty());
    let raw = observe(&harness, transport.clone()).await;
    assert_eq!(
        raw.set
            .status
            .as_ref()
            .unwrap()
            .public_v1_topology
            .as_ref()
            .unwrap()
            .member_count,
        1
    );
    let target = peers(&raw)
        .into_iter()
        .find(|peer| peer.storage == remote.target.scope.storage)
        .unwrap();
    assert_ne!(target.read_status, AccessStatus::Granted);
    assert_ne!(target.write_status, AccessStatus::Granted);
    harness.step().await.unwrap();
    let accepted = observe(&harness, transport.clone()).await;
    assert!(
        peers(&accepted)
            .iter()
            .any(|peer| peer.remote_operations.iter().any(|operation| {
                operation.operation_id == build.operation_id
                    && operation.selection == **remote
                    && operation.state == RemoteReplicaState::Ready
                    && operation.accepted
            }))
    );
    assert_eq!(
        accepted
            .set
            .status
            .as_ref()
            .unwrap()
            .public_v1_topology
            .as_ref()
            .unwrap()
            .member_count,
        1
    );
    assert_eq!(harness.ready_members(2).await.unwrap(), 1);
    pump(&mut harness, 3).await;
    assert_eq!(*transport.build_goals.lock().unwrap(), vec![build.clone()]);
    assert_eq!(
        *transport.build_acknowledgements.lock().unwrap(),
        vec![build.clone()]
    );
    let status = harness.set().await.unwrap().status.unwrap();
    for goal in [&terminal, &build] {
        let entries = status
            .replica_goals
            .iter()
            .filter(|entry| entry.goal == *goal)
            .collect::<Vec<_>>();
        assert_eq!(entries.len(), 1);
        assert_eq!(
            entries[0].outcome.as_ref().unwrap().status,
            ReplicaGoalStatus::Achieved
        );
    }
    assert_eq!(std::fs::read(copied).unwrap(), b"empty-provider-v1");
    assert_eq!(transport.forbidden.load(Ordering::Acquire), 0);
    harness.finish(Ok(())).await.unwrap();
}

#[tokio::test]
async fn production_ordinary_current_terminal_vetoes_pending_build_before_actual_copy() {
    for id in [1, 2] {
        for desired in [ReplicaGoalKind::Close, ReplicaGoalKind::Abort] {
            let (mut harness, transport) = builtin_singleton().await;
            let build = retain_fresh_build(&mut harness).await;
            let raw = observe(&harness, transport.clone()).await;
            let target = peers(&raw)
                .into_iter()
                .find(|peer| peer.storage.replica.replica_id.value() == 2)
                .unwrap();
            let copied = harness
                .data_root(target.storage.replica.instance_id.as_str())
                .await
                .unwrap()
                .join("application/copied-state");
            let peer = peers(&raw)
                .into_iter()
                .find(|peer| peer.storage.replica.replica_id.value() == id)
                .unwrap();
            let terminal = retain_terminal(&harness, &peer, desired).await;
            let endpoint = crate::cluster_api::agent_endpoint(
                raw.pods
                    .iter()
                    .find(|pod| {
                        pod.uid().as_deref() == Some(peer.storage.replica.instance_id.as_str())
                    })
                    .unwrap(),
            )
            .unwrap();
            transport
                .grpc
                .submit_goal(
                    &endpoint,
                    "local-test-token",
                    proto::ReplicaGoalRequest {
                        protocol_version: ORDINARY_ACTION_FORMAT,
                        canonical_json: serde_json::to_vec(&terminal).unwrap(),
                    },
                )
                .await
                .unwrap();
            wait(async || {
                peers(&observe(&harness, transport.clone()).await)
                    .iter()
                    .any(|snapshot| {
                        snapshot.storage == peer.storage
                            && snapshot.withdrawn
                            && snapshot.goals.iter().any(|entry| {
                                entry.goal == terminal
                                    && entry.status == ReplicaGoalStatus::Achieved
                            })
                    })
            })
            .await;
            let closed = peers(&observe(&harness, transport.clone()).await)
                .into_iter()
                .find(|snapshot| snapshot.storage == peer.storage)
                .unwrap();
            for status in [
                ReplicaGoalStatus::Pending,
                ReplicaGoalStatus::Failed("retained current terminal failure".into()),
                ReplicaGoalStatus::Uncertain("retained current terminal uncertainty".into()),
                ReplicaGoalStatus::Achieved,
            ] {
                let mut held = closed.clone();
                held.goals
                    .iter_mut()
                    .find(|entry| entry.goal == terminal)
                    .unwrap()
                    .status = status;
                *transport.held_snapshot.lock().unwrap() = Some(held);
                pump(&mut harness, 2).await;
                assert!(transport.build_goals.lock().unwrap().is_empty());
                assert!(transport.build_acknowledgements.lock().unwrap().is_empty());
                assert!(!copied.exists());
            }
            *transport.held_snapshot.lock().unwrap() = None;
            let blocked = observe(&harness, transport.clone()).await;
            assert!(
                blocked
                    .set
                    .status
                    .as_ref()
                    .unwrap()
                    .replica_goals
                    .iter()
                    .any(|entry| entry.goal == build && entry.outcome.is_none())
            );
            assert!(
                peers(&blocked)
                    .iter()
                    .all(|peer| peer.remote_operations.is_empty())
            );
            assert_eq!(
                blocked
                    .set
                    .status
                    .as_ref()
                    .unwrap()
                    .public_v1_topology
                    .as_ref()
                    .unwrap()
                    .member_count,
                1
            );
            assert!(
                peers(&blocked)
                    .iter()
                    .filter(|peer| peer.storage.replica.replica_id.value() == 2)
                    .all(|peer| peer.read_status != AccessStatus::Granted
                        && peer.write_status != AccessStatus::Granted)
            );
            harness.finish(Ok(())).await.unwrap();
        }
    }
}

#[tokio::test]
async fn production_ordinary_reopen_retries_one_discover_with_lost_reply_and_held_publication() {
    let (mut harness, applications, transport) = start(false, 2, false).await;
    until(&mut harness, || {
        applications
            .engines
            .lock()
            .unwrap()
            .get(&1)
            .is_some_and(|engine| engine.builds.load(Ordering::Acquire) > 0)
    })
    .await;
    applications.release();
    assert_eq!(harness.ready_members(2).await.unwrap(), 1);
    let before = peers(&observe(&harness, transport.clone()).await)
        .into_iter()
        .find(|peer| peer.storage.replica.replica_id.value() == 2)
        .unwrap();
    let prefix = std::fs::read(applications.engine(2).root.join("durable-state")).unwrap();
    restart_target(&mut harness, &applications).await;
    let reopened = peers(&observe(&harness, transport.clone()).await)
        .into_iter()
        .find(|peer| peer.storage.replica.replica_id.value() == 2)
        .unwrap();
    assert_eq!(reopened.storage, before.storage);
    assert_eq!(reopened.open_mode, OpenMode::Existing);
    assert_ne!(reopened.process_session_id, before.process_session_id);
    assert_ne!(reopened.creation_id, before.creation_id);
    assert!(reopened.publication.is_none());
    *transport.held_snapshot.lock().unwrap() = Some(reopened.clone());
    transport.lost_discover_reply.store(true, Ordering::Release);
    pump(&mut harness, 8).await;
    let retained = harness.set().await.unwrap().status.unwrap().replica_goals;
    let owners = retained
        .iter()
        .filter(|entry| entry.goal.scope.process_session_id == reopened.process_session_id)
        .collect::<Vec<_>>();
    assert_eq!(
        owners.len(),
        1,
        "stale discovery observation appended another owner"
    );
    let selected = &owners[0].goal;
    assert!(matches!(selected.desired, ReplicaGoalKind::Discover));
    assert!(owners[0].outcome.is_none());
    let retries = transport
        .discover_submissions
        .lock()
        .unwrap()
        .iter()
        .filter(|goal| goal.scope.process_session_id == reopened.process_session_id)
        .cloned()
        .collect::<Vec<_>>();
    assert!(
        retries.len() > 1,
        "lost response did not exercise exact retry"
    );
    assert!(retries.iter().all(|goal| goal == selected));
    let engine = applications.engine(2);
    wait(async || engine.opens.load(Ordering::Acquire) == 1).await;
    *transport.held_snapshot.lock().unwrap() = None;
    until(&mut harness, || {
        engine.role_calls.load(Ordering::Acquire) > 0
    })
    .await;
    pump(&mut harness, 4).await;
    let observed = peers(&observe(&harness, transport.clone()).await)
        .into_iter()
        .find(|peer| peer.storage == reopened.storage)
        .unwrap();
    assert_eq!(
        observed
            .goals
            .iter()
            .filter(|entry| entry.goal == *selected)
            .count(),
        1
    );
    assert!(
        harness
            .set()
            .await
            .unwrap()
            .status
            .unwrap()
            .replica_goals
            .iter()
            .any(|entry| entry.goal == *selected
                && entry
                    .outcome
                    .as_ref()
                    .is_some_and(|outcome| outcome.status == ReplicaGoalStatus::Achieved))
    );
    assert_eq!(engine.opens.load(Ordering::Acquire), 1);
    assert_eq!(
        std::fs::read(engine.root.join("durable-state")).unwrap(),
        prefix
    );
    assert_eq!(transport.forbidden.load(Ordering::Acquire), 0);
    close(harness, &applications).await;
}

async fn build_goal(harness: &Harness<Applications>) -> ReplicaGoal {
    harness
        .set()
        .await
        .unwrap()
        .status
        .unwrap()
        .replica_goals
        .into_iter()
        .find(|entry| matches!(entry.goal.desired, ReplicaGoalKind::Build { .. }))
        .unwrap()
        .goal
}

async fn wait_ready(
    harness: &Harness<Applications>,
    transport: Arc<ObservedTransport>,
    accepted: bool,
) {
    wait(async || {
        peers(&observe(harness, transport.clone()).await)
            .iter()
            .any(|peer| {
                peer.remote_operations.iter().any(|remote| {
                    remote.state == RemoteReplicaState::Ready && remote.accepted == accepted
                })
            })
    })
    .await;
}

#[tokio::test]
async fn production_ordinary_target_reopen_invalidates_build_return_replay_acceptance_and_new_voter_admission()
 {
    for boundary in ["callback", "acceptance", "admission"] {
        let (mut harness, applications, transport) = start(false, 2, false).await;
        until(&mut harness, || {
            applications
                .engines
                .lock()
                .unwrap()
                .get(&1)
                .is_some_and(|engine| engine.builds.load(Ordering::Acquire) == 1)
        })
        .await;
        let source = applications.engine(1);
        let selected = build_goal(&harness).await;
        if boundary != "callback" {
            source.build_release.add_permits(1);
            wait_ready(&harness, transport.clone(), false).await;
        }
        if boundary == "admission" {
            harness.step().await.unwrap();
            wait_ready(&harness, transport.clone(), true).await;
            harness.step().await.unwrap();
            assert!(
                harness
                    .set()
                    .await
                    .unwrap()
                    .status
                    .unwrap()
                    .replica_goals
                    .iter()
                    .any(|entry| matches!(
                        entry.goal.desired,
                        ReplicaGoalKind::Reconfigure {
                            role: ReplicaRole::Primary,
                            ..
                        }
                    ))
            );
        }
        restart_target(&mut harness, &applications).await;
        let raw = observe(&harness, transport.clone()).await;
        let target = peers(&raw)
            .into_iter()
            .find(|peer| peer.storage.replica.replica_id.value() == 2)
            .unwrap();
        let ReplicaGoalKind::Build { remote } = &selected.desired else {
            unreachable!()
        };
        assert_ne!(
            target.process_session_id,
            remote.target.scope.process_session_id
        );
        assert_ne!(target.creation_id, remote.target.scope.creation_id);
        assert!(target.publication.is_none());
        if boundary == "callback" {
            source.late_build_success.store(true, Ordering::Release);
            source.build_release.add_permits(1);
            wait(async || source.trace.lock().unwrap().contains(&"build-return")).await;
        } else {
            let endpoint = crate::cluster_api::agent_endpoint(
                raw.pods
                    .iter()
                    .find(|pod| {
                        pod.uid().as_deref()
                            == Some(selected.scope.storage.replica.instance_id.as_str())
                    })
                    .unwrap(),
            )
            .unwrap();
            assert!(
                transport
                    .grpc
                    .submit_goal(
                        &endpoint,
                        "local-test-token",
                        proto::ReplicaGoalRequest {
                            protocol_version: ORDINARY_ACTION_FORMAT,
                            canonical_json: serde_json::to_vec(&selected).unwrap(),
                        }
                    )
                    .await
                    .is_err(),
                "exact completed replay ignored target reopen"
            );
        }
        pump(&mut harness, 3).await;
        let raw = observe(&harness, transport.clone()).await;
        assert!(
            peers(&raw)
                .iter()
                .flat_map(|peer| &peer.remote_operations)
                .all(|remote| remote.state != RemoteReplicaState::Ready)
        );
        assert_eq!(
            raw.set
                .status
                .unwrap()
                .public_v1_topology
                .unwrap()
                .member_count,
            1
        );
        assert!(
            !source
                .trace
                .lock()
                .unwrap()
                .contains(&"joint-configuration"),
            "stale target reached voting callback"
        );
        assert_eq!(source.builds.load(Ordering::Acquire), 1);
        close(harness, &applications).await;
    }
}

#[tokio::test]
async fn production_ordinary_ready_remove_is_local_retirement_not_target_drainage() {
    let (mut harness, applications, transport) = start(false, 2, false).await;
    until(&mut harness, || {
        applications
            .engines
            .lock()
            .unwrap()
            .get(&1)
            .is_some_and(|engine| engine.builds.load(Ordering::Acquire) == 1)
    })
    .await;
    let source = applications.engine(1);
    let target = applications.engine(2);
    source.build_release.add_permits(1);
    wait_ready(&harness, transport.clone(), false).await;
    harness.step().await.unwrap();
    wait_ready(&harness, transport.clone(), true).await;
    target.close_hold.store(true, Ordering::Release);
    scale(&harness, 1).await;
    until(&mut harness, || target.closes.load(Ordering::Acquire) > 0).await;
    assert_eq!(source.removes.load(Ordering::Acquire), 1);
    assert_eq!(source.aborts.load(Ordering::Acquire), 0);
    let raw = observe(&harness, transport.clone()).await;
    let snapshots = peers(&raw);
    let source_snapshot = snapshots
        .iter()
        .find(|peer| peer.storage.replica.replica_id.value() == 1)
        .unwrap();
    let target_snapshot = snapshots
        .iter()
        .find(|peer| peer.storage.replica.replica_id.value() == 2)
        .unwrap();
    assert!(
        source_snapshot
            .remote_operations
            .iter()
            .any(|remote| remote.state == RemoteReplicaState::Dropped)
    );
    assert!(
        target_snapshot.provider_writer_held
            && target_snapshot.withdrawn
            && target_snapshot.publication.is_none()
    );
    assert!(!target.engine_joined.load(Ordering::Acquire));
    assert_eq!(
        std::fs::read(target.root.join("copied-state")).unwrap(),
        b"ordinary-engine-state"
    );
    close(harness, &applications).await;
}

#[tokio::test]
async fn production_ordinary_lost_successful_remove_ack_recreation_persists_exact_result_then_joins_target()
 {
    let (mut harness, applications, transport) = start(false, 2, false).await;
    until(&mut harness, || {
        applications
            .engines
            .lock()
            .unwrap()
            .get(&1)
            .is_some_and(|engine| engine.builds.load(Ordering::Acquire) == 1)
    })
    .await;
    let source = applications.engine(1);
    let target = applications.engine(2);
    source.build_release.add_permits(1);
    wait_ready(&harness, transport.clone(), false).await;
    harness.step().await.unwrap();
    wait_ready(&harness, transport.clone(), true).await;
    target.close_hold.store(true, Ordering::Release);
    target.service_close_hold.store(true, Ordering::Release);
    transport
        .lose_remove_ack_reply
        .store(true, Ordering::Release);
    scale(&harness, 1).await;
    until(&mut harness, || {
        transport.lost_remove_ack.lock().unwrap().is_some()
    })
    .await;
    assert!(
        harness
            .last_error
            .as_ref()
            .unwrap()
            .contains("injected lost successful Remove acknowledgement reply")
    );
    let (acknowledgement, accepted) = transport.lost_remove_ack.lock().unwrap().clone().unwrap();
    assert!(!acknowledgement.logically_retired);
    let exact_outcome = accepted
        .goals
        .iter()
        .find(|observation| observation.goal == acknowledgement.goal)
        .unwrap()
        .clone();
    assert_eq!(exact_outcome.status, ReplicaGoalStatus::Achieved);
    let ReplicaGoalKind::Remove { remote: selection } = &acknowledgement.goal.desired else {
        unreachable!()
    };
    assert!(accepted.remote_operations.iter().any(|remote| {
        remote.operation_id == acknowledgement.goal.operation_id
            && remote.selection == **selection
            && remote.accepted
            && remote.state == RemoteReplicaState::Dropped
    }));
    let pending = harness
        .set()
        .await
        .unwrap()
        .status
        .unwrap()
        .replica_goals
        .into_iter()
        .find(|entry| entry.goal == acknowledgement.goal)
        .unwrap();
    assert!(
        pending.outcome.is_none(),
        "lost reply was persisted as success"
    );
    assert_eq!(transport.remove_acknowledgements.load(Ordering::Acquire), 1);
    assert_eq!(source.removes.load(Ordering::Acquire), 1);
    let before_target = peers(&observe(&harness, transport.clone()).await)
        .into_iter()
        .find(|peer| peer.storage.replica.replica_id.value() == 2)
        .unwrap();
    assert!(
        before_target.provider_writer_held
            && !before_target.withdrawn
            && before_target.publication.is_some()
    );
    assert_eq!(target.aborts.load(Ordering::Acquire), 0);
    assert_eq!(target.closes.load(Ordering::Acquire), 0);

    harness.recreate_controller().unwrap();
    harness.reconciler = production(&harness.api, transport.clone());
    assert!(
        harness
            .set()
            .await
            .unwrap()
            .status
            .unwrap()
            .replica_goals
            .contains(&pending)
    );
    until(&mut harness, || target.closes.load(Ordering::Acquire) > 0).await;
    pump(&mut harness, 2).await;
    let raw = observe(&harness, transport.clone()).await;
    let status = raw.set.status.as_ref().unwrap();
    let persisted = status
        .replica_goals
        .iter()
        .find(|entry| entry.goal == pending.goal)
        .unwrap();
    assert_eq!(persisted.digest, pending.digest);
    assert_eq!(persisted.outcome.as_ref(), Some(&exact_outcome));
    assert_eq!(transport.remove_acknowledgements.load(Ordering::Acquire), 2);
    assert_eq!(source.removes.load(Ordering::Acquire), 1);
    assert_eq!(source.builds.load(Ordering::Acquire), 1);
    assert_eq!(source.aborts.load(Ordering::Acquire), 0);
    let settlement = status
        .replica_goals
        .iter()
        .find(|entry| {
            entry.goal.scope.storage == before_target.storage
                && matches!(entry.goal.desired, ReplicaGoalKind::Abort)
        })
        .unwrap()
        .clone();
    assert!(settlement.outcome.is_none());
    let snapshots = peers(&raw);
    let source_snapshot = snapshots
        .iter()
        .find(|peer| peer.storage.replica.replica_id.value() == 1)
        .unwrap();
    assert_eq!(
        source_snapshot.remote_operations,
        accepted.remote_operations
    );
    let target_snapshot = snapshots
        .iter()
        .find(|peer| peer.storage.replica.replica_id.value() == 2)
        .unwrap();
    assert!(
        target_snapshot.provider_writer_held
            && target_snapshot.withdrawn
            && target_snapshot.publication.is_none()
            && target_snapshot.read_status != AccessStatus::Granted
            && target_snapshot.write_status != AccessStatus::Granted
    );
    assert_eq!(target.aborts.load(Ordering::Acquire), 1);
    assert!(!target.engine_joined.load(Ordering::Acquire));
    assert!(!target.service_joined.load(Ordering::Acquire));
    target.close_release.add_permits(1);
    wait(async || {
        target.engine_joined.load(Ordering::Acquire)
            && target.service_closes.load(Ordering::Acquire) == 1
    })
    .await;
    pump(&mut harness, 2).await;
    let target_snapshot = peers(&observe(&harness, transport.clone()).await)
        .into_iter()
        .find(|peer| peer.storage.replica.replica_id.value() == 2)
        .unwrap();
    assert!(target_snapshot.provider_writer_held && target_snapshot.publication.is_none());
    assert!(!target.service_joined.load(Ordering::Acquire));
    assert!(
        harness
            .set()
            .await
            .unwrap()
            .status
            .unwrap()
            .replica_goals
            .contains(&settlement)
    );
    target.service_close_release.add_permits(1);
    wait(async || target.service_joined.load(Ordering::Acquire)).await;
    wait(async || {
        peers(&observe(&harness, transport.clone()).await)
            .iter()
            .any(|peer| peer.storage == before_target.storage && !peer.provider_writer_held)
    })
    .await;
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            harness.step().await.unwrap();
            if harness
                .set()
                .await
                .unwrap()
                .status
                .unwrap()
                .replica_goals
                .iter()
                .any(|entry| {
                    entry.goal == settlement.goal
                        && entry
                            .outcome
                            .as_ref()
                            .is_some_and(|outcome| outcome.status == ReplicaGoalStatus::Achieved)
                })
            {
                break;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(target.closes.load(Ordering::Acquire), 1);
    assert_eq!(target.service_closes.load(Ordering::Acquire), 1);
    assert_eq!(source.removes.load(Ordering::Acquire), 1);
    assert_eq!(transport.forbidden.load(Ordering::Acquire), 0);
    close(harness, &applications).await;
}

#[tokio::test]
async fn production_ordinary_matched_crd_and_canonical_goals_reject_before_any_transport_or_mutation()
 {
    let (mut harness, applications, transport) = start(false, 2, false).await;
    until(&mut harness, || {
        applications
            .engines
            .lock()
            .unwrap()
            .get(&1)
            .is_some_and(|engine| engine.builds.load(Ordering::Acquire) == 1)
    })
    .await;
    let sets = Api::<KubericSet>::namespaced(harness.api.client(), NAMESPACE);
    let original = sets.get(NAME).await.unwrap();
    for case in ["spec", "goal-format", "digest", "legacy-command"] {
        let mut invalid = sets.get(NAME).await.unwrap();
        invalid.spec = original.spec.clone();
        invalid.status = original.status.clone();
        match case {
            "spec" => invalid.spec.hosting_format = 14,
            "goal-format" => {
                invalid.status.as_mut().unwrap().replica_goals[0]
                    .goal
                    .format = 14
            }
            "digest" => {
                invalid.status.as_mut().unwrap().replica_goals[0]
                    .goal
                    .operation_id = OperationId::new("changed-with-old-digest")
            }
            "legacy-command" => {
                let goal = &invalid.status.as_ref().unwrap().replica_goals[0].goal;
                invalid.status.as_mut().unwrap().public_v1_operation =
                    Some(crate::crd::PublicV1OperationStatus {
                        target: goal.scope.storage.replica.clone(),
                        operation_id: goal.operation_id.clone(),
                        revision: goal.revision,
                        canonical_command_json: "{}".into(),
                    });
            }
            _ => unreachable!(),
        }
        sets.replace(NAME, &PostParams::default(), &invalid)
            .await
            .unwrap();
        // Status has its own Kubernetes subresource.
        let mut current = sets.get(NAME).await.unwrap();
        current.status = invalid.status;
        sets.replace_status(NAME, &PostParams::default(), &current)
            .await
            .unwrap();
        let before = snapshot(&harness.api);
        let calls = transport.snapshots.load(Ordering::Acquire);
        assert!(harness.reconciler.reconcile(NAMESPACE, NAME).await.is_err());
        assert_eq!(snapshot(&harness.api), before);
        assert_eq!(transport.snapshots.load(Ordering::Acquire), calls);
        assert_eq!(applications.engine(1).builds.load(Ordering::Acquire), 1);
    }
    let mut restored = sets.get(NAME).await.unwrap();
    restored.spec = original.spec;
    sets.replace(NAME, &PostParams::default(), &restored)
        .await
        .unwrap();
    let mut restored = sets.get(NAME).await.unwrap();
    restored.status = original.status;
    sets.replace_status(NAME, &PostParams::default(), &restored)
        .await
        .unwrap();
    close(harness, &applications).await;
}

#[tokio::test]
async fn production_ordinary_same_format_crash_image_preserves_uncertainty_and_opens_fresh_discovery_only()
 {
    let (mut harness, applications, transport) = start(false, 2, false).await;
    until(&mut harness, || {
        applications
            .engines
            .lock()
            .unwrap()
            .get(&1)
            .is_some_and(|engine| engine.builds.load(Ordering::Acquire) == 1)
    })
    .await;
    let selected = build_goal(&harness).await;
    let source = applications.engine(1);
    let raw = observe(&harness, transport.clone()).await;
    let pod = raw
        .pods
        .iter()
        .find(|pod| {
            pod.uid().as_deref() == Some(selected.scope.storage.replica.instance_id.as_str())
        })
        .unwrap();
    let root = tempfile::Builder::new()
        .prefix(".m1-production-crash-image-")
        .tempdir_in(std::env::current_dir().unwrap())
        .unwrap();
    let (config, _) = pod::resolve(
        pod,
        &raw.pvcs,
        &raw.secrets,
        root.path(),
        "ordinary-independent",
    )
    .unwrap();
    std::fs::create_dir_all(config.data_root.join(".kuberic")).unwrap();
    let database = config.data_root.join(".kuberic/agent.sqlite3");
    std::fs::copy(
        harness
            .data_root(selected.scope.storage.replica.instance_id.as_str())
            .await
            .unwrap()
            .join(".kuberic/agent.sqlite3"),
        &database,
    )
    .unwrap();
    let image = std::fs::read(&database).unwrap();
    let prefix = std::fs::read(source.root.join("durable-state")).unwrap();
    // The selection is committed and no SQLite transaction is open. Only this
    // isolated metadata image is reopened; no application data is reset.
    source.late_build_success.store(true, Ordering::Release);
    source.build_release.add_permits(1);
    harness
        .stop(selected.scope.storage.replica.instance_id.as_str())
        .await
        .unwrap();
    assert!(
        source.engine_joined.load(Ordering::Acquire)
            && source.service_joined.load(Ordering::Acquire)
    );
    let fresh_engine = Engine::new(source.root.clone(), true);
    let fresh_application = Arc::new(Application(fresh_engine.clone()));
    let resolver = Arc::new(HarnessResolver {
        api: harness.api.clone(),
        dns: KubernetesDnsResolver::new(config.resource_uid.clone(), NAMESPACE),
    });
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let (shutdown, receiver) = watch::channel(false);
    let host = ReplicaHost::new(
        config,
        fresh_application.clone(),
        kuberic_runtime::host::ApplicationStorageState::Established,
        resolver,
    )
    .with_application_storage_paths(BTreeMap::from([(
        "application-0".into(),
        source.root.clone(),
    )]));
    let mut running = host
        .start_with_shutdown(listener, receiver)
        .await
        .unwrap()
        .unwrap();
    let current = harness.api.get(Kind::Pod, &pod.name_any()).unwrap();
    harness
        .api
        .update_infrastructure(
            Kind::Pod,
            &pod.name_any(),
            selected.scope.storage.replica.instance_id.as_str(),
            current["metadata"]["resourceVersion"].as_str().unwrap(),
            |pod| {
                pod["metadata"]["annotations"][CONTROL_ADDRESS_ANNOTATION] =
                    serde_json::Value::String(endpoint.clone());
            },
        )
        .unwrap();
    let reopened = peers(&observe(&harness, transport.clone()).await)
        .into_iter()
        .find(|peer| peer.storage.replica.replica_id.value() == 1)
        .unwrap();
    assert!(reopened.publication.is_none());
    assert_ne!(
        reopened.process_session_id,
        selected.scope.process_session_id
    );
    assert_ne!(reopened.creation_id, selected.scope.creation_id);
    assert!(
        reopened
            .goals
            .iter()
            .any(|observation| observation.goal == selected
                && matches!(observation.status, ReplicaGoalStatus::Uncertain(_)))
    );
    assert_eq!(fresh_engine.builds.load(Ordering::Acquire), 0);
    pump(&mut harness, 5).await;
    let reopened = peers(&observe(&harness, transport.clone()).await)
        .into_iter()
        .find(|peer| peer.storage.replica.replica_id.value() == 1)
        .unwrap();
    assert_eq!(
        reopened.publication.as_ref().unwrap().stage,
        ReplicaPublicationStage::DiscoveryPending
    );
    assert!(
        reopened
            .goals
            .iter()
            .any(|observation| observation.goal == selected
                && matches!(observation.status, ReplicaGoalStatus::Uncertain(_)))
    );
    assert!(
        reopened
            .remote_operations
            .iter()
            .all(|remote| remote.state != RemoteReplicaState::Ready)
    );
    assert_ne!(reopened.write_status, AccessStatus::Granted);
    assert_eq!(fresh_engine.opens.load(Ordering::Acquire), 1);
    assert_eq!(fresh_engine.builds.load(Ordering::Acquire), 0);
    assert_eq!(
        std::fs::read(source.root.join("durable-state")).unwrap(),
        prefix
    );
    assert!(!image.is_empty());
    fresh_engine.release();
    shutdown.send_replace(true);
    running.shutdown();
    running.wait().await.unwrap();
    assert!(
        fresh_engine.engine_joined.load(Ordering::Acquire)
            && fresh_engine.service_joined.load(Ordering::Acquire)
    );
    close(harness, &applications).await;
}

#[tokio::test]
async fn production_ordinary_retained_remove_before_build_survives_recreation_and_blocks_late_selection()
 {
    let (mut harness, applications, transport) = start(false, 2, false).await;
    loop {
        harness.step().await.unwrap();
        if harness
            .set()
            .await
            .unwrap()
            .status
            .as_ref()
            .is_some_and(|status| status.public_v1_topology.is_some())
        {
            break;
        }
    }
    let raw = observe(&harness, transport.clone()).await;
    let snapshots = peers(&raw);
    let source = snapshots
        .iter()
        .find(|peer| peer.storage.replica.replica_id.value() == 1)
        .unwrap();
    let target = snapshots
        .iter()
        .find(|peer| peer.storage.replica.replica_id.value() == 2)
        .unwrap();
    let mut tombstone = source
        .goals
        .iter()
        .max_by_key(|observation| observation.goal.revision)
        .unwrap()
        .goal
        .clone();
    tombstone.revision += 1;
    tombstone.operation_id = OperationId::new("retained-remove-before-build");
    tombstone.desired = ReplicaGoalKind::Remove {
        remote: Box::new(RemoteReplicaSelection {
            original_member: None,
            source: source.publication.clone().unwrap(),
            target: target.publication.clone().unwrap(),
            target_control_address: crate::cluster_api::agent_endpoint(
                raw.pods
                    .iter()
                    .find(|pod| {
                        pod.uid().as_deref() == Some(target.storage.replica.instance_id.as_str())
                    })
                    .unwrap(),
            )
            .unwrap(),
            generation: 1,
        }),
    };
    let sets = Api::<KubericSet>::namespaced(harness.api.client(), NAMESPACE);
    let mut set = sets.get(NAME).await.unwrap();
    set.status
        .as_mut()
        .unwrap()
        .replica_goals
        .push(crate::crd::RetainedReplicaGoal::new(tombstone.clone()).unwrap());
    sets.replace_status(NAME, &PostParams::default(), &set)
        .await
        .unwrap();
    harness.recreate_controller().unwrap();
    harness.reconciler = production(&harness.api, transport.clone());
    loop {
        harness.step().await.unwrap();
        if harness
            .set()
            .await
            .unwrap()
            .status
            .unwrap()
            .replica_goals
            .iter()
            .find(|entry| entry.goal == tombstone)
            .unwrap()
            .outcome
            .is_some()
        {
            break;
        }
    }
    pump(&mut harness, 3).await;
    assert_eq!(applications.engine(1).builds.load(Ordering::Acquire), 0);
    assert_eq!(applications.engine(1).removes.load(Ordering::Acquire), 0);
    let rejection = harness.last_error.as_ref().unwrap();
    assert!(
        rejection.contains("tombstone") || rejection.contains("watermark"),
        "{rejection}"
    );
    let raw = observe(&harness, transport.clone()).await;
    assert!(
        peers(&raw)
            .iter()
            .flat_map(|peer| &peer.remote_operations)
            .any(|remote| remote.state == RemoteReplicaState::Dropped)
    );
    close(harness, &applications).await;
}

#[tokio::test]
async fn production_ordinary_failed_prepare_withdraws_before_retained_engine_service_and_writer_settlement()
 {
    let (mut harness, applications, transport) = start(true, 2, false).await;
    until(&mut harness, || {
        let engines = applications.engines.lock().unwrap();
        engines.len() == 2
            && engines
                .values()
                .all(|engine| engine.role_calls.load(Ordering::Acquire) > 0)
    })
    .await;
    let target = applications.engine(2);
    target.role_failure.store(true, Ordering::Release);
    target.close_hold.store(true, Ordering::Release);
    target.service_close_hold.store(true, Ordering::Release);
    let before = std::fs::read(target.root.join("durable-state")).unwrap();
    target.role_release.add_permits(1);
    wait(async || target.closes.load(Ordering::Acquire) == 1).await;
    let raw = observe(&harness, transport.clone()).await;
    let target_snapshot = peers(&raw)
        .into_iter()
        .find(|peer| peer.storage.replica.replica_id.value() == 2)
        .unwrap();
    assert!(
        target_snapshot.withdrawn
            && target_snapshot.publication.is_none()
            && target_snapshot.provider_writer_held
    );
    assert_ne!(target_snapshot.write_status, AccessStatus::Granted);
    assert_ne!(target_snapshot.read_status, AccessStatus::Granted);
    assert!(!target.engine_joined.load(Ordering::Acquire));
    assert!(!target.service_joined.load(Ordering::Acquire));
    let trace = target.trace.lock().unwrap().clone();
    assert!(
        trace
            .iter()
            .position(|event| *event == "replicator-abort")
            .unwrap()
            < trace
                .iter()
                .position(|event| *event == "replicator-close")
                .unwrap()
    );
    let context = target.context();
    assert!(context.local_publication().await.unwrap().is_none());
    assert_ne!(context.write_status().await.unwrap(), AccessStatus::Granted);
    pump(&mut harness, 2).await;
    let retained = harness.set().await.unwrap().status.unwrap();
    assert!(retained.replica_goals.iter().any(|entry| {
        entry
            .outcome
            .as_ref()
            .is_some_and(|outcome| matches!(outcome.status, ReplicaGoalStatus::Failed(_)))
    }));
    target.close_release.add_permits(1);
    wait(async || target.service_closes.load(Ordering::Acquire) == 1).await;
    assert!(target.engine_joined.load(Ordering::Acquire));
    assert!(!target.service_joined.load(Ordering::Acquire));
    assert!(
        peers(&observe(&harness, transport.clone()).await)
            .iter()
            .find(|peer| peer.storage.replica.replica_id.value() == 2)
            .unwrap()
            .provider_writer_held
    );
    target.service_close_release.add_permits(1);
    wait(async || {
        !peers(&observe(&harness, transport.clone()).await)
            .iter()
            .find(|peer| peer.storage.replica.replica_id.value() == 2)
            .unwrap()
            .provider_writer_held
    })
    .await;
    assert!(target.service_joined.load(Ordering::Acquire));
    assert_eq!(
        std::fs::read(target.root.join("durable-state")).unwrap(),
        before
    );
    assert_eq!(applications.engine(1).builds.load(Ordering::Acquire), 0);
    close(harness, &applications).await;
}

#[tokio::test]
async fn production_ordinary_inbuild_cancel_retains_late_callback_and_joins_before_writer_release()
{
    let (mut harness, applications, transport) = start(false, 2, false).await;
    until(&mut harness, || {
        applications
            .engines
            .lock()
            .unwrap()
            .get(&1)
            .is_some_and(|engine| engine.builds.load(Ordering::Acquire) == 1)
    })
    .await;
    let source = applications.engine(1);
    let target = applications.engine(2);
    source.close_hold.store(true, Ordering::Release);
    source.service_close_hold.store(true, Ordering::Release);
    scale(&harness, 1).await;
    until(&mut harness, || source.closes.load(Ordering::Acquire) == 1).await;
    let raw = observe(&harness, transport.clone()).await;
    let snapshot = peers(&raw)
        .into_iter()
        .find(|peer| peer.storage.replica.replica_id.value() == 1)
        .unwrap();
    assert!(snapshot.withdrawn && snapshot.publication.is_none() && snapshot.provider_writer_held);
    assert!(
        !snapshot
            .remote_operations
            .iter()
            .any(|remote| remote.state == RemoteReplicaState::Ready)
    );
    assert_eq!(source.removes.load(Ordering::Acquire), 0);
    assert!(!source.trace.lock().unwrap().contains(&"build-return"));
    source.close_release.add_permits(1);
    wait(async || source.service_closes.load(Ordering::Acquire) == 1).await;
    assert!(source.engine_joined.load(Ordering::Acquire));
    source.service_close_release.add_permits(1);
    wait(async || source.service_joined.load(Ordering::Acquire)).await;
    let held = peers(&observe(&harness, transport.clone()).await)
        .into_iter()
        .find(|peer| peer.storage.replica.replica_id.value() == 1)
        .unwrap();
    assert!(
        held.provider_writer_held,
        "ordinary root was inferred joined while actual Build is held"
    );
    source.build_release.add_permits(1);
    wait(async || {
        !peers(&observe(&harness, transport.clone()).await)
            .iter()
            .find(|peer| peer.storage.replica.replica_id.value() == 1)
            .unwrap()
            .provider_writer_held
    })
    .await;
    pump(&mut harness, 2).await;
    let raw = observe(&harness, transport.clone()).await;
    assert!(peers(&raw).iter().all(|peer| {
        peer.publication.is_none()
            && peer
                .remote_operations
                .iter()
                .all(|remote| remote.state != RemoteReplicaState::Ready)
    }));
    assert!(
        !target.root.join("copied-state").exists(),
        "late Build mutated a withdrawn target"
    );
    assert_eq!(source.builds.load(Ordering::Acquire), 1);
    assert_eq!(
        std::fs::read(source.root.join("durable-state")).unwrap(),
        b"acknowledged-application-prefix"
    );
    close(harness, &applications).await;
}

#[tokio::test]
async fn production_ordinary_stale_canonical_revision_epoch_instance_and_goal_id_reject_without_effects()
 {
    let (mut harness, applications, transport) = start(false, 2, false).await;
    until(&mut harness, || {
        applications
            .engines
            .lock()
            .unwrap()
            .get(&1)
            .is_some_and(|engine| engine.builds.load(Ordering::Acquire) == 1)
    })
    .await;
    let raw = observe(&harness, transport.clone()).await;
    let original = raw
        .set
        .status
        .as_ref()
        .unwrap()
        .replica_goals
        .iter()
        .find(|entry| matches!(entry.goal.desired, ReplicaGoalKind::Build { .. }))
        .unwrap()
        .goal
        .clone();
    let pod = raw
        .pods
        .iter()
        .find(|pod| {
            pod.uid().as_deref() == Some(original.scope.storage.replica.instance_id.as_str())
        })
        .unwrap();
    let endpoint = crate::cluster_api::agent_endpoint(pod).unwrap();
    let before = peers(&raw)
        .into_iter()
        .find(|peer| peer.storage == original.scope.storage)
        .unwrap();
    let mut changed_id = original.clone();
    changed_id.desired = ReplicaGoalKind::Abort;
    let mut stale_revision = changed_id.clone();
    stale_revision.operation_id = OperationId::new("stale-revision");
    let mut stale_epoch = stale_revision.clone();
    stale_epoch.operation_id = OperationId::new("stale-epoch");
    stale_epoch.revision += 1;
    stale_epoch.scope.epoch.configuration_number = 0;
    stale_epoch.scope.configuration.current.epoch = stale_epoch.scope.epoch;
    stale_epoch.scope.configuration.current.configuration_id =
        stale_epoch.scope.configuration.current.expected_id();
    let mut stale_instance = stale_revision.clone();
    stale_instance.operation_id = OperationId::new("stale-instance");
    stale_instance.revision += 1;
    stale_instance.scope.storage.replica.instance_id = ReplicaInstanceId::new("another-pod");
    let mut stale_process = stale_revision.clone();
    stale_process.operation_id = OperationId::new("stale-process");
    stale_process.revision += 1;
    stale_process.scope.process_session_id = ProcessSessionId::new("another-process");
    for invalid in [
        changed_id,
        stale_revision,
        stale_epoch,
        stale_instance,
        stale_process,
    ] {
        assert!(
            transport
                .grpc
                .submit_goal(
                    &endpoint,
                    "local-test-token",
                    proto::ReplicaGoalRequest {
                        protocol_version: ORDINARY_ACTION_FORMAT,
                        canonical_json: serde_json::to_vec(&invalid).unwrap(),
                    }
                )
                .await
                .is_err()
        );
    }
    let after = peers(&observe(&harness, transport.clone()).await)
        .into_iter()
        .find(|peer| peer.storage == original.scope.storage)
        .unwrap();
    assert_eq!(before, after);
    assert_eq!(applications.engine(1).builds.load(Ordering::Acquire), 1);
    assert_eq!(applications.engine(1).aborts.load(Ordering::Acquire), 0);
    close(harness, &applications).await;
}
