#![cfg(feature = "testing")]

//! Public factory/host/service journeys over real authenticated loopback sockets.
//! Metadata setup supplies controller authority, never an injected native peer.

use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use external_classic_v1_provider::ExternalService;
use kuberic_runtime::StatefulServiceReplica;
use kuberic_runtime::application::{OpenContext, OperationDataStream, RoleChange};
use kuberic_runtime::control::{convert, proto};
use kuberic_runtime::host::{
    ApplicationStorageState, ReplicaEndpointResolver, ReplicaHost, ReplicaProcessConfig,
    RunningReplica,
};
use kuberic_runtime::protocol::command::{
    PublicV1HistoryCommand, PublicV1OperationCommand, PublicV1SessionRenewalCommand,
};
use kuberic_runtime::protocol::public_operations::*;
use kuberic_runtime::protocol::types::*;
#[allow(
    clippy::disallowed_types,
    reason = "test-only decoration of the same native creation's protected callbacks"
)]
use kuberic_runtime::replicator::{
    DefaultReplicatorFactory, PrimaryReplicator, ReplicaInformation, ReplicaSetConfiguration,
    ReplicaSetQuorumMode, Replicator, ReplicatorFactory, ReplicatorFactoryContext,
    ReplicatorInterfaces, ReplicatorSettings,
};
use kuberic_runtime::{Result, RuntimeError, StateProvider};
use kvstore2::persistence::KvPersistence;
use kvstore2::service::KvService;
use kvstore2::state::CopyGate;
use sqlite_replicated::service::SqliteService;

const CREDENTIAL: &str = "classic-v1-test-credential";

mod native_wire {
    tonic::include_proto!("kuberic.sf.v1");
}

fn authorized<T>(message: T) -> tonic::Request<T> {
    let mut request = tonic::Request::new(message);
    request.metadata_mut().insert(
        "authorization",
        format!("Bearer {CREDENTIAL}").parse().unwrap(),
    );
    request
}

fn identity_for(resource: &ResourceUid, id: i64) -> ReplicaIdentity {
    let initialization = derive_initialization_id(
        resource,
        ReplicaId::new(id),
        &PodUid::new(format!("pod-{id}")),
        &PvcUid::new(format!("pvc-{id}")),
    );
    ReplicaIdentity {
        replica_id: ReplicaId::new(id),
        instance_id: ReplicaInstanceId::new(format!("pod-{id}")),
        agent_generation: derive_agent_generation(&initialization),
    }
}

async fn report_at(
    control: &str,
    resource: &ResourceUid,
    identity: &ReplicaIdentity,
) -> PublicV1AgentReport {
    let mut client = proto::agent_control_client::AgentControlClient::connect(control.to_owned())
        .await
        .unwrap();
    let response = client
        .get_status(authorized(proto::GetAgentStatusRequest {
            protocol_version: kuberic_runtime::protocol::PROTOCOL_VERSION,
            resource_uid: resource.to_string(),
            replica_id: identity.replica_id.value(),
            expected_instance_id: identity.instance_id.to_string(),
        }))
        .await
        .unwrap()
        .into_inner();
    convert::normalize_public_v1_agent_status_report(response).unwrap()
}

#[async_trait::async_trait]
trait Sample: StatefulServiceReplica + Sized + 'static {
    fn fresh(path: &Path) -> bool;
    fn create(path: &Path) -> Arc<Self>;
    async fn drain(&self) -> kuberic_runtime::Result<()>;
}

#[async_trait::async_trait]
impl Sample for KvService {
    fn fresh(path: &Path) -> bool {
        KvPersistence::is_fresh_empty(path).unwrap()
    }
    fn create(path: &Path) -> Arc<Self> {
        Arc::new(KvService::new(
            Arc::new(KvPersistence::open(path).unwrap()),
            "http://127.0.0.1:0".into(),
            CopyGate::disabled(),
        ))
    }
    async fn drain(&self) -> kuberic_runtime::Result<()> {
        self.shutdown_and_drain().await
    }
}

struct RoleAddressKvService {
    inner: Arc<KvService>,
    address: String,
    primary_calls: AtomicUsize,
    block_primary: AtomicBool,
    fail_primary: AtomicBool,
    primary_entered: tokio::sync::Notify,
    primary_release: tokio::sync::Notify,
}

#[async_trait::async_trait]
impl StatefulServiceReplica for RoleAddressKvService {
    async fn open(
        self: Arc<Self>,
        context: kuberic_runtime::application::OpenContext,
    ) -> kuberic_runtime::Result<Arc<dyn kuberic_runtime::Replicator>> {
        self.inner.clone().open(context).await
    }

    async fn change_role(
        &self,
        role: ReplicaRole,
    ) -> kuberic_runtime::Result<kuberic_runtime::application::RoleChange> {
        if role == ReplicaRole::Primary {
            self.primary_calls.fetch_add(1, Ordering::SeqCst);
            self.primary_entered.notify_one();
            if self.block_primary.load(Ordering::SeqCst) {
                self.primary_release.notified().await;
            }
            if self.fail_primary.load(Ordering::SeqCst) {
                return Err(kuberic_runtime::RuntimeError::Application(
                    "replacement application role rejected".into(),
                ));
            }
        }
        let mut changed = self.inner.change_role(role).await?;
        changed.service_address = (role == ReplicaRole::Primary).then(|| self.address.clone());
        Ok(changed)
    }

    async fn close(&self) -> kuberic_runtime::Result<()> {
        self.inner.close().await
    }

    fn abort(&self) {
        self.inner.abort();
    }
}

#[async_trait::async_trait]
impl Sample for RoleAddressKvService {
    fn fresh(path: &Path) -> bool {
        KvService::fresh(path)
    }

    fn create(path: &Path) -> Arc<Self> {
        static CREATIONS: AtomicUsize = AtomicUsize::new(0);
        Arc::new(Self {
            inner: KvService::create(path),
            address: format!(
                "http://kv-{}.example.test:8080",
                CREATIONS.fetch_add(1, Ordering::SeqCst)
            ),
            primary_calls: AtomicUsize::new(0),
            block_primary: AtomicBool::new(false),
            fail_primary: AtomicBool::new(false),
            primary_entered: tokio::sync::Notify::new(),
            primary_release: tokio::sync::Notify::new(),
        })
    }

    async fn drain(&self) -> kuberic_runtime::Result<()> {
        self.inner.drain().await
    }
}

#[async_trait::async_trait]
impl Sample for SqliteService {
    fn fresh(path: &Path) -> bool {
        sqlite_replicated::SqlitePersistence::is_fresh_empty(path).unwrap()
    }
    fn create(path: &Path) -> Arc<Self> {
        Arc::new(SqliteService::deferred(path.to_path_buf(), "http://127.0.0.1:0".into()).unwrap())
    }

    async fn drain(&self) -> kuberic_runtime::Result<()> {
        self.shutdown_and_drain().await
    }
}

#[async_trait::async_trait]
impl Sample for ExternalService {
    fn fresh(path: &Path) -> bool {
        ExternalService::is_fresh(path).unwrap()
    }
    fn create(path: &Path) -> Arc<Self> {
        Arc::new(ExternalService::new(
            path.to_path_buf(),
            "http://127.0.0.1:0".into(),
        ))
    }
    async fn drain(&self) -> kuberic_runtime::Result<()> {
        self.shutdown_and_drain().await
    }
}

#[derive(Clone, Copy)]
enum QueryScript {
    Native,
    Value(i64),
    NotReady,
    Failed(&'static str),
}

impl QueryScript {
    fn result(self) -> Option<Result<i64>> {
        match self {
            Self::Native => None,
            Self::Value(value) => Some(Ok(value)),
            Self::NotReady => Some(Err(RuntimeError::ProgressNotReady)),
            Self::Failed(detail) => Some(Err(RuntimeError::Application(detail.into()))),
        }
    }
}

struct QueryScripts {
    outcomes: Mutex<[QueryScript; 2]>,
    calls: [AtomicUsize; 2],
}

#[allow(
    clippy::disallowed_types,
    reason = "transparent native query decorator"
)]
struct QueryReplicator {
    inner: Arc<dyn PrimaryReplicator>,
    scripts: Arc<QueryScripts>,
}

#[async_trait::async_trait]
impl Replicator for QueryReplicator {
    async fn open(&self) -> Result<String> {
        self.inner.open().await
    }
    async fn change_role(&self, epoch: Epoch, role: ReplicaRole) -> Result<()> {
        self.inner.change_role(epoch, role).await
    }
    async fn update_epoch(&self, epoch: Epoch) -> Result<()> {
        self.inner.update_epoch(epoch).await
    }
    async fn close(&self) -> Result<()> {
        self.inner.close().await
    }
    fn abort(&self) {
        self.inner.abort();
    }
    async fn current_progress(&self) -> Result<i64> {
        self.scripts.calls[0].fetch_add(1, Ordering::SeqCst);
        let scripted = self.scripts.outcomes.lock().unwrap()[0].result();
        match scripted {
            Some(result) => result,
            None => self.inner.current_progress().await,
        }
    }
    async fn catch_up_capability(&self) -> Result<i64> {
        self.scripts.calls[1].fetch_add(1, Ordering::SeqCst);
        let scripted = self.scripts.outcomes.lock().unwrap()[1].result();
        match scripted {
            Some(result) => result,
            None => self.inner.catch_up_capability().await,
        }
    }
}

#[allow(
    clippy::disallowed_types,
    reason = "transparent native query decorator"
)]
#[async_trait::async_trait]
impl PrimaryReplicator for QueryReplicator {
    async fn on_data_loss(&self) -> Result<bool> {
        self.inner.on_data_loss().await
    }
    async fn update_catch_up_replica_set_configuration(
        &self,
        current: ReplicaSetConfiguration,
        previous: ReplicaSetConfiguration,
    ) -> Result<()> {
        self.inner
            .update_catch_up_replica_set_configuration(current, previous)
            .await
    }
    async fn wait_for_catch_up_quorum(&self, mode: ReplicaSetQuorumMode) -> Result<()> {
        self.inner.wait_for_catch_up_quorum(mode).await
    }
    async fn update_current_replica_set_configuration(
        &self,
        current: ReplicaSetConfiguration,
    ) -> Result<()> {
        self.inner
            .update_current_replica_set_configuration(current)
            .await
    }
    async fn build_replica(&self, replica: ReplicaInformation) -> Result<()> {
        self.inner.build_replica(replica).await
    }
    async fn remove_replica(&self, replica: ReplicaId) -> Result<()> {
        self.inner.remove_replica(replica).await
    }
}

