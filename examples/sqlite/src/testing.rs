//! Package-local in-process v2 fixture. Authority belongs to the agent-side test
//! driver, never to the production SQLite service. Routing is agent-owned test infrastructure.
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use kuberic_agent::hosting::PodRuntime;
use kuberic_agent::hosting::PreparedCopy;
use kuberic_agent::runtime_adapter::RuntimeAdapter;
use kuberic_agent::session::ProcessSession;
use kuberic_agent::sqlite_store::SqliteStore;
use kuberic_agent::state::{AgentState, SCHEMA_VERSION, StorageIdentity};
use kuberic_agent::store::AgentStore;
use kuberic_agent::testing::{InProcessTransport, Message, TransportError, TransportEvent};
use kuberic_protocol::types::*;
use kuberic_runtime::application::OpenMode;
use kuberic_runtime::replicator::copy::{BuildConfiguration, PrepareCopyRequest};
use kuberic_runtime_internal::authority::AdmittedAuthority;
use kuberic_runtime_internal::authority::{
    BuildAuthority, BuildAuthorityStore, BuildProgressStore,
};
use kuberic_runtime_internal::effects::{RuntimeEffect, RuntimeEffectAction};
use tonic::{Request, Status};

use crate::proto::sqlite_store_server::SqliteStore as _;
use crate::{SqlitePersistence, proto, server::SqliteServer, service::SqliteService};

/// One-shot deterministic application cut, available only in test builds.
#[derive(Default)]
pub struct PauseGate {
    armed: std::sync::atomic::AtomicBool,
    entered: tokio::sync::Notify,
    released: tokio::sync::Notify,
}

impl PauseGate {
    pub fn arm(&self) {
        assert!(!self.armed.swap(true, std::sync::atomic::Ordering::SeqCst));
    }
    pub async fn wait_entered(&self) {
        tokio::time::timeout(Duration::from_secs(5), self.entered.notified())
            .await
            .expect("application reached the requested durable cut");
    }
    pub fn release(&self) {
        self.released.notify_one();
    }
    pub(crate) async fn pause(&self) {
        if self.armed.swap(false, std::sync::atomic::Ordering::SeqCst) {
            self.entered.notify_one();
            self.released.notified().await;
        }
    }
}

pub fn scratch() -> tempfile::TempDir {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/sqlite-v2-tests");
    std::fs::create_dir_all(&root).unwrap();
    tempfile::tempdir_in(root).unwrap()
}

pub fn identity(id: i64) -> ReplicaIdentity {
    ReplicaIdentity {
        replica_id: ReplicaId::new(id),
        instance_id: ReplicaInstanceId::new(format!("sqlite-{id}")),
        agent_generation: AgentGeneration::new(format!("sqlite-generation-{id}")),
    }
}

pub fn configuration(
    members: &[ReplicaIdentity],
    primary: usize,
    epoch: i64,
) -> ConfigurationDescriptor {
    ConfigurationDescriptor::new(
        Epoch::new(0, epoch),
        members[primary].replica_id,
        members
            .iter()
            .enumerate()
            .map(|(index, identity)| ConfigurationMember {
                identity: identity.clone(),
                role: if index == primary {
                    ReplicaRole::Primary
                } else {
                    ReplicaRole::ActiveSecondary
                },
            })
            .collect(),
        members.len() as u32 / 2 + 1,
    )
}

pub fn authority(
    local: ReplicaIdentity,
    configuration: ConfigurationDescriptor,
) -> AdmittedAuthority {
    AdmittedAuthority {
        local_identity: local,
        current_configuration: configuration,
        previous_configuration: None,
        transition_kind: None,
        switchover_handoff: None,
        secondary_removal: None,
        scale_up: None,
    }
}

pub struct SqlitePod {
    pub runtime: Arc<PodRuntime>,
    pub application: Arc<SqliteService>,
    pub store: Arc<SqliteStore>,
    pub server: SqliteServer,
    pub root: PathBuf,
    pub identity: ReplicaIdentity,
    pub session: ProcessSession,
    replicas: u32,
}

impl SqlitePod {
    pub async fn new(id: i64, root: PathBuf, replicas: u32) -> Self {
        Self::with_identity(identity(id), root, replicas).await
    }

    pub async fn with_identity(identity: ReplicaIdentity, root: PathBuf, replicas: u32) -> Self {
        let id = identity.replica_id.value();
        let metadata = SqliteStore::metadata_database_path(&root);
        let store = if metadata.exists() {
            SqliteStore::open_existing(metadata, None).unwrap()
        } else {
            assert!(SqlitePersistence::is_fresh_empty(&root.join("application")).unwrap());
            SqliteStore::create_authorized(
                metadata,
                AgentState::new(StorageIdentity {
                    schema_version: SCHEMA_VERSION,
                    resource_uid: ResourceUid::new("sqlite-test"),
                    pod_uid: PodUid::new(identity.instance_id.as_str()),
                    pvc_uid: PvcUid::new(
                        if identity.instance_id == crate::testing::identity(id).instance_id {
                            format!("sqlite-pvc-{id}")
                        } else {
                            format!("sqlite-pvc-{}", identity.instance_id)
                        },
                    ),
                    initialization_id: InitializationId::new(format!("sqlite-init-{id}")),
                    local_identity: identity.clone(),
                    effective_policy: EffectivePolicy::fixed(replicas, 30).unwrap(),
                }),
            )
            .unwrap()
        };
        let store = Arc::new(store);
        assert_eq!(
            store.load_state().await.unwrap().identity.local_identity,
            identity
        );
        let persistence = Arc::new(SqlitePersistence::open(root.join("application")).unwrap());
        let application =
            Arc::new(SqliteService::new(persistence, format!("in-process://sqlite-{id}")).unwrap());
        let runtime = Arc::new(PodRuntime::new(
            identity.clone(),
            application.clone(),
            store.clone(),
        ));
        let server = SqliteServer::new(application.clone());
        Self {
            runtime,
            application,
            store,
            server,
            root,
            identity,
            session: ProcessSession::new(),
            replicas,
        }
    }

    pub async fn effect(&self, action: RuntimeEffectAction) -> kuberic_agent::Result<()> {
        let sequence = self.store.load_state().await?.next_effect_sequence;
        self.effect_as(
            OperationId::new(format!("sqlite-effect-{sequence}")),
            action,
        )
        .await
    }

    pub async fn effect_as(
        &self,
        operation_id: OperationId,
        action: RuntimeEffectAction,
    ) -> kuberic_agent::Result<()> {
        let sequence = self.store.load_state().await?.next_effect_sequence;
        RuntimeAdapter::new(self.store.clone(), self.runtime.clone())
            .execute(RuntimeEffect {
                operation_id,
                sequence,
                action,
            })
            .await?;
        Ok(())
    }

    pub async fn open(&self) {
        self.effect(RuntimeEffectAction::Open(OpenMode::Existing))
            .await
            .unwrap();
    }

    pub async fn singleton(root: PathBuf) -> Self {
        let pod = Self::new(1, root, 1).await;
        pod.open().await;
        pod.effect(RuntimeEffectAction::AdmitAuthority(Box::new(authority(
            pod.identity.clone(),
            configuration(std::slice::from_ref(&pod.identity), 0, 1),
        ))))
        .await
        .unwrap();
        pod.effect(RuntimeEffectAction::ChangeRole(ReplicaRole::Primary))
            .await
            .unwrap();
        pod.grant().await;
        pod
    }

    pub async fn grant(&self) {
        self.effect(RuntimeEffectAction::SetAccessStatus {
            read: AccessStatus::Granted,
            write: AccessStatus::Granted,
        })
        .await
        .unwrap();
    }

    /// Stop attached routes and finish/cancel client requests first. Recovery
    /// constructs new stores, service and runtime rather than reusing journals.
    pub async fn reopen(self) -> Self {
        self.reopen_with_access(None).await
    }

    /// Restart with access closed so transport can be rebound before journal recovery.
    pub async fn reopen_closed(self) -> Self {
        self.reopen_with_access(Some((
            AccessStatus::ReconfigurationPending,
            AccessStatus::ReconfigurationPending,
        )))
        .await
    }

    async fn reopen_with_access(self, access: Option<(AccessStatus, AccessStatus)>) -> Self {
        let identity = self.identity.clone();
        let root = self.root.clone();
        let replicas = self.replicas;
        let old_session = self.session.id().clone();
        self.runtime.abort();
        drop(self);
        let pod = Self::with_identity(identity, root, replicas).await;
        assert_ne!(*pod.session.id(), old_session);
        let state = pod.store.load_state().await.unwrap();
        assert!(
            state.pending_effect.is_none(),
            "fixture restart requires a settled control effect"
        );
        let (read, write) = access.unwrap_or((state.read_status, state.write_status));
        pod.runtime
            .reconstruct(OpenMode::Existing, state.role, read, write, None)
            .await
            .unwrap();
        // Build admission is restored from the durable command journal, not
        // from any old runtime's build maps or remembered application progress.
        if state.role == ReplicaRole::IdleSecondary {
            for command in state
                .build_commands
                .values()
                .filter(|command| !command.retire)
            {
                if !state.retired_builds.contains(&command.operation_id)
                    && let Some(build) = &command.authority
                {
                    pod.effect(RuntimeEffectAction::AdmitBuildAuthority(Box::new(
                        build.clone(),
                    )))
                    .await
                    .unwrap();
                }
            }
        }
        pod
    }