struct QueryFactory(Arc<QueryScripts>);

#[async_trait::async_trait]
impl ReplicatorFactory for QueryFactory {
    async fn create_replicator(
        &self,
        context: ReplicatorFactoryContext,
        provider: Option<Arc<dyn StateProvider>>,
        settings: ReplicatorSettings,
    ) -> Result<ReplicatorInterfaces> {
        let native = DefaultReplicatorFactory::new()
            .create_replicator(context, provider, settings)
            .await?;
        let state = native.state_replicator();
        let inner = native.primary_replicator().unwrap();
        Ok(native.wrap_primary(
            Arc::new(QueryReplicator {
                inner,
                scripts: self.0.clone(),
            }),
            state,
        ))
    }
}

// Observation-only service reuses the real KV store/committer and the Node host
// fixture. Scripted query values are not writes, ACKs, or durability receipts.
struct QueryService {
    committer: Arc<kvstore2::v1_test_bridge::Committer>,
    scripts: Arc<QueryScripts>,
    control: OnceLock<Arc<dyn Replicator>>,
    settings: OnceLock<Arc<dyn kuberic_runtime::replicator::StateReplicator>>,
}

#[async_trait::async_trait]
impl StateProvider for QueryService {
    async fn update_epoch(&self, _: Epoch, previous_epoch_last_lsn: i64) -> Result<()> {
        assert!(previous_epoch_last_lsn >= 0);
        self.last_committed_lsn().await.map(|_| ())
    }
    async fn last_committed_lsn(&self) -> Result<i64> {
        self.committer
            .progress()
            .map_err(|error| RuntimeError::Application(error.to_string()))
    }
    async fn get_copy_context(&self) -> Result<OperationDataStream> {
        Ok(Box::pin(futures::stream::empty()))
    }
    async fn get_copy_state(
        &self,
        up_to: i64,
        _: OperationDataStream,
    ) -> Result<OperationDataStream> {
        let snapshot = self
            .committer
            .snapshot(up_to)
            .await
            .map_err(|error| RuntimeError::Application(error.to_string()))?;
        Ok(Box::pin(futures::stream::iter([Ok(snapshot)])))
    }
    async fn on_data_loss(&self) -> Result<bool> {
        Ok(false)
    }
}

#[async_trait::async_trait]
impl StatefulServiceReplica for QueryService {
    async fn open(self: Arc<Self>, context: OpenContext) -> Result<Arc<dyn Replicator>> {
        let interfaces = context
            .partition
            .with_factory(Arc::new(QueryFactory(self.scripts.clone())))
            .create_replicator(
                Some(self.clone()),
                Some(ReplicatorSettings {
                    replication_address: "http://127.0.0.1:0".into(),
                }),
            )
            .await?;
        let control = interfaces.replicator();
        assert!(
            self.settings
                .set(interfaces.state_replicator().unwrap())
                .is_ok()
        );
        assert!(self.control.set(control.clone()).is_ok());
        Ok(control)
    }
    async fn change_role(&self, _: ReplicaRole) -> Result<RoleChange> {
        Ok(RoleChange {
            service_address: None,
        })
    }
    async fn close(&self) -> Result<()> {
        self.drain().await
    }
    fn abort(&self) {
        self.committer.abort();
    }
}

#[async_trait::async_trait]
impl Sample for QueryService {
    fn fresh(path: &Path) -> bool {
        KvService::fresh(path)
    }
    fn create(path: &Path) -> Arc<Self> {
        let committer =
            Arc::new(kvstore2::v1_test_bridge::Committer::open(path, Arc::new(|_| {})).unwrap());
        Arc::new(Self {
            committer,
            scripts: Arc::new(QueryScripts {
                outcomes: Mutex::new([QueryScript::Native; 2]),
                calls: [AtomicUsize::new(0), AtomicUsize::new(0)],
            }),
            control: OnceLock::new(),
            settings: OnceLock::new(),
        })
    }
    async fn drain(&self) -> Result<()> {
        self.committer
            .close()
            .await
            .map_err(|error| RuntimeError::Application(error.to_string()))
    }
}

#[tokio::test]
async fn public_factory_port_zero_effective_settings_are_immutable_and_round_trip() {
    tokio::time::timeout(Duration::from_secs(30), async {
        let root = tempfile::tempdir().unwrap();
        let epoch = Epoch::new(0, 1);
        let mut node = Node::<QueryService>::start_sized(root.path(), 1, 0, 1).await;
        node.open(ReplicaRole::Primary, epoch).await;
        node.single(epoch).await;
        let before = node.report().await;
        let address = before.replication_address.clone().unwrap();
        assert!(!address.ends_with(":0"));
        let state = node.service.settings.get().unwrap();
        state
            .update_replicator_settings(ReplicatorSettings {
                replication_address: address.clone(),
            })
            .await
            .expect("unchanged effective endpoint rejected by real factory registration");
        assert!(
            state
                .update_replicator_settings(ReplicatorSettings {
                    replication_address: "http://127.0.0.1:1".into(),
                })
                .await
                .is_err()
        );
        assert!(
            state
                .update_replicator_settings(ReplicatorSettings {
                    replication_address: "http://127.0.0.1:0".into(),
                })
                .await
                .is_err(),
            "unresolved input is not an effective endpoint alias"
        );
        let after = node.report().await;
        assert_eq!(before.replication_address, after.replication_address);
        assert_eq!(before.process_session_id, after.process_session_id);
        assert_eq!(before.replicator_creation_id, after.replicator_creation_id);
        assert_eq!(before.role, after.role);
        let _socket = tokio::net::TcpStream::connect(
            address
                .trim_end_matches('/')
                .strip_prefix("http://")
                .unwrap(),
        )
        .await
        .unwrap();
        drop(_socket);
        node.close().await;
    })
    .await
    .expect("effective settings journey exceeded its bound");
}

struct Resolver;
impl ReplicaEndpointResolver for Resolver {
    fn control_endpoint(&self, _: &ReplicaIdentity) -> String {
        "http://127.0.0.1:1".into()
    }
    fn replication_endpoint(&self, _: &ReplicaIdentity) -> String {
        "http://127.0.0.1:1".into()
    }
}

struct Node<A: Sample = KvService> {
    running: RunningReplica,
    service: Arc<A>,
    identity: ReplicaIdentity,
    storage: ReplicaStorageIncarnationContext,
    history: HistoryContext,
    preview: PublicV1Identity,
    revision: u64,
    control_address: String,
    session: ProcessSessionId,
}

impl<A: Sample> Node<A> {
    async fn start(root: &Path, id: i64, revision: u64) -> Self {
        Self::start_sized(root, id, revision, 2).await
    }