    pub async fn execute(&self, sql: &str) -> Result<proto::ExecuteResponse, Status> {
        self.server
            .execute(Request::new(proto::ExecuteRequest {
                sql: sql.into(),
                params: Vec::new(),
            }))
            .await
            .map(tonic::Response::into_inner)
    }
    pub async fn query(&self, sql: &str) -> Result<proto::QueryResponse, Status> {
        self.server
            .query(Request::new(proto::QueryRequest {
                sql: sql.into(),
                params: Vec::new(),
            }))
            .await
            .map(tonic::Response::into_inner)
    }
    pub async fn batch(&self, statements: &[&str]) -> Result<proto::ExecuteBatchResponse, Status> {
        self.server
            .execute_batch(Request::new(proto::ExecuteBatchRequest {
                statements: statements.iter().map(|s| (*s).to_owned()).collect(),
            }))
            .await
            .map(tonic::Response::into_inner)
    }
    pub async fn count(&self) -> i64 {
        let result = self.query("SELECT COUNT(*) FROM data").await.unwrap();
        match result.rows[0].values[0].kind {
            Some(proto::value::Kind::IntegerValue(value)) => value,
            _ => panic!("expected integer"),
        }
    }
}

impl Drop for SqlitePod {
    fn drop(&mut self) {
        self.runtime.abort();
    }
}

pub struct Routes {
    task: tokio::task::JoinHandle<()>,
    pub events: tokio::sync::mpsc::UnboundedReceiver<TransportEvent>,
}
impl Routes {
    pub async fn wait_for_applied(&mut self, peers: &[&SqlitePod], lsn: i64) {
        let mut remaining = peers
            .iter()
            .map(|p| p.identity.clone())
            .collect::<std::collections::BTreeSet<_>>();
        tokio::time::timeout(Duration::from_secs(5), async {
            while !remaining.is_empty() {
                match self.events.recv().await.expect("running transport") {
                    TransportEvent::Applied {
                        acknowledgement, ..
                    } => {
                        let id: ReplicaIdentity =
                            acknowledgement.receiver.unwrap().try_into().unwrap();
                        if let Some(peer) = peers.iter().find(|p| p.identity == id) {
                            assert_eq!(
                                acknowledgement.receiver_session_id,
                                peer.session.id().as_str()
                            );
                            if acknowledgement.applied_lsn >= lsn {
                                remaining.remove(&id);
                            }
                        }
                    }
                    TransportEvent::Received { .. } => {}
                    other => panic!("unexpected event while waiting for peer ACKs: {other:?}"),
                }
            }
        })
        .await
        .expect("fresh process-session durable ACKs");
    }

    /// Await driver teardown before reopening any of its registered replicas.
    pub async fn stop(mut self) {
        self.task.abort();
        match (&mut self.task).await {
            Ok(()) => {}
            Err(error) if error.is_cancelled() => {}
            Err(error) => panic!("in-process transport driver failed: {error}"),
        }
    }
}
impl Drop for Routes {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Deliver actual public wire messages and their explicit durable service ACKs.
pub async fn route(source: &SqlitePod, targets: &[&SqlitePod]) -> Routes {
    let mut transport = InProcessTransport::new();
    for pod in std::iter::once(source).chain(targets.iter().copied()) {
        transport
            .register(pod.runtime.clone(), pod.session.id().clone())
            .await
            .unwrap();
    }
    let (sender, events) = tokio::sync::mpsc::unbounded_channel();
    let task = tokio::spawn(async move {
        loop {
            for event in transport.next().await.events {
                if let TransportEvent::Rejected { error, .. } = &event {
                    if matches!(error, TransportError::Unregistered(_)) {
                        tracing::debug!(%error, "test link has no reachable exact receiver");
                    } else {
                        tracing::error!(%error, "in-process delivery rejected");
                    }
                }
                if sender.send(event).is_err() {
                    return;
                }
            }
        }
    });
    Routes { task, events }
}

pub async fn wait_applied(pods: &[&SqlitePod], lsn: i64) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let mut applied = true;
            for pod in pods {
                applied &= pod
                    .runtime
                    .snapshot()
                    .await
                    .verified_replication_lsn
                    .is_some_and(|verified| verified >= lsn);
            }
            if applied {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("replicas durably acknowledged the operation");
}

pub async fn bootstrap(pods: &[&SqlitePod]) -> ConfigurationDescriptor {
    let members = pods
        .iter()
        .map(|pod| pod.identity.clone())
        .collect::<Vec<_>>();
    let configuration = configuration(&members, 0, 1);
    for pod in pods {
        pod.open().await;
    }
    pods[0]
        .effect(RuntimeEffectAction::ChangeRole(ReplicaRole::Primary))
        .await
        .unwrap();
    let mut builds = Vec::new();
    for target in &pods[1..] {
        target
            .effect(RuntimeEffectAction::ChangeRole(ReplicaRole::IdleSecondary))
            .await
            .unwrap();
        let build_id =
            OperationId::new(format!("bootstrap-{}", target.identity.replica_id.value()));
        let build = pods[0]
            .runtime
            .authorize_build(
                build_id.clone(),
                target.identity.clone(),
                BuildConfiguration::Bootstrap(configuration.clone()),
            )
            .await
            .unwrap();
        target
            .effect(RuntimeEffectAction::AdmitBuildAuthority(Box::new(build)))
            .await
            .unwrap();
        let mut prepared = pods[0]
            .runtime
            .data_plane()
            .prepare_copy(PrepareCopyRequest {
                build_id: build_id.clone(),
                target: target.identity.clone(),
                configuration: BuildConfiguration::Bootstrap(configuration.clone()),
                copy_context: Box::pin(futures::stream::empty()),
            })
            .await
            .unwrap();
        let mut transport = InProcessTransport::new();
        for pod in [pods[0], *target] {
            transport
                .register(pod.runtime.clone(), pod.session.id().clone())
                .await
                .unwrap();
        }
        loop {
            let item = prepared.items.next().await.unwrap().unwrap();
            let last = item.final_item;
            let message = transport.bind(Message::Copy(item)).unwrap();
            let delivery = transport.enqueue(message).unwrap();
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    let mut completed = false;
                    for event in transport.next().await.events {
                        match event {
                            TransportEvent::Copied { delivery: id, .. } if id == delivery => {
                                completed = true
                            }
                            other => panic!("unexpected bootstrap transport event: {other:?}"),
                        }
                    }
                    if completed {
                        break;
                    }
                }
            })
            .await
            .expect("durable copy acknowledgement");
            if last {
                break;
            }
        }
        builds.push((target, build_id));
    }
    for (index, pod) in pods.iter().enumerate() {
        let mut admitted = authority(pod.identity.clone(), configuration.clone());
        admitted.transition_kind = Some(TransitionKind::Bootstrap);
        pod.effect(RuntimeEffectAction::AdmitAuthority(Box::new(admitted)))
            .await
            .unwrap();
        pod.effect(RuntimeEffectAction::ChangeRole(if index == 0 {
            ReplicaRole::Primary
        } else {
            ReplicaRole::ActiveSecondary
        }))
        .await
        .unwrap();
    }
    for (target, build_id) in builds {
        pods[0]
            .effect(RuntimeEffectAction::RetireBuild(build_id.clone()))
            .await
            .unwrap();
        target
            .effect(RuntimeEffectAction::RetireBuild(build_id))
            .await
            .unwrap();
    }
    pods[0].grant().await;
    configuration
}

#[derive(Debug, Clone)]
pub struct SqlReceipt {
    pub id: i64,
    pub value: String,
    pub lsn: i64,
}

pub async fn create_data(primary: &SqlitePod) {
    primary
        .execute("CREATE TABLE data(id INTEGER PRIMARY KEY, value TEXT NOT NULL)")
        .await
        .unwrap();
}

pub async fn write_receipt(primary: &SqlitePod, id: i64) -> SqlReceipt {
    let value = format!("receipt-{id}");
    let response = tokio::time::timeout(
        Duration::from_secs(10),
        primary.execute(&format!("INSERT INTO data VALUES({id},'{value}')")),
    )
    .await
    .expect("SQL write reaches its admitted quorum")
    .unwrap();
    SqlReceipt {
        id,
        value,
        lsn: response.lsn,
    }
}

pub async fn assert_receipts(primary: &SqlitePod, receipts: &[SqlReceipt]) {
    for receipt in receipts {
        let rows = primary
            .query(&format!("SELECT value FROM data WHERE id={}", receipt.id))
            .await
            .unwrap()
            .rows;
        assert_eq!(rows.len(), 1, "acknowledged receipt {receipt:?}");
        assert_eq!(
            rows[0].values[0].kind,
            Some(proto::value::Kind::TextValue(receipt.value.clone()))
        );
        assert!(
            primary
                .application
                .persistence()
                .progress()
                .unwrap()
                .committed_lsn
                >= receipt.lsn
        );
    }
}

/// Inspect an offline reconstruction, not a public SQL read or a commit shortcut.
pub fn assert_durable_receipts(replica: &SqlitePod, receipts: &[SqlReceipt]) {
    let inspection = scratch();
    let path = inspection.path().join("applied.sqlite");
    std::fs::write(
        &path,
        replica
            .application
            .persistence()
            .applied_image_for_test()
            .unwrap(),
    )
    .unwrap();
    let database = rusqlite::Connection::open(path).unwrap();
    assert_eq!(
        database
            .query_row("PRAGMA integrity_check", [], |r| r.get::<_, String>(0))
            .unwrap(),
        "ok"
    );
    for receipt in receipts {
        assert_eq!(
            database
                .query_row("SELECT value FROM data WHERE id=?1", [receipt.id], |r| {
                    r.get::<_, String>(0)
                })
                .unwrap(),
            receipt.value
        );
        assert!(
            replica
                .application
                .persistence()
                .progress()
                .unwrap()
                .applied_lsn
                >= receipt.lsn
        );
    }
}