    async fn start_sized(root: &Path, id: i64, revision: u64, replicas: u32) -> Self {
        let resource = ResourceUid::new("classic-v1-public-journey");
        let preview = PublicV1Identity::new(1);
        let identity = identity_for(&resource, id);
        let application = root.join("application");
        let fresh = A::fresh(&application);
        let service = A::create(&application);
        let host = ReplicaHost::new(
            ReplicaProcessConfig {
                resource_uid: resource.clone(),
                replica_id: identity.replica_id,
                pod_uid: PodUid::new(format!("pod-{id}")),
                pvc_uid: PvcUid::new(format!("pvc-{id}")),
                data_root: root.to_path_buf(),
                replication_address: "127.0.0.1:0".parse().unwrap(),
                bearer_token: CREDENTIAL.into(),
                rpc_deadline: Duration::from_secs(5),
                transport_window_capacity: 64,
            },
            service.clone(),
            if fresh {
                ApplicationStorageState::FreshEmpty
            } else {
                ApplicationStorageState::Established
            },
            Arc::new(Resolver),
        )
        .with_application_storage_paths(std::collections::BTreeMap::from([(
            "application".into(),
            application,
        )]));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let control_address = format!("http://{}", listener.local_addr().unwrap());
        let startup = tokio::spawn(host.start(listener));
        let mut client = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                match proto::agent_control_client::AgentControlClient::connect(
                    control_address.clone(),
                )
                .await
                {
                    Ok(client) => break client,
                    Err(_) => tokio::time::sleep(Duration::from_millis(10)).await,
                }
            }
        })
        .await
        .expect("public control listener starts");
        let status = client
            .get_status(authorized(proto::GetAgentStatusRequest {
                protocol_version: kuberic_runtime::protocol::PROTOCOL_VERSION,
                resource_uid: resource.to_string(),
                replica_id: id,
                expected_instance_id: identity.instance_id.to_string(),
            }))
            .await
            .unwrap()
            .into_inner();
        if status.storage_state == proto::AgentStorageState::Uninitialized as i32 {
            let initialization = derive_initialization_id(
                &resource,
                identity.replica_id,
                &PodUid::new(format!("pod-{id}")),
                &PvcUid::new(format!("pvc-{id}")),
            );
            let bootstrap = ConfigurationDescriptor::new(
                Epoch::new(0, 1),
                ReplicaId::new(1),
                (1..=i64::from(replicas))
                    .map(|member| ConfigurationMember {
                        identity: identity_for(&resource, member),
                        role: if member == 1 {
                            ReplicaRole::Primary
                        } else {
                            ReplicaRole::ActiveSecondary
                        },
                    })
                    .collect(),
                replicas / 2 + 1,
            );
            client
                .execute(authorized(proto::ExecuteCommandRequest {
                    protocol_version: kuberic_runtime::protocol::PROTOCOL_VERSION,
                    resource_uid: resource.to_string(),
                    target: Some(identity.clone().into()),
                    expected_process_session_id: status.process_session_id,
                    command: Some(
                        proto::execute_command_request::Command::InitializeAgentStore(
                            proto::InitializeAgentStoreCommand {
                                initialization_id: initialization.to_string(),
                                resource_uid: resource.to_string(),
                                local_replica_id: id,
                                expected_instance_id: identity.instance_id.to_string(),
                                expected_pod_uid: format!("pod-{id}"),
                                expected_pvc_uid: format!("pvc-{id}"),
                                assigned_agent_generation: identity.agent_generation.to_string(),
                                effective_policy: Some(
                                    EffectivePolicy::fixed(replicas, 1).unwrap().into(),
                                ),
                                bootstrap_configuration: Some(bootstrap.into()),
                                provisioning: None,
                            },
                        ),
                    ),
                }))
                .await
                .unwrap();
        }
        drop(client);
        let running = tokio::time::timeout(Duration::from_secs(10), startup)
            .await
            .expect("initialized public host starts")
            .unwrap()
            .unwrap();
        let report = report_at(&control_address, &resource, &identity).await;
        assert_eq!(report.replica, identity);
        Self {
            running,
            service,
            identity,
            storage: report.storage_incarnation,
            history: report.history,
            preview,
            revision,
            control_address,
            session: report.process_session_id,
        }
    }

    async fn report(&self) -> PublicV1AgentReport {
        report_at(
            &self.control_address,
            &self.history.resource_uid,
            &self.identity,
        )
        .await
    }

    async fn command(&mut self, program: PublicOperationProgram) {
        if !matches!(
            program,
            PublicOperationProgram::Build(_) | PublicOperationProgram::Remove(_)
        ) {
            self.revision += 1;
        }
        let id = match &program {
            PublicOperationProgram::Build(build) | PublicOperationProgram::Remove(build) => {
                build.attempt.clone()
            }
            _ => OperationId::new(format!(
                "{}-{}-{}",
                self.identity.replica_id, self.session, self.revision
            )),
        };
        let class = match &program {
            PublicOperationProgram::Build(build) => PublicOperationClass::Build {
                target: build.replica.replica_id,
            },
            PublicOperationProgram::Remove(build) => PublicOperationClass::Remove {
                target: build.replica.replica_id,
            },
            PublicOperationProgram::Close => PublicOperationClass::Close,
            PublicOperationProgram::Abort => PublicOperationClass::Abort,
            _ => PublicOperationClass::Authority,
        };
        let revision = self.revision;
        let intent = PublicOperationIntent {
            identity: self.preview.clone(),
            operation_id: id,
            revision,
            process_session_id: self.session.clone(),
            class,
            input_digest: program.digest(),
            replay_disposition: program.replay_disposition(),
            lifecycle: None,
            program: Some(program),
        };
        let operation = PublicV1OperationCommand {
            target: self.identity.clone(),
            intent,
        };
        self.submit(operation).await;
    }

    async fn submit(&mut self, operation: PublicV1OperationCommand) {
        let label = format!("{:?}", operation.intent);
        self.submit_result(operation)
            .await
            .unwrap_or_else(|error| panic!("{label}: {error}"));
    }

    async fn submit_result(
        &mut self,
        operation: PublicV1OperationCommand,
    ) -> std::result::Result<(), tonic::Status> {
        self.revision = self.revision.max(operation.intent.revision);
        let mut client =
            proto::agent_control_client::AgentControlClient::connect(self.control_address.clone())
                .await
                .unwrap();
        tokio::time::timeout(
            Duration::from_secs(20),
            client.execute(authorized(proto::ExecuteCommandRequest {
                protocol_version: kuberic_runtime::protocol::PROTOCOL_VERSION,
                resource_uid: self.history.resource_uid.to_string(),
                target: Some(self.identity.clone().into()),
                expected_process_session_id: self.session.to_string(),
                command: Some(proto::execute_command_request::Command::PublicV1Operation(
                    convert::public_v1_operation_command_to_proto(&operation).unwrap(),
                )),
            })),
        )
        .await
        .expect("production host operation completes")
        .map(|_| ())
    }

    async fn open(&mut self, role: ReplicaRole, epoch: Epoch) {
        let existing = self.report().await.open_mode == OpenMode::Existing;
        self.command(PublicOperationProgram::Open {
            replica: self.identity.clone(),
            existing,
        })
        .await;
        self.command(PublicOperationProgram::Role { epoch, role })
            .await;
        if role == ReplicaRole::Primary {
            self.command(PublicOperationProgram::Epoch { epoch }).await;
        }
    }

    async fn descriptor(&self, role: ReplicaRole) -> PublicReplicaDescriptor {
        let report = self.report().await;
        PublicReplicaDescriptor {
            build_id: OperationId::default(),
            identity: self.identity.clone(),
            replication_address: report.replication_address.unwrap(),
            process_session_id: report.process_session_id,
            storage_incarnation: Some(self.storage.clone()),
            role,
            current_progress: report.progress.current().expect("native query succeeded"),
            catch_up_capability: report
                .progress
                .capability()
                .expect("native query succeeded"),
            must_catchup: false,
        }
    }

    async fn single(&mut self, epoch: Epoch) -> ConfigurationDescriptor {
        let current = ConfigurationDescriptor::new(
            epoch,
            self.identity.replica_id,
            vec![ConfigurationMember {
                identity: self.identity.clone(),
                role: ReplicaRole::Primary,
            }],
            1,
        );
        self.command(PublicOperationProgram::Configuration(PublicConfiguration {
            current: current.clone(),
            previous: None,
            replicas: Vec::new(),
        }))
        .await;
        current
    }

    async fn build(
        &mut self,
        target: &mut Self,
        epoch: Epoch,
        current: &ConfigurationDescriptor,
    ) -> PublicInboundBuildAuthority {
        let target_report = target.report().await;
        let attempt = OperationId::new(format!(
            "build-{}-{}",
            target.identity.replica_id, target_report.process_session_id
        ));
        let address = target_report.replication_address.unwrap();
        let source_address = self.report().await.replication_address.unwrap();
        let source_session = self.session.clone();
        let build = PublicBuildInput {
            attempt: attempt.clone(),
            epoch,
            current_configuration_id: Some(current.configuration_id.clone()),
            replica: target.identity.clone(),
            target_storage: target.storage.clone(),
            process_session_id: target_report.process_session_id.clone(),
            source_address: source_address.clone(),
            replication_address: address.clone(),
            history: Some(self.history.clone()),
        };
        let authority = PublicInboundBuildAuthority {
            operation_id: attempt.clone(),
            operation_revision: self.revision,
            attempt: PublicBuildAttemptReference {
                resource_uid: self.history.resource_uid.clone(),
                epoch,
                build_id: attempt,
                source: self.identity.clone(),
                source_storage: self.storage.clone(),
                source_session,
                target: target.identity.clone(),
                target_storage: target.storage.clone(),
                target_session: target_report.process_session_id,
            },
            history: self.history.clone(),
            current_configuration_id: current.configuration_id.clone(),
            source_address,
            target_address: address,
        };
        target.install_build_authority(authority.clone()).await;
        self.command(PublicOperationProgram::Build(build)).await;
        target
            .command(PublicOperationProgram::Role {
                epoch,
                role: ReplicaRole::ActiveSecondary,
            })
            .await;
        target
            .command(PublicOperationProgram::Epoch { epoch })
            .await;
        let full = ConfigurationDescriptor::new(
            epoch,
            self.identity.replica_id,
            vec![
                ConfigurationMember {
                    identity: self.identity.clone(),
                    role: ReplicaRole::Primary,
                },
                ConfigurationMember {
                    identity: target.identity.clone(),
                    role: ReplicaRole::ActiveSecondary,
                },
            ],
            2,
        );
        target
            .command(PublicOperationProgram::Configuration(PublicConfiguration {
                current: full.clone(),
                previous: None,
                replicas: vec![self.descriptor(ReplicaRole::Primary).await],
            }))
            .await;
        self.command(PublicOperationProgram::Configuration(PublicConfiguration {
            current: full,
            previous: None,
            replicas: vec![target.descriptor(ReplicaRole::ActiveSecondary).await],
        }))
        .await;
        authority
    }

    async fn install_build_authority(&self, authority: PublicInboundBuildAuthority) {
        let mut client =
            proto::replica_peer_client::ReplicaPeerClient::connect(self.control_address.clone())
                .await
                .unwrap();
        client
            .execute(authorized(proto::ExecuteCommandRequest {
                protocol_version: kuberic_runtime::protocol::PROTOCOL_VERSION,
                resource_uid: self.history.resource_uid.to_string(),
                target: Some(self.identity.clone().into()),
                expected_process_session_id: self.session.to_string(),
                command: Some(proto::execute_command_request::Command::PublicV1History(
                    convert::public_v1_history_command_to_proto(
                        &PublicV1HistoryCommand::InstallInboundBuildAuthority {
                            authority: Box::new(authority),
                        },
                    )
                    .unwrap(),
                )),
            }))
            .await
            .unwrap();
    }

    fn renewal_request(
        &self,
        command: PublicV1SessionRenewalCommand,
    ) -> proto::ExecuteCommandRequest {
        proto::ExecuteCommandRequest {
            protocol_version: kuberic_runtime::protocol::PROTOCOL_VERSION,
            resource_uid: self.history.resource_uid.to_string(),
            target: Some(self.identity.clone().into()),
            expected_process_session_id: self.session.to_string(),
            command: Some(proto::execute_command_request::Command::PublicV1History(
                convert::public_v1_history_command_to_proto(
                    &PublicV1HistoryCommand::SessionRenewal {
                        command: Box::new(command),
                    },
                )
                .unwrap(),
            )),
        }
    }

    async fn renewal(&self, command: PublicV1SessionRenewalCommand) {
        let mut client =
            proto::agent_control_client::AgentControlClient::connect(self.control_address.clone())
                .await
                .unwrap();
        client
            .execute(authorized(self.renewal_request(command)))
            .await
            .unwrap();
    }

    async fn close(mut self) {
        self.command(PublicOperationProgram::Close).await;
        self.running.shutdown();
        self.running.wait().await.unwrap();
        self.service.drain().await.unwrap();
    }

    async fn stop(mut self) -> Option<kuberic_runtime::RuntimeError> {
        self.running.shutdown();
        self.running.wait().await.unwrap();
        self.service.drain().await.err()
    }
}