pub async fn assert_closed(replica: &SqlitePod) {
    assert!(
        tokio::time::timeout(
            Duration::from_secs(5),
            replica.execute("INSERT INTO data VALUES(-999,'stale-writer')")
        )
        .await
        .expect("a stale request is rejected without waiting for replication")
        .is_err()
    );
}

pub async fn close_access(replica: &SqlitePod) {
    replica
        .effect(RuntimeEffectAction::SetAccessStatus {
            read: AccessStatus::ReconfigurationPending,
            write: AccessStatus::ReconfigurationPending,
        })
        .await
        .unwrap();
}

pub async fn wait_catchup(primary: &SqlitePod) {
    tokio::time::timeout(
        Duration::from_secs(5),
        primary.effect(RuntimeEffectAction::WaitForCatchup),
    )
    .await
    .expect("validated catch-up boundary")
    .unwrap();
}

pub async fn copy_transport(source: &SqlitePod, target: &SqlitePod) -> InProcessTransport {
    let mut transport = InProcessTransport::new();
    for pod in [source, target] {
        transport
            .register(pod.runtime.clone(), pod.session.id().clone())
            .await
            .unwrap();
    }
    transport
}

pub async fn deliver_copy(
    transport: &mut InProcessTransport,
    item: kuberic_wire::proto::CopyItem,
) -> Result<kuberic_wire::proto::CopyAck, TransportError> {
    let message = transport.bind(Message::Copy(item))?;
    let id = transport.enqueue(message)?;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let mut events = transport.next().await.events.into_iter();
            if let Some(event) = events.next() {
                assert!(
                    events.next().is_none(),
                    "copy helper owns one outstanding delivery"
                );
                match event {
                    TransportEvent::Copied {
                        delivery,
                        acknowledgement,
                    } if delivery == id => return Ok(acknowledgement),
                    TransportEvent::Rejected { error, .. } => return Err(error),
                    other => panic!("unexpected copy transport event: {other:?}"),
                }
            }
        }
    })
    .await
    .expect("copy delivery settles")
}

pub async fn authorize_copy(
    source: &SqlitePod,
    target: &SqlitePod,
    id: &OperationId,
) -> BuildAuthority {
    let build = source
        .runtime
        .authorize_build(
            id.clone(),
            target.identity.clone(),
            BuildConfiguration::Current,
        )
        .await
        .unwrap();
    target
        .store
        .journal_build(&kuberic_protocol::command::EnsureReplicaBuild {
            operation_id: id.clone(),
            local_replica_id: target.identity.replica_id,
            expected_instance_id: target.identity.instance_id.clone(),
            expected_agent_generation: target.identity.agent_generation.clone(),
            target: target.identity.clone(),
            authority: Some(build.clone()),
            source_session_id: Some(source.session.id().clone()),
            retire: false,
        })
        .await
        .unwrap();
    target
        .effect(RuntimeEffectAction::AdmitBuildAuthority(Box::new(
            build.clone(),
        )))
        .await
        .unwrap();
    build
}

pub async fn prepare_copy(
    source: &SqlitePod,
    target: &SqlitePod,
    id: &OperationId,
) -> PreparedCopy {
    authorize_copy(source, target, id).await;
    source
        .runtime
        .data_plane()
        .prepare_copy(PrepareCopyRequest {
            build_id: id.clone(),
            target: target.identity.clone(),
            configuration: BuildConfiguration::Current,
            copy_context: Box::pin(futures::stream::empty()),
        })
        .await
        .unwrap()
}