fn persist_query_observation(
    root: &Path,
    label: &str,
    report: &PublicV1AgentReport,
) -> PublicV1AgentReport {
    // This is an observation snapshot, not a native continuation database or
    // service durability receipt. Consumers only see the reloaded RPC report.
    let path = root.join(format!("{label}.json"));
    std::fs::write(&path, serde_json::to_vec(report).unwrap()).unwrap();
    std::fs::File::open(&path).unwrap().sync_all().unwrap();
    let reloaded: PublicV1AgentReport =
        serde_json::from_reader(std::fs::File::open(&path).unwrap()).unwrap();
    assert_eq!(&reloaded, report);
    reloaded.validate().unwrap();
    reloaded
}

fn query_accepted(report: &PublicV1AgentReport) -> PublicV1AcceptedStatus {
    PublicV1AcceptedStatus {
        identity: report.identity.clone(),
        history_initialization_id: derive_history_initialization_id(
            &report.history.resource_uid,
            PublicProtocolGeneration::new(report.identity.generation),
        ),
        history: report.history.clone(),
        data_loss_recovery: None,
        completed_operations: report
            .completed_operations
            .iter()
            .map(PublicV1AcceptedOperationCompletion::from_completion)
            .collect(),
        extensions: Default::default(),
    }
}

#[tokio::test]
async fn native_query_outcome_matrix_survives_rpc_persistence_and_controller_eligibility() {
    use kuberic_controller::evaluator::public_v1_topology::{
        PublicV1TopologyPlan, PublicV1TopologyReplica, plan_public_v1_topology,
    };
    use kuberic_controller::evaluator::testing::{
        PublicV1ElectionStatus, compare_public_v1_election_reports, public_v1_election_status,
    };
    use std::cmp::Ordering as Comparison;

    tokio::time::timeout(Duration::from_secs(60), async {
        let root = tempfile::tempdir().unwrap();
        let mut node = Node::<QueryService>::start_sized(&root.path().join("node"), 1, 0, 1).await;
        node.open(ReplicaRole::Primary, Epoch::new(0, 1)).await;
        let current = node.single(Epoch::new(0, 1)).await;
        let native = node.report().await;
        assert_eq!(native.progress.current(), Some(0));
        assert_eq!(native.progress.capability(), Some(0));
        let accepted = query_accepted(&native);
        let admitted = [native.storage_incarnation.clone()];
        let value = QueryScript::Value;
        let failed = QueryScript::Failed;
        let ready = QueryScript::NotReady;
        let cases = [
            ("unequal-capability-higher", value(7), value(11)),
            ("unequal-current-higher", value(11), value(7)),
            ("successful-zero", value(0), value(13)),
            ("current-not-ready", ready, value(13)),
            ("current-failed", failed("current query failure"), value(13)),
            ("capability-not-ready", value(17), ready),
            (
                "capability-failed",
                value(17),
                failed("capability query failure"),
            ),
            (
                "independent-failures",
                failed("current query failure"),
                failed("capability query failure"),
            ),
        ];
        for (label, current_query, capability_query) in cases {
            *node.service.scripts.outcomes.lock().unwrap() = [current_query, capability_query];
            let before = node
                .service
                .scripts
                .calls
                .each_ref()
                .map(|calls| calls.load(Ordering::SeqCst));
            let received = node.report().await;
            for (calls, previous) in node.service.scripts.calls.iter().zip(before) {
                assert!(calls.load(Ordering::SeqCst) > previous, "{label}");
            }
            let expected = |script: QueryScript| match script.result().unwrap() {
                Ok(value) => ProgressQueryResult::Value(value),
                Err(RuntimeError::ProgressNotReady) => ProgressQueryResult::NotReady,
                Err(error) => ProgressQueryResult::Failed(error.to_string()),
            };
            assert_eq!(
                received.progress.current_progress,
                expected(current_query),
                "{label}: current query must not borrow capability or a previous success"
            );
            assert_eq!(
                received.progress.catch_up_capability,
                expected(capability_query),
                "{label}: capability must not borrow current progress or become zero"
            );
            assert_eq!(
                received.replicator_creation_id,
                native.replicator_creation_id
            );
            assert_eq!(received.process_session_id, native.process_session_id);
            assert_eq!(received.storage_incarnation, native.storage_incarnation);
            assert_eq!(received.history, native.history);
            assert_eq!(received.extensions, native.extensions);
            assert_eq!(received.read_status, native.read_status);
            assert_eq!(received.write_status, native.write_status);
            let observed = persist_query_observation(root.path(), label, &received);
            let available = observed.progress.current().is_some();
            assert_eq!(
                public_v1_election_status(&observed, &admitted),
                if available {
                    PublicV1ElectionStatus::Eligible
                } else {
                    PublicV1ElectionStatus::Ineligible
                },
                "{label}: election requires current, not capability"
            );
            assert_eq!(
                compare_public_v1_election_reports(&observed, &native, &admitted),
                observed.progress.current().map(|current| current.cmp(&0)),
                "{label}"
            );
            assert_eq!(
                public_v1_election_status(&observed, &[]),
                PublicV1ElectionStatus::Ineligible,
                "{label}: successful queries do not admit unaccepted storage"
            );
            let plan = plan_public_v1_topology(
                &accepted,
                Some(&current),
                &[PublicV1TopologyReplica {
                    replication_address: observed.replication_address.clone().unwrap(),
                    report: observed,
                }],
                1,
                None,
                Some(node.identity.replica_id),
                "query-matrix",
            )
            .unwrap();
            assert_eq!(
                plan,
                if available {
                    PublicV1TopologyPlan::Stable
                } else {
                    PublicV1TopologyPlan::Wait
                },
                "{label}: missing current cannot authorize topology work"
            );
        }
        *node.service.scripts.outcomes.lock().unwrap() = [value(3), value(99)];
        let left = persist_query_observation(root.path(), "comparison-left", &node.report().await);
        *node.service.scripts.outcomes.lock().unwrap() = [value(5), value(1)];
        let right =
            persist_query_observation(root.path(), "comparison-right", &node.report().await);
        assert_eq!(
            compare_public_v1_election_reports(&left, &right, &admitted),
            Some(Comparison::Less),
            "election orders current progress, never the independent capability"
        );
        *node.service.scripts.outcomes.lock().unwrap() = [QueryScript::Native; 2];
        assert_eq!(node.report().await.progress, native.progress);
        node.close().await;
    })
    .await
    .expect("bounded connected native query matrix");
}

#[tokio::test]
async fn native_query_not_queried_preserves_copy_detail_and_cannot_elect_or_build() {
    use kuberic_controller::evaluator::public_v1_topology::{
        PublicV1TopologyPlan, PublicV1TopologyReplica, plan_public_v1_topology,
    };
    use kuberic_controller::evaluator::testing::{
        PublicV1ElectionStatus, compare_public_v1_election_reports, public_v1_election_status,
    };

    tokio::time::timeout(Duration::from_secs(60), async {
        let root = tempfile::tempdir().unwrap();
        let mut node = Node::<QueryService>::start_sized(&root.path().join("node"), 1, 0, 1).await;
        let unopened = persist_query_observation(root.path(), "unopened", &node.report().await);
        assert_eq!(
            unopened.progress.current_progress,
            ProgressQueryResult::NotReady
        );
        assert_eq!(
            unopened.progress.catch_up_capability,
            ProgressQueryResult::NotReady
        );
        node.open(ReplicaRole::IdleSecondary, Epoch::new(0, 1))
            .await;
        *node.service.scripts.outcomes.lock().unwrap() = [
            QueryScript::Failed("this callback must not run before copy"),
            QueryScript::Value(999),
        ];
        let calls = node
            .service
            .scripts
            .calls
            .each_ref()
            .map(|calls| calls.load(Ordering::SeqCst));
        let observed =
            persist_query_observation(root.path(), "copy-incomplete", &node.report().await);
        let pending = ProgressQueryResult::NotQueried("native copy is incomplete".into());
        assert_eq!(observed.progress.current_progress, pending);
        assert_eq!(observed.progress.catch_up_capability, pending);
        assert_ne!(observed.progress, unopened.progress);
        assert_eq!(
            node.service
                .scripts
                .calls
                .each_ref()
                .map(|calls| calls.load(Ordering::SeqCst)),
            calls,
            "native readiness suppresses both callbacks rather than fabricating a successful zero"
        );
        let admitted = [observed.storage_incarnation.clone()];
        assert_eq!(
            public_v1_election_status(&observed, &admitted),
            PublicV1ElectionStatus::Ineligible
        );
        assert_eq!(
            compare_public_v1_election_reports(&observed, &observed, &admitted),
            None
        );
        assert_eq!(
            plan_public_v1_topology(
                &query_accepted(&observed),
                None,
                &[PublicV1TopologyReplica {
                    replication_address: observed.replication_address.clone().unwrap(),
                    report: observed,
                }],
                1,
                None,
                None,
                "query-not-queried",
            )
            .unwrap(),
            PublicV1TopologyPlan::Wait,
            "an open endpoint without queried current is not promotion or Build authority"
        );
        node.close().await;
    })
    .await
    .expect("bounded unqueried native receiver observation");
}

#[tokio::test]
async fn native_query_rejoin_matrix_distinguishes_source_capability_from_target_current() {
    use kuberic_controller::evaluator::public_v1_topology::{
        PublicV1TopologyPlan, PublicV1TopologyReplica, plan_public_v1_topology,
    };

    tokio::time::timeout(Duration::from_secs(60), async {
        let root = tempfile::tempdir().unwrap();
        let mut source = Node::<QueryService>::start(&root.path().join("source"), 1, 0).await;
        let mut target = Node::<QueryService>::start(&root.path().join("target"), 2, 0).await;
        let epoch = Epoch::new(0, 1);
        source.open(ReplicaRole::Primary, epoch).await;
        let current = source.single(epoch).await;
        target.open(ReplicaRole::Primary, epoch).await;
        target.single(epoch).await;
        let mut accepted = query_accepted(&source.report().await);
        accepted.completed_operations.extend(
            query_accepted(&target.report().await).completed_operations,
        );
        accepted.validate_evidence().unwrap();
        let value = QueryScript::Value;
        let ready = QueryScript::NotReady;
        let failed = QueryScript::Failed;
        let cases = [
            ("retained", [value(7), value(3)], [value(5), value(99)], Some(ReplicaRole::ActiveSecondary)),
            ("range-requires-copy", [value(7), value(9)], [value(5), value(99)], Some(ReplicaRole::IdleSecondary)),
            ("source-capability-not-ready", [value(7), ready], [value(5), value(99)], Some(ReplicaRole::IdleSecondary)),
            ("source-capability-failed", [value(7), failed("source capability failed")], [value(5), value(99)], Some(ReplicaRole::IdleSecondary)),
            ("target-current-not-ready", [value(7), value(3)], [ready, value(99)], Some(ReplicaRole::IdleSecondary)),
            ("target-current-failed", [value(7), value(3)], [failed("target current failed"), value(99)], Some(ReplicaRole::IdleSecondary)),
            ("target-capability-not-ready", [value(7), value(3)], [value(5), ready], Some(ReplicaRole::ActiveSecondary)),
            ("target-capability-failed", [value(7), value(3)], [value(5), failed("target capability failed")], Some(ReplicaRole::ActiveSecondary)),
            ("source-current-not-ready", [ready, value(3)], [value(5), value(99)], None),
            ("source-current-failed", [failed("source current failed"), value(3)], [value(5), value(99)], None),
        ];
        for (label, source_queries, target_queries, expected_role) in cases {
            *source.service.scripts.outcomes.lock().unwrap() = source_queries;
            *target.service.scripts.outcomes.lock().unwrap() = target_queries;
            let mut replicas = Vec::new();
            for (name, node) in [("source", &source), ("target", &target)] {
                let report = persist_query_observation(
                    root.path(),
                    &format!("{label}-{name}"),
                    &node.report().await,
                );
                replicas.push(PublicV1TopologyReplica {
                    replication_address: report.replication_address.clone().unwrap(),
                    report,
                });
            }
            let plan = plan_public_v1_topology(
                &accepted,
                Some(&current),
                &replicas,
                2,
                None,
                Some(source.identity.replica_id),
                "query-rejoin",
            )
            .unwrap();
            if let Some(role) = expected_role {
                let PublicV1TopologyPlan::Begin { command, .. } = plan else {
                    panic!("{label}: expected exact target preparation, got {plan:?}");
                };
                assert_eq!(command.target, target.identity, "{label}");
                assert!(
                    matches!(
                        command.intent.program,
                        Some(PublicOperationProgram::Role { role: actual, .. }) if actual == role
                    ),
                    "{label}: retained rejoin prepares ActiveSecondary; copy prepares IdleSecondary: {:?}",
                    command.intent.program
                );
            } else {
                assert_eq!(
                    plan,
                    PublicV1TopologyPlan::Wait,
                    "{label}: unavailable source current cannot authorize rejoin or Build"
                );
            }
        }
        *source.service.scripts.outcomes.lock().unwrap() = [QueryScript::Native; 2];
        *target.service.scripts.outcomes.lock().unwrap() = [QueryScript::Native; 2];
        target.close().await;
        source.close().await;
    })
    .await
    .expect("bounded retained-rejoin query eligibility matrix");
}