pub async fn copy_items(
    copy: &mut PreparedCopy,
    through: i64,
) -> Vec<kuberic_wire::proto::CopyItem> {
    let mut items = Vec::new();
    loop {
        let item = tokio::time::timeout(Duration::from_secs(5), copy.items.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let done = (item.final_item || !item.snapshot_chunk) && item.lsn >= through;
        items.push(item);
        if done {
            break;
        }
    }
    items
}

pub async fn retire_copy(source: &SqlitePod, target: &SqlitePod, id: &OperationId) {
    for pod in [source, target] {
        pod.effect(RuntimeEffectAction::RetireBuild(id.clone()))
            .await
            .unwrap();
    }
}

pub fn scale_up_intent(
    previous: &ConfigurationDescriptor,
    candidate: &SqlitePod,
    build: &BuildAuthority,
    catch_up: i64,
) -> ScaleUpIntent {
    let mut members = previous.members.clone();
    members.push(ConfigurationMember {
        identity: candidate.identity.clone(),
        role: ReplicaRole::ActiveSecondary,
    });
    let old_policy = EffectivePolicy::fixed(previous.members.len() as u32, 30).unwrap();
    let new_policy = EffectivePolicy::fixed(members.len() as u32, 30).unwrap();
    let current = ConfigurationDescriptor::new(
        Epoch::new(
            previous.epoch.data_loss_number,
            previous.epoch.configuration_number + 1,
        ),
        previous.primary_id,
        members,
        new_policy.write_quorum,
    );
    let mut intent = ScaleUpIntent {
        operation_id: OperationId::default(),
        resource_uid: ResourceUid::new("sqlite-test"),
        spec_generation: current.epoch.configuration_number as u64,
        desired_replicas: current.members.len() as u32,
        previous_configuration: previous.clone(),
        current_configuration: current,
        previous_policy: old_policy,
        current_policy: new_policy,
        primary: build.source.clone(),
        target: candidate.identity.clone(),
        build_id: build.build_id.clone(),
        snapshot_boundary_lsn: build.replication_boundary_lsn,
        catch_up_boundary_lsn: catch_up,
    };
    intent.operation_id = intent.expected_operation_id();
    kuberic_protocol::validation::validate_scale_up(&intent).unwrap();
    intent
}

pub async fn admit_expansion(pods: &[&SqlitePod], intent: &ScaleUpIntent) {
    // The new member is activated first; the incumbent primary keeps serving
    // only through the runtime's validated same-primary scale-up path.
    for pod in pods.iter().rev() {
        let mut admitted = authority(pod.identity.clone(), intent.current_configuration.clone());
        admitted.previous_configuration = Some(intent.previous_configuration.clone());
        admitted.transition_kind = Some(TransitionKind::ScaleUp);
        admitted.scale_up = Some(Box::new(ScaleUpConfigurationEvidence::Admission {
            intent: intent.clone(),
        }));
        pod.effect(RuntimeEffectAction::AdmitAuthority(Box::new(admitted)))
            .await
            .unwrap();
        if pod.identity == intent.target {
            pod.effect(RuntimeEffectAction::ChangeRole(
                ReplicaRole::ActiveSecondary,
            ))
            .await
            .unwrap();
        }
    }
    wait_catchup(pods[0]).await;
    for pod in pods.iter().rev() {
        let mut completed = authority(pod.identity.clone(), intent.current_configuration.clone());
        completed.scale_up = Some(Box::new(ScaleUpConfigurationEvidence::Admission {
            intent: intent.clone(),
        }));
        pod.effect(RuntimeEffectAction::AdmitAuthority(Box::new(completed)))
            .await
            .unwrap();
    }
    pods[0].grant().await;
}

pub async fn copy_progress(
    pod: &SqlitePod,
    id: &OperationId,
) -> kuberic_runtime_internal::authority::DurableBuildProgress {
    pod.store.load_build_progress(id).await.unwrap().unwrap()
}

pub async fn persisted_build(pod: &SqlitePod, id: &OperationId) -> BuildAuthority {
    pod.store.load_build(id).await.unwrap().unwrap()
}

/// Complete a handoff/failover through real peer delivery, then current-only
/// convergence. `boundary` is an actually retained, acknowledged/elected suffix.
pub async fn change_primary(
    pods: &[&SqlitePod],
    previous: &ConfigurationDescriptor,
    target: &SqlitePod,
    planned: bool,
    boundary: i64,
) -> ConfigurationDescriptor {
    let source = pods
        .iter()
        .find(|p| p.identity.replica_id == previous.primary_id)
        .unwrap();
    let handoff = if planned {
        source
            .effect(RuntimeEffectAction::PrepareSwitchover {
                preparation_generation: previous.epoch.configuration_number as u64 + 1,
                request_id: SwitchoverRequestId::new(format!(
                    "handoff-{}",
                    previous.epoch.configuration_number
                )),
                source: source.identity.clone(),
                target: target.identity.clone(),
                starting_configuration_id: previous.configuration_id.clone(),
                starting_epoch: previous.epoch,
            })
            .await
            .unwrap();
        let handoff = source
            .store
            .load_state()
            .await
            .unwrap()
            .prepared_switchover
            .unwrap();
        assert_eq!(handoff.handoff_lsn, boundary);
        Some(handoff)
    } else {
        source.runtime.abort();
        None
    };
    let members = previous
        .members
        .iter()
        .map(|m| m.identity.clone())
        .collect::<Vec<_>>();
    let index = members
        .iter()
        .position(|id| *id == target.identity)
        .unwrap();
    let current = configuration(&members, index, previous.epoch.configuration_number + 1);
    let survivors: Vec<_> = pods
        .iter()
        .copied()
        .filter(|pod| planned || pod.identity != source.identity)
        .collect();
    for pod in &survivors {
        let mut admitted = authority(pod.identity.clone(), current.clone());
        admitted.previous_configuration = Some(previous.clone());
        admitted.transition_kind = Some(if planned {
            TransitionKind::PlannedSwitchover
        } else {
            TransitionKind::Failover
        });
        admitted.switchover_handoff = handoff.clone();
        pod.effect(RuntimeEffectAction::AdmitAuthority(Box::new(admitted)))
            .await
            .unwrap();
        if !planned {
            pod.effect(RuntimeEffectAction::AuthorizeFailoverPrefix(boundary))
                .await
                .unwrap();
        }
        pod.effect(RuntimeEffectAction::ChangeRole(
            if pod.identity == target.identity {
                ReplicaRole::Primary
            } else {
                ReplicaRole::ActiveSecondary
            },
        ))
        .await
        .unwrap();
    }
    let peers = survivors
        .iter()
        .copied()
        .filter(|p| p.identity != target.identity)
        .collect::<Vec<_>>();
    let routes = route(target, &peers).await;
    for peer in &peers {
        target
            .runtime
            .repair_peer(peer.identity.clone(), boundary.saturating_sub(1))
            .await
            .unwrap();
    }
    wait_catchup(target).await;
    for pod in &survivors {
        let mut completed = authority(pod.identity.clone(), current.clone());
        completed.switchover_handoff = handoff.clone();
        pod.effect(RuntimeEffectAction::AdmitAuthority(Box::new(completed)))
            .await
            .unwrap();
    }
    target.grant().await;
    routes.stop().await;
    current
}

pub async fn removal_intent(
    previous: &ConfigurationDescriptor,
    removed: &SqlitePod,
) -> SecondaryScaleDownIntent {
    assert_ne!(previous.primary_id, removed.identity.replica_id);
    let policy = EffectivePolicy::fixed(previous.members.len() as u32 - 1, 30).unwrap();
    let current = ConfigurationDescriptor::new(
        Epoch::new(
            previous.epoch.data_loss_number,
            previous.epoch.configuration_number + 1,
        ),
        previous.primary_id,
        previous
            .members
            .iter()
            .filter(|m| m.identity != removed.identity)
            .cloned()
            .collect(),
        policy.write_quorum,
    );
    let storage = removed.store.load_state().await.unwrap().identity;
    let resource = storage.resource_uid;
    let mut intent = SecondaryScaleDownIntent {
        operation_id: OperationId::default(),
        resource_uid: resource.clone(),
        spec_generation: current.epoch.configuration_number as u64,
        desired_replicas: current.members.len() as u32,
        previous_configuration: previous.clone(),
        current_configuration: current,
        previous_policy: EffectivePolicy::fixed(previous.members.len() as u32, 30).unwrap(),
        current_policy: policy,
        primary: previous
            .members
            .iter()
            .find(|m| m.role == ReplicaRole::Primary)
            .unwrap()
            .identity
            .clone(),
        target: removed.identity.clone(),
        cleanup: ReplicaCleanupIdentity {
            pod: CleanupResourceIdentity::Present {
                name: format!("pod-{}", removed.identity.replica_id),
                uid: storage.pod_uid.to_string(),
            },
            pvc: CleanupResourceIdentity::Present {
                name: format!("pvc-{}", removed.identity.replica_id),
                uid: storage.pvc_uid.to_string(),
            },
            endpoint: CleanupResourceIdentity::Present {
                name: derive_replica_endpoint_name(&resource, &removed.identity),
                uid: format!("endpoint-{}", removed.identity.instance_id),
            },
        },
    };
    intent.operation_id = intent.expected_operation_id();
    kuberic_protocol::validation::validate_secondary_scale_down(&intent).unwrap();
    intent
}

async fn removal_witness(pod: &SqlitePod) -> SecondaryRemovalWitness {
    let snapshot = pod.runtime.snapshot().await;
    let state = pod.store.load_state().await.unwrap();
    let admitted = snapshot.authority.unwrap();
    SecondaryRemovalWitness {
        resource_uid: state.identity.resource_uid,
        identity: pod.identity.clone(),
        role: snapshot.role,
        process_session_id: pod.session.id().clone(),
        report_sequence: pod.session.next_report_sequence(),
        epoch: admitted.current_configuration.epoch,
        previous_configuration_id: admitted.previous_configuration.map(|c| c.configuration_id),
        current_configuration_id: admitted.current_configuration.configuration_id,
        verified_replication_lsn: snapshot
            .verified_replication_lsn
            .expect("authority-verified SQL prefix"),
        write_status: snapshot.write_status,
        pending_operation_id: state.pending_effect.map(|p| p.effect.operation_id),
        retained_operation_id: state.retained_result.map(|r| r.effect.operation_id),
    }
}

async fn observe_removal(pods: &[&SqlitePod], witnesses: &[SecondaryRemovalWitness]) {
    for pod in pods {
        for witness in witnesses.iter().filter(|w| w.identity != pod.identity) {
            pod.effect(RuntimeEffectAction::RegisterPeerSession {
                identity: witness.identity.clone(),
                session: witness.process_session_id.clone(),
            })
            .await
            .unwrap();
            pod.effect(RuntimeEffectAction::ObserveSecondaryRemovalWitness(
                Box::new(witness.clone()),
            ))
            .await
            .unwrap();
        }
    }
}

/// All evidence is sampled from actual runtime/store postconditions and sessions.
/// This fixture supplies controller decisions, never synthetic application ACKs.
pub async fn scale_down(
    pods: &[&SqlitePod],
    removed: &SqlitePod,
    previous: &ConfigurationDescriptor,
) -> SecondaryScaleDownCleanup {
    let intent = removal_intent(previous, removed).await;
    let primary = pods.iter().find(|p| p.identity == intent.primary).unwrap();
    primary
        .effect_as(
            intent.command_operation_id(SecondaryRemovalStage::Prepare, &primary.identity),
            RuntimeEffectAction::PrepareSecondaryRemoval {
                intent: Box::new(intent.clone()),
                process_session_id: primary.session.id().clone(),
                report_sequence: primary.session.next_report_sequence(),
            },
        )
        .await
        .unwrap();
    let preparation = primary
        .store
        .load_state()
        .await
        .unwrap()
        .prepared_secondary_removal
        .unwrap();
    let mut old = Vec::new();
    for pod in pods {
        old.push(removal_witness(pod).await);
    }
    let mut evidence = SecondaryRemovalEvidence {
        preparation,
        previous_read_quorum: old,
        reduced_write_quorum: Vec::new(),
    };
    kuberic_protocol::validation::validate_secondary_removal_evidence(&evidence, false).unwrap();
    for pod in pods {
        let mut admitted = authority(pod.identity.clone(), intent.current_configuration.clone());
        admitted.previous_configuration = Some(previous.clone());
        admitted.transition_kind = Some(TransitionKind::SecondaryScaleDown);
        admitted.secondary_removal = Some(evidence.clone());
        pod.effect_as(
            intent.command_operation_id(SecondaryRemovalStage::PreviousCurrent, &pod.identity),
            RuntimeEffectAction::AdmitAuthority(Box::new(admitted)),
        )
        .await
        .unwrap();
    }
    for pod in pods {
        evidence
            .reduced_write_quorum
            .push(removal_witness(pod).await);
    }
    observe_removal(pods, &evidence.reduced_write_quorum).await;
    wait_catchup(primary).await;
    for pod in pods {
        let mut admitted = authority(pod.identity.clone(), intent.current_configuration.clone());
        admitted.secondary_removal = Some(evidence.clone());
        pod.effect_as(
            intent.command_operation_id(SecondaryRemovalStage::CurrentOnly, &pod.identity),
            RuntimeEffectAction::AdmitAuthority(Box::new(admitted)),
        )
        .await
        .unwrap();
    }
    let mut current = Vec::new();
    for pod in pods {
        current.push(removal_witness(pod).await);
    }
    observe_removal(pods, &current).await;
    let committed = SecondaryScaleDownCleanup {
        evidence,
        current_only_write_quorum: current,
        retirement: None,
    };
    kuberic_protocol::validation::validate_secondary_scale_down_cleanup(&committed).unwrap();
    for pod in pods {
        pod.effect_as(
            intent.command_operation_id(SecondaryRemovalStage::AcceptCommit, &pod.identity),
            RuntimeEffectAction::AcceptSecondaryRemovalCommit(Box::new(committed.clone())),
        )
        .await
        .unwrap();
    }
    let report = ReplicaRetirementReport {
        intent: intent.clone(),
        operation_id: intent.command_operation_id(SecondaryRemovalStage::Retire, &removed.identity),
        process_session_id: removed.session.id().clone(),
        report_sequence: removed.session.next_report_sequence(),
        epoch: intent.current_configuration.epoch,
        role: ReplicaRole::None,
        read_status: AccessStatus::NotPrimary,
        write_status: AccessStatus::NotPrimary,
        application_closed: true,
        peers_fenced: true,
    };
    removed
        .effect_as(
            report.operation_id.clone(),
            RuntimeEffectAction::RetireReplica(Box::new(
                kuberic_runtime_internal::authority::RetiredAuthority {
                    committed: committed.clone(),
                    report,
                },
            )),
        )
        .await
        .unwrap();
    primary.grant().await;
    committed
}