#[tokio::test]
async fn public_factory_primary_process_renewal_retains_exact_role_address_authority() {
    tokio::time::timeout(Duration::from_secs(90), async {
            for (fail_replay, demote) in [(false, false), (false, true), (true, false)] {
            let root = tempfile::tempdir().unwrap();
            let epoch = Epoch::new(0, 1);
            let renewed_epoch = Epoch::new(0, 2);
            let mut predecessor = Node::<RoleAddressKvService>::start_sized(root.path(), 1, 0, 1).await;
            predecessor.open(ReplicaRole::Primary, epoch).await;
            predecessor.single(epoch).await;
            predecessor
                .service
                .inner
                .replicate_put("renewal".into(), "retained".into())
                .await
                .unwrap();
            let old = predecessor.report().await;
            let old_location = old.service_location.clone().unwrap();
            assert_eq!(old_location.address, predecessor.service.address);
            let old_revision = predecessor.revision;
            predecessor.stop().await;

            let mut replacement =
                Node::<RoleAddressKvService>::start_sized(root.path(), 1, old_revision, 1).await;
            let fresh = replacement.report().await;
            assert_eq!(fresh.storage_incarnation, old.storage_incarnation);
            assert_eq!(fresh.history, old.history);
            assert_ne!(fresh.process_session_id, old.process_session_id);
            assert_ne!(fresh.replicator_creation_id, old.replicator_creation_id);
            assert_eq!(fresh.role, ReplicaRole::None);
            assert!(fresh.service_location.is_none());
            assert!(fresh.completed_operations.is_empty());
            replacement
                .command(PublicOperationProgram::Open {
                    replica: replacement.identity.clone(),
                    existing: true,
                })
                .await;
            let open_revision = replacement.revision;
            let fresh = replacement.report().await;
            let endpoint = |report: &PublicV1AgentReport| ProcessSessionEndpoint {
                identity: report.replica.clone(),
                storage_incarnation: report.storage_incarnation.clone(),
                process_session_id: report.process_session_id.clone(),
                replicator_creation_id: report.replicator_creation_id.clone(),
                role: report.role,
            };
            let configuration = ConfigurationDescriptor::new(
                renewed_epoch,
                replacement.identity.replica_id,
                vec![ConfigurationMember {
                    identity: replacement.identity.clone(),
                    role: ReplicaRole::Primary,
                }],
                1,
            );
            let required = PublicV1SessionRenewalStatus::required(ProcessSessionRenewalRequired {
                operation_id: OperationId::new("real-primary-process-renewal"),
                reason: ProcessSessionRenewalReason::PrimaryReplacement,
                resource_uid: old.resource_uid.clone(),
                history: old.history.clone(),
                previous_epoch: epoch,
                renewal_epoch: renewed_epoch,
                predecessor: endpoint(&old),
                replacement: Some(endpoint(&fresh)),
                surviving_sessions: Vec::new(),
                fresh_configuration: configuration.clone(),
                predecessor_contained: true,
                data_loss: None,
            })
            .unwrap();
            required.required.validate_replacement_report(&fresh).unwrap();
            replacement
                .renewal(PublicV1SessionRenewalCommand::CloseAccessAndIngress {
                    accepted: required.clone(),
                })
                .await;
            let pending = required
                .close_access()
                .unwrap()
                .freeze_surviving_set()
                .unwrap()
                .begin_epoch_barriers()
                .unwrap()
                .finish_epoch_barriers()
                .unwrap();
            let replay = PublicV1SessionRenewalCommand::ReplayReplacementRole {
                accepted: pending,
                process_session_id: fresh.process_session_id.clone(),
                epoch: renewed_epoch,
                role: ReplicaRole::Primary,
            };
            replacement.service.block_primary.store(true, Ordering::SeqCst);
            replacement.service.fail_primary.store(fail_replay, Ordering::SeqCst);
            let mut client = proto::agent_control_client::AgentControlClient::connect(
                replacement.control_address.clone(),
            )
            .await
            .unwrap();
            let request = replacement.renewal_request(replay.clone());
            let mut waiter = tokio::spawn(async move { client.execute(authorized(request)).await });
            tokio::select! {
                () = replacement.service.primary_entered.notified() => {},
                result = &mut waiter => panic!("role replay returned before application callback: {result:?}"),
                () = tokio::time::sleep(Duration::from_secs(10)) => panic!("role callback was not entered"),
            }
            let in_flight = replacement.report().await;
            assert_ne!(in_flight.write_status, AccessStatus::Granted);
            assert!(in_flight.service_location.is_none());
            let role_intent = in_flight
                .active_operations
                .iter()
                .find(|intent| {
                    intent.program
                        == Some(PublicOperationProgram::Role {
                            epoch: renewed_epoch,
                            role: ReplicaRole::Primary,
                        })
                })
                .expect("application callback has a durable current-process owner")
                .clone();
            assert_eq!(role_intent.process_session_id, fresh.process_session_id);
            assert_eq!(role_intent.revision, open_revision + 1);
            assert_ne!(role_intent.operation_id, old_location.operation_id);
            waiter.abort();
            assert!(waiter.await.unwrap_err().is_cancelled());
            replacement.service.primary_release.notify_one();
            let completed = tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    let report = replacement.report().await;
                    if let Some(completion) = report.completed_operations.iter().find(|completion| {
                        completion.operation_id == role_intent.operation_id
                    }) {
                        break completion.clone();
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("cancelled RPC leaves role callback and completion owned");
            assert_eq!(completed.call_kind, PublicV1CallKind::Role);
            assert_eq!(completed.revision, role_intent.revision);
            assert_eq!(completed.process_session_id, fresh.process_session_id);
            assert_eq!(completed.input_digest, role_intent.input_digest);
            assert_eq!(completed.history, fresh.history);
            assert_eq!(completed.storage_incarnation, fresh.storage_incarnation);
            replacement.revision = completed.revision;
            if fail_replay {
                assert!(
                    matches!(&completed.result, PublicV1OperationResult::Failed(error)
                        if error.contains("replacement application role rejected")),
                    "{completed:?}"
                );
                replacement
                    .command(PublicOperationProgram::Configuration(PublicConfiguration {
                        current: configuration,
                        previous: None,
                        replicas: Vec::new(),
                    }))
                    .await;
                let rejected = replacement.report().await;
                assert!(rejected.configuration.is_some());
                assert_ne!(rejected.write_status, AccessStatus::Granted);
                assert!(rejected.service_location.is_none());
                assert!(replacement.service.inner.replicate_put("renewal".into(), "rejected".into()).await.is_err());
                assert_eq!(replacement.service.primary_calls.load(Ordering::SeqCst), 1);
                replacement.close().await;
                continue;
            }
            assert_eq!(completed.result, PublicV1OperationResult::Succeeded);
            replacement.renewal(replay.clone()).await;
            assert_eq!(replacement.service.primary_calls.load(Ordering::SeqCst), 1);
            assert!(replacement.report().await.service_location.is_none());
            replacement
                .command(PublicOperationProgram::Configuration(PublicConfiguration {
                    current: configuration.clone(),
                    previous: None,
                    replicas: Vec::new(),
                }))
                .await;
            let renewed = replacement.report().await;
            let location = renewed.service_location.as_ref().unwrap();
            assert_eq!(renewed.write_status, AccessStatus::Granted);
            assert_eq!(location.identity, replacement.preview);
            assert_eq!(location.resource_uid, fresh.resource_uid);
            assert_eq!(location.replica, fresh.replica);
            assert_eq!(location.process_session_id, fresh.process_session_id);
            assert_eq!(location.operation_id, completed.operation_id);
            assert_eq!(location.revision, completed.revision);
            assert_eq!(location.epoch, renewed_epoch);
            assert_eq!(location.address, replacement.service.address);
            assert_ne!(location.address, old_location.address);
            renewed.validate().unwrap();
            assert_eq!(
                replacement.service.inner.get("renewal").await.unwrap().as_deref(),
                Some("retained")
            );
            replacement
                .service
                .inner
                .replicate_put("renewal".into(), "new-process".into())
                .await
                .unwrap();
            assert_eq!(
                replacement.service.inner.get("renewal").await.unwrap().as_deref(),
                Some("new-process")
            );

            let mut stale_report = renewed.clone();
            stale_report.service_location = Some(old_location.clone());
            assert!(stale_report.validate().is_err());
            let stale_role = PublicOperationProgram::Role {
                epoch,
                role: ReplicaRole::Primary,
            };
            let failure = replacement
                .submit_result(PublicV1OperationCommand {
                    target: replacement.identity.clone(),
                    intent: PublicOperationIntent {
                        identity: replacement.preview.clone(),
                        operation_id: old_location.operation_id,
                        revision: old_location.revision,
                        process_session_id: old.process_session_id,
                        class: PublicOperationClass::Authority,
                        input_digest: stale_role.digest(),
                        replay_disposition: stale_role.replay_disposition(),
                        lifecycle: None,
                        program: Some(stale_role),
                    },
                })
                .await
                .unwrap_err();
            assert_eq!(failure.code(), tonic::Code::InvalidArgument);
            assert!(failure.message().contains("differs from execute envelope"));
            assert_eq!(replacement.report().await.service_location, renewed.service_location);
            assert_eq!(replacement.service.primary_calls.load(Ordering::SeqCst), 1);

            if demote {
                replacement
                    .command(PublicOperationProgram::Role {
                        epoch: Epoch::new(0, 3),
                        role: ReplicaRole::ActiveSecondary,
                    })
                    .await;
                let demoted = replacement.report().await;
                assert_eq!(demoted.role, ReplicaRole::ActiveSecondary);
                assert_ne!(demoted.write_status, AccessStatus::Granted);
                assert!(demoted.service_location.is_none());
            }
            replacement.command(PublicOperationProgram::Close).await;
            let closed = replacement.report().await;
            assert_eq!(closed.role, ReplicaRole::None);
            assert!(closed.service_location.is_none());
            assert!(closed.replication_address.is_none());
            replacement.running.shutdown();
            replacement.running.wait().await.unwrap();
            replacement.service.drain().await.unwrap();
            }
        })
        .await
        .expect("bounded real primary process renewal");
}

#[tokio::test]
async fn public_factory_real_kv_copy_quorum_commit_restart_and_promotion() {
    tokio::time::timeout(Duration::from_secs(120), async {
        let root = tempfile::tempdir().unwrap();
        let source_root = root.path().join("source");
        let target_root = root.path().join("target");
        let epoch = Epoch::new(0, 1);
        let mut source = Node::<KvService>::start(&source_root, 1, 0).await;
        let mut target = Node::<KvService>::start(&target_root, 2, 0).await;
        source.open(ReplicaRole::Primary, epoch).await;
        let single = source.single(epoch).await;
        target.open(ReplicaRole::IdleSecondary, epoch).await;
        source.build(&mut target, epoch, &single).await;
        let lsn = source
            .service
            .replicate_put("key".into(), "committed".into())
            .await
            .unwrap();
        assert!(lsn > 0);
        assert_eq!(
            source.service.get("key").await.unwrap().as_deref(),
            Some("committed")
        );
        assert_eq!(
            target.service.persistence().get("key").as_deref(),
            Some("committed")
        );
        let promotion_epoch = Epoch::new(0, 2);
        source
            .command(PublicOperationProgram::Role {
                epoch: promotion_epoch,
                role: ReplicaRole::ActiveSecondary,
            })
            .await;
        source
            .command(PublicOperationProgram::Epoch {
                epoch: promotion_epoch,
            })
            .await;
        target
            .command(PublicOperationProgram::Role {
                epoch: promotion_epoch,
                role: ReplicaRole::Primary,
            })
            .await;
        target
            .command(PublicOperationProgram::Epoch {
                epoch: promotion_epoch,
            })
            .await;
        let promoted = ConfigurationDescriptor::new(
            promotion_epoch,
            target.identity.replica_id,
            vec![
                ConfigurationMember {
                    identity: target.identity.clone(),
                    role: ReplicaRole::Primary,
                },
                ConfigurationMember {
                    identity: source.identity.clone(),
                    role: ReplicaRole::ActiveSecondary,
                },
            ],
            2,
        );
        source
            .command(PublicOperationProgram::Configuration(PublicConfiguration {
                current: promoted.clone(),
                previous: None,
                replicas: vec![target.descriptor(ReplicaRole::Primary).await],
            }))
            .await;
        target
            .command(PublicOperationProgram::Configuration(PublicConfiguration {
                current: promoted,
                previous: None,
                replicas: vec![source.descriptor(ReplicaRole::ActiveSecondary).await],
            }))
            .await;
        let promoted_lsn = target
            .service
            .replicate_put("key".into(), "after-promotion".into())
            .await
            .unwrap();
        assert!(promoted_lsn > lsn);
        assert_eq!(
            source.service.persistence().get("key").as_deref(),
            Some("after-promotion")
        );
        let source_revision = source.revision;
        let target_revision = target.revision;
        assert!(matches!(
            source.stop().await,
            Some(kuberic_runtime::RuntimeError::OperationCancelled)
        ));
        assert!(matches!(
            target.stop().await,
            Some(kuberic_runtime::RuntimeError::OperationCancelled)
        ));

        let mut source = Node::<KvService>::start(&source_root, 1, source_revision).await;
        let mut target = Node::<KvService>::start(&target_root, 2, target_revision).await;
        let epoch = Epoch::new(0, 3);
        target.open(ReplicaRole::Primary, epoch).await;
        let single = target.single(epoch).await;
        source.open(ReplicaRole::IdleSecondary, epoch).await;
        target.build(&mut source, epoch, &single).await;
        assert_eq!(
            target.service.get("key").await.unwrap().as_deref(),
            Some("after-promotion")
        );
        let later = target
            .service
            .replicate_put("key".into(), "after-restart".into())
            .await
            .unwrap();
        assert!(later > promoted_lsn);
        assert_eq!(
            source.service.persistence().get("key").as_deref(),
            Some("after-restart")
        );
        target.close().await;
        source.close().await;
    })
    .await
    .expect("bounded production KV journey");
}

async fn sql_write(
    server: &sqlite_replicated::server::SqliteServer,
    sql: &str,
) -> std::result::Result<i64, tonic::Status> {
    use sqlite_replicated::proto::sqlite_store_server::SqliteStore as _;
    server
        .execute(tonic::Request::new(
            sqlite_replicated::proto::ExecuteRequest {
                sql: sql.into(),
                params: Vec::new(),
            },
        ))
        .await
        .map(|response| response.into_inner().lsn)
}

async fn sql_values(
    server: &sqlite_replicated::server::SqliteServer,
) -> std::result::Result<Vec<String>, tonic::Status> {
    use sqlite_replicated::proto::{self, sqlite_store_server::SqliteStore as _};
    server
        .query(tonic::Request::new(proto::QueryRequest {
            sql: "SELECT value FROM data ORDER BY id".into(),
            params: Vec::new(),
        }))
        .await
        .map(|response| {
            response
                .into_inner()
                .rows
                .into_iter()
                .map(|row| match &row.values[0].kind {
                    Some(proto::value::Kind::TextValue(value)) => value.clone(),
                    other => panic!("unexpected SQLite value {other:?}"),
                })
                .collect()
        })
}

#[tokio::test]
async fn selected_sqlite_real_wal_isolation_publication_failure_and_restart() {
    use sqlite_replicated::server::SqliteServer;
    tokio::time::timeout(Duration::from_secs(120), async {
        let root = tempfile::tempdir().unwrap();
        let a = root.path().join("a");
        let b = root.path().join("b");
        let mut source = Node::<SqliteService>::start(&a, 1, 0).await;
        let mut target = Node::<SqliteService>::start(&b, 2, 0).await;
        let epoch = Epoch::new(0, 1);
        source.open(ReplicaRole::Primary, epoch).await;
        let single = source.single(epoch).await;
        target.open(ReplicaRole::IdleSecondary, epoch).await;
        source.build(&mut target, epoch, &single).await;
        let server = SqliteServer::new(source.service.clone());
        sql_write(
            &server,
            "CREATE TABLE data(id INTEGER PRIMARY KEY, value TEXT NOT NULL)",
        )
        .await
        .unwrap();
        let baseline = sql_write(&server, "INSERT INTO data VALUES (1,'acknowledged')")
            .await
            .unwrap();
        let committer = source.service.committer_for_test().unwrap();
        committer.before_commit().arm();
        committer.before_publication().arm();
        committer.before_success().arm();
        let client = server.clone();
        let write = tokio::spawn(async move {
            sql_write(&client, "INSERT INTO data VALUES (2,'isolated')").await
        });
        committer.before_commit().wait_entered().await;
        assert!(!write.is_finished());
        assert!(
            sql_values(&server).await.is_err(),
            "pending transaction must not leak"
        );
        assert_eq!(
            source
                .service
                .persistence()
                .progress()
                .unwrap()
                .committed_lsn,
            baseline
        );
        assert_eq!(source.report().await.read_status, AccessStatus::Granted);
        committer.before_commit().release();
        committer.before_publication().wait_entered().await;
        assert!(!write.is_finished());
        assert!(sql_values(&server).await.is_err());
        assert!(
            source
                .service
                .persistence()
                .progress()
                .unwrap()
                .committed_lsn
                > baseline
        );
        committer.before_publication().release();
        committer.before_success().wait_entered().await;
        assert!(!write.is_finished());
        assert!(sql_values(&server).await.is_err());
        committer.before_success().release();
        let committed = write.await.unwrap().unwrap();
        assert!(committed > baseline);
        assert_eq!(
            sql_values(&server).await.unwrap(),
            ["acknowledged", "isolated"]
        );
        drop(committer);
        drop(server);
        let ar = source.revision;
        let br = target.revision;
        assert!(matches!(
            source.stop().await,
            Some(kuberic_runtime::RuntimeError::OperationCancelled)
        ));
        assert!(matches!(
            target.stop().await,
            Some(kuberic_runtime::RuntimeError::OperationCancelled)
        ));

        let mut source = Node::<SqliteService>::start(&a, 1, ar).await;
        let mut target = Node::<SqliteService>::start(&b, 2, br).await;
        let epoch = Epoch::new(0, 2);
        source.open(ReplicaRole::Primary, epoch).await;
        let single = source.single(epoch).await;
        target.open(ReplicaRole::IdleSecondary, epoch).await;
        source.build(&mut target, epoch, &single).await;
        let server = SqliteServer::new(source.service.clone());
        assert_eq!(
            sql_values(&server).await.unwrap(),
            ["acknowledged", "isolated"]
        );
        let committer = source.service.committer_for_test().unwrap();
        committer.fail_once(sqlite_replicated::v1_test_bridge::FailureCut::Publication);
        let error = sql_write(&server, "INSERT INTO data VALUES (3,'ambiguous')")
            .await
            .unwrap_err();
        assert_eq!(error.code(), tonic::Code::Unknown);
        assert!(sql_values(&server).await.is_err());
        assert!(
            source
                .report()
                .await
                .progress
                .current()
                .is_none_or(|native| native > committed)
        );
        drop(committer);
        drop(server);
        let diagnostic = source
            .stop()
            .await
            .expect("retained publication diagnostic");
        assert!(
            diagnostic
                .to_string()
                .contains("injected SQLite service cut"),
            "{diagnostic}"
        );
        target.close().await;
    })
    .await
    .expect("bounded production SQLite journey");
}

#[tokio::test]
async fn externally_compiled_helper_free_provider_uses_public_factory_host_and_transport() {
    use bytes::Bytes;
    use futures::FutureExt;
    tokio::time::timeout(Duration::from_secs(120), async {
        let root = tempfile::tempdir().unwrap();
        let a = root.path().join("external-a");
        let b = root.path().join("external-b");
        let mut source = Node::<ExternalService>::start(&a, 1, 0).await;
        let mut target = Node::<ExternalService>::start(&b, 2, 0).await;
        let epoch = Epoch::new(0, 1);
        source.open(ReplicaRole::Primary, epoch).await;
        let single = source.single(epoch).await;
        assert!(
            source
                .service
                .begin_write(Bytes::from_static(b"not-admitted"))
                .await
                .is_err()
        );
        target.open(ReplicaRole::IdleSecondary, epoch).await;
        source.build(&mut target, epoch, &single).await;
        let first = source
            .service
            .begin_write(Bytes::from_static(b"abandoned-observer"))
            .await
            .unwrap();
        assert_eq!(
            first.lsn, 1,
            "definitive rejection did not consume native assignment"
        );
        assert!(
            first.wait().now_or_never().is_none(),
            "assignment precedes native quorum and service commit"
        );
        let second = source
            .service
            .begin_write(Bytes::from_static(b"acknowledged"))
            .await
            .unwrap();
        assert!(second.lsn > 1);
        let acknowledged = second.wait().await.unwrap();
        assert_eq!(
            source.service.values().await.unwrap(),
            [
                Bytes::from_static(b"abandoned-observer"),
                Bytes::from_static(b"acknowledged")
            ]
        );
        let source_revision = source.revision;
        let target_revision = target.revision;
        assert!(source.stop().await.is_none());
        assert!(target.stop().await.is_none());

        let mut source = Node::<ExternalService>::start(&a, 1, source_revision).await;
        let mut target = Node::<ExternalService>::start(&b, 2, target_revision).await;
        let epoch = Epoch::new(0, 2);
        target.open(ReplicaRole::Primary, epoch).await;
        let single = target.single(epoch).await;
        source.open(ReplicaRole::IdleSecondary, epoch).await;
        target.build(&mut source, epoch, &single).await;
        assert_eq!(
            target.service.values().await.unwrap(),
            [
                Bytes::from_static(b"abandoned-observer"),
                Bytes::from_static(b"acknowledged")
            ]
        );
        let later = target
            .service
            .begin_write(Bytes::from_static(b"recovered"))
            .await
            .unwrap();
        assert!(later.lsn > acknowledged);
        later.wait().await.unwrap();
        target.close().await;
        source.close().await;
    })
    .await
    .expect("bounded public external-provider journey");
}

#[tokio::test]
async fn controller_bootstraps_real_native_receivers_before_voting_admission() {
    use kuberic_controller::evaluator::public_v1_topology::{
        PublicV1TopologyPlan, PublicV1TopologyReplica, plan_public_v1_topology,
    };
    tokio::time::timeout(Duration::from_secs(120), async {
        let root = tempfile::tempdir().unwrap();
        let mut nodes = Vec::new();
        for id in 1..=3 {
            nodes.push(
                Node::<KvService>::start_sized(
                    &root.path().join(format!("replica-{id}")),
                    id,
                    0,
                    3,
                )
                .await,
            );
        }
        let mut accepted = PublicV1AcceptedStatus {
            identity: nodes[0].preview.clone(),
            history_initialization_id: derive_history_initialization_id(
                &nodes[0].history.resource_uid,
                PublicProtocolGeneration::new(1),
            ),
            history: nodes[0].history.clone(),
            data_loss_recovery: None,
            completed_operations: Vec::new(),
            extensions: Default::default(),
        };
        let mut current = None;
        let mut routed = None;
        let mut stable = false;
        for _ in 0..200 {
            let mut observations = Vec::new();
            for node in &nodes {
                let report = node.report().await;
                observations.push(PublicV1TopologyReplica {
                    replication_address: report.replication_address.clone().unwrap_or_default(),
                    report,
                });
            }
            match plan_public_v1_topology(
                &accepted,
                current.as_ref(),
                &observations,
                3,
                None,
                routed,
                "1",
            )
            .unwrap()
            {
                PublicV1TopologyPlan::Begin {
                    transition,
                    command,
                } => {
                    let source = nodes
                        .iter()
                        .position(|node| node.identity == command.target)
                        .unwrap();
                    if let Some(PublicOperationProgram::Build(build)) =
                        command.intent.program.as_ref()
                    {
                        let target = nodes
                            .iter()
                            .position(|node| node.identity == build.replica)
                            .unwrap();
                        nodes[target]
                            .install_build_authority(PublicInboundBuildAuthority {
                                operation_id: command.intent.operation_id.clone(),
                                operation_revision: command.intent.revision,
                                attempt: PublicBuildAttemptReference {
                                    resource_uid: accepted.history.resource_uid.clone(),
                                    epoch: build.epoch,
                                    build_id: build.attempt.clone(),
                                    source: nodes[source].identity.clone(),
                                    source_storage: nodes[source].storage.clone(),
                                    source_session: nodes[source].session.clone(),
                                    target: nodes[target].identity.clone(),
                                    target_storage: nodes[target].storage.clone(),
                                    target_session: nodes[target].session.clone(),
                                },
                                history: accepted.history.clone(),
                                current_configuration_id: transition
                                    .intent
                                    .current_configuration_id
                                    .clone()
                                    .unwrap(),
                                source_address: nodes[source]
                                    .report()
                                    .await
                                    .replication_address
                                    .unwrap(),
                                target_address: build.replication_address.clone(),
                            })
                            .await;
                    }
                    let id = command.intent.operation_id.clone();
                    nodes[source].submit(*command).await;
                    let report = nodes[source].report().await;
                    let completion = report
                        .completed_operations
                        .iter()
                        .find(|completion| completion.operation_id == id)
                        .unwrap();
                    assert_eq!(completion.result, PublicV1OperationResult::Succeeded);
                    accepted.completed_operations.push(
                        PublicV1AcceptedOperationCompletion::from_completion(completion),
                    );
                    accepted.validate_evidence().unwrap();
                }
                PublicV1TopologyPlan::PersistTopology { expected, next } => {
                    assert_eq!(
                        expected.as_ref(),
                        current
                            .as_ref()
                            .map(|configuration: &ConfigurationDescriptor| {
                                &configuration.configuration_id
                            })
                    );
                    current = Some(*next);
                }
                PublicV1TopologyPlan::RoutePrimary { primary } => routed = Some(primary.replica_id),
                PublicV1TopologyPlan::Wait => tokio::time::sleep(Duration::from_millis(10)).await,
                PublicV1TopologyPlan::Stable => {
                    stable = true;
                    break;
                }
            }
        }
        assert!(
            stable,
            "public controller/native bootstrap did not converge"
        );
        assert_eq!(current.unwrap().members.len(), 3);
        let primary = nodes
            .iter()
            .find(|node| node.identity.replica_id == ReplicaId::new(1))
            .unwrap();
        primary
            .service
            .replicate_put("controller".into(), "committed".into())
            .await
            .unwrap();
        for node in nodes {
            assert_eq!(
                node.service.persistence().get("controller").as_deref(),
                Some("committed")
            );
            node.close().await;
        }
    })
    .await
    .expect("bounded controller-to-production-host bootstrap");
}

#[tokio::test]
async fn production_carrier_rejects_stale_bindings_and_retires_successor_copy() {
    tokio::time::timeout(Duration::from_secs(120), async {
        let root = tempfile::tempdir().unwrap();
        let mut source = Node::<KvService>::start(&root.path().join("source"), 1, 0).await;
        let mut target = Node::<KvService>::start(&root.path().join("target"), 2, 0).await;
        let epoch = Epoch::new(0, 1);
        source.open(ReplicaRole::Primary, epoch).await;
        let single = source.single(epoch).await;
        target.open(ReplicaRole::IdleSecondary, epoch).await;
        let first_build = source.build(&mut target, epoch, &single).await;
        let source_report = source.report().await;
        let target_report = target.report().await;
        let configuration = source_report
            .completed_operations
            .iter()
            .filter_map(|completion| completion.current_configuration_id.as_ref())
            .next_back()
            .unwrap();
        let endpoint = |report: &PublicV1AgentReport| {
            serde_json::json!({
                "identity": report.replica,
                "processSessionId": report.process_session_id,
                "storage": report.storage_incarnation,
                "address": report.replication_address,
            })
        };
        let binding = serde_json::json!({
            "history": source_report.history,
            "epoch": epoch,
            "currentConfigurationId": configuration,
            "previousConfigurationId": null,
            "primary": endpoint(&source_report),
            "secondary": endpoint(&target_report),
            "build": null,
        });
        let mut rejected = vec![(binding.clone(), "Bearer incorrect".to_owned())];
        for (side, field) in [("primary", "processSessionId"), ("secondary", "address")] {
            let mut stale = binding.clone();
            stale[side][field] = "stale".into();
            rejected.push((stale, format!("Bearer {CREDENTIAL}")));
        }
        let mut stale_storage = binding.clone();
        stale_storage["primary"]["storage"]["storageGeneration"] = "retired".into();
        rejected.push((stale_storage, format!("Bearer {CREDENTIAL}")));
        let mut stale_history = binding;
        stale_history["history"]["historyId"] = "predecessor".into();
        rejected.push((stale_history, format!("Bearer {CREDENTIAL}")));
        for (binding, credential) in rejected {
            let mut client = native_wire::carrier_client::CarrierClient::connect(
                target_report.replication_address.clone().unwrap(),
            )
            .await
            .unwrap();
            let mut request =
                tonic::Request::new(futures::stream::empty::<native_wire::Envelope>());
            request
                .metadata_mut()
                .insert("authorization", credential.parse().unwrap());
            request.metadata_mut().insert_bin(
                "kuberic-native-binding-bin",
                tonic::metadata::MetadataValue::from_bytes(&serde_json::to_vec(&binding).unwrap()),
            );
            let failure = client.exchange(request).await.unwrap_err();
            assert!(
                matches!(
                    failure.code(),
                    tonic::Code::Unauthenticated | tonic::Code::PermissionDenied
                ),
                "{failure}"
            );
        }
        source
            .service
            .replicate_put("identity".into(), "unaffected".into())
            .await
            .unwrap();
        assert_eq!(
            target.service.persistence().get("identity").as_deref(),
            Some("unaffected")
        );

        let mut successor = first_build;
        successor.operation_revision = source.revision + 1;
        successor.operation_id = OperationId::new("successor-copy");
        successor.attempt.build_id = successor.operation_id.clone();
        let mut client =
            proto::replica_peer_client::ReplicaPeerClient::connect(target.control_address.clone())
                .await
                .unwrap();
        let failure = client
            .execute(authorized(proto::ExecuteCommandRequest {
                protocol_version: kuberic_runtime::protocol::PROTOCOL_VERSION,
                resource_uid: target.history.resource_uid.to_string(),
                target: Some(target.identity.clone().into()),
                expected_process_session_id: target.session.to_string(),
                command: Some(proto::execute_command_request::Command::PublicV1History(
                    convert::public_v1_history_command_to_proto(
                        &PublicV1HistoryCommand::InstallInboundBuildAuthority {
                            authority: Box::new(successor),
                        },
                    )
                    .unwrap(),
                )),
            }))
            .await
            .unwrap_err();
        assert!(failure.message().contains("requires process replacement"));
        let retired = target.report().await;
        assert_eq!(retired.read_status, AccessStatus::NotPrimary);
        assert_eq!(retired.write_status, AccessStatus::NotPrimary);
        source.stop().await;
        target.stop().await;
    })
    .await
    .expect("bounded production admission and receiver retirement");
}

#[tokio::test]
async fn helper_free_recovery_scopes_changed_unchanged_and_failed_observations() {
    use bytes::Bytes;
    tokio::time::timeout(Duration::from_secs(120), async {
        for outcome in ["unchanged", "changed", "failed"] {
            let root = tempfile::tempdir().unwrap();
            let mut node = Node::<ExternalService>::start_sized(root.path(), 1, 0, 1).await;
            node.command(PublicOperationProgram::Open {
                replica: node.identity.clone(),
                existing: false,
            })
            .await;
            let before = node.report().await;
            let authorization_id = OperationId::new(format!("loss-{outcome}"));
            let proposed =
                derive_data_loss_history(&authorization_id, &node.history, 1, &node.storage)
                    .unwrap();
            let epoch = Epoch::new(1, 2);
            let revision = node.revision + 1;
            let authorization = DataLossAuthorization {
                authorization_id: authorization_id.clone(),
                predecessor: node.history.clone(),
                proposed: proposed.clone(),
                provisional_primary: node.storage.clone(),
                reset_scope: ProviderProgressReset::Preserve,
                replay_disposition: DataLossReplay::ReplaceOnAmbiguity,
            };
            let input = PublicLifecycleInput {
                recipe: PublicLifecycleRecipe::InitialPrimary,
                replica: node.identity.clone(),
                epoch,
                possible_data_loss: PossibleDataLossIntent::Possible,
                current: ConfigurationDescriptor::new(
                    epoch,
                    node.identity.replica_id,
                    vec![ConfigurationMember {
                        identity: node.identity.clone(),
                        role: ReplicaRole::Primary,
                    }],
                    1,
                ),
                previous: None,
                replicas: Vec::new(),
                data_loss_recovery: Some(PublicDataLossRecoveryInput {
                    authorization,
                    attempt: DataLossAttempt {
                        authorization_id,
                        attempt_id: OperationId::new(format!("recover-{outcome}")),
                        process: ProcessAttemptContext {
                            process_session_id: node.session.clone(),
                            replicator_creation_id: before.replicator_creation_id,
                            catch_up_specific_quorum: before
                                .extensions
                                .catch_up_specific_quorum
                                .unwrap(),
                            data_loss_replay: DataLossReplay::ReplaceOnAmbiguity,
                            epoch,
                            attempt_revision: revision,
                        },
                    },
                }),
            };
            input.validate().unwrap();
            let command = PublicV1OperationCommand {
                target: node.identity.clone(),
                intent: PublicOperationIntent {
                    identity: node.preview.clone(),
                    operation_id: OperationId::new(format!("recover-{outcome}")),
                    revision,
                    process_session_id: node.session.clone(),
                    class: PublicOperationClass::Authority,
                    input_digest: input.digest(),
                    replay_disposition: PublicOperationReplayDisposition::DataLoss(
                        DataLossReplay::ReplaceOnAmbiguity,
                    ),
                    lifecycle: Some(input),
                    program: None,
                },
            };
            let service = node.service.clone();
            service.pause_recovery(match outcome {
                "unchanged" => None,
                "changed" => Some(Ok(vec![Bytes::from_static(b"restored")])),
                "failed" => Some(Err("provider recovery failed".into())),
                _ => unreachable!(),
            });
            let control = node.control_address.clone();
            let history = node.history.clone();
            let identity = node.identity.clone();
            let mut task = tokio::spawn(async move {
                let result = node.submit_result(command).await;
                (node, result)
            });
            tokio::select! {
                _ = service.recovery_entered() => {}
                result = &mut task => panic!(
                    "recovery did not reach provider: {:?}",
                    result.map(|(_, result)| result),
                ),
            }
            let pending = report_at(&control, &history.resource_uid, &identity).await;
            assert_eq!(pending.history, history);
            assert!(matches!(
                pending.progress.current_progress,
                ProgressQueryResult::NotQueried(_)
            ));
            assert_eq!(pending.read_status, AccessStatus::NotPrimary);
            assert_eq!(pending.write_status, AccessStatus::NotPrimary);
            service.release_recovery();
            let (mut node, result) = task.await.unwrap();
            let report = node.report().await;
            if outcome == "failed" {
                assert!(
                    result
                        .unwrap_err()
                        .message()
                        .contains("provider recovery failed")
                );
                assert_eq!(report.history, history);
                assert!(report.progress.current().is_none());
                assert_eq!(report.write_status, AccessStatus::NotPrimary);
                node.stop().await;
            } else {
                result.unwrap();
                assert_eq!(report.history, proposed);
                assert!(report.has_current_open_endpoint());
                let restored = if outcome == "changed" { 1 } else { 0 };
                assert_eq!(report.progress.current(), Some(restored));
                assert_eq!(
                    report.data_loss_recovery.as_ref().unwrap().stage,
                    PublicDataLossRecoveryStage::Admitted
                );
                let write = node
                    .service
                    .begin_write(Bytes::from_static(b"new-history"))
                    .await
                    .unwrap();
                assert_eq!(write.lsn, restored + 1);
                write.wait().await.unwrap();
                node.history = proposed;
                node.close().await;
            }
        }
    })
    .await
    .expect("bounded helper-free recovery observation scope");
}
