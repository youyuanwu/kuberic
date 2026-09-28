//! Package-local in-process v2 fixture. Authority belongs to the agent-side test
//! driver, never to the production SQLite service. Phase 3 extracts routing.
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use kuberic_agent::hosting::{OutboundReplication, PodRuntime};
use kuberic_agent::runtime_adapter::RuntimeAdapter;
use kuberic_agent::session::ProcessSession;
use kuberic_agent::sqlite_store::SqliteStore;
use kuberic_agent::state::{AgentState, SCHEMA_VERSION, StorageIdentity};
use kuberic_agent::store::AgentStore;
use kuberic_protocol::types::*;
use kuberic_runtime::application::OpenMode;
use kuberic_runtime::replicator::copy::{BuildConfiguration, PrepareCopyRequest};
use kuberic_runtime_internal::authority::AdmittedAuthority;
use kuberic_runtime_internal::effects::{RuntimeEffect, RuntimeEffectAction};
use tonic::{Request, Status};

use crate::proto::sqlite_store_server::SqliteStore as _;
use crate::{SqlitePersistence, proto, server::SqliteServer, service::SqliteService};

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
        let identity = identity(id);
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
                    pvc_uid: PvcUid::new(format!("sqlite-pvc-{id}")),
                    initialization_id: InitializationId::new(format!("sqlite-init-{id}")),
                    local_identity: identity.clone(),
                    effective_policy: EffectivePolicy::fixed(replicas, 30).unwrap(),
                }),
            )
            .unwrap()
        };
        let store = Arc::new(store);
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
        RuntimeAdapter::new(self.store.clone(), self.runtime.clone())
            .execute(RuntimeEffect {
                operation_id: OperationId::new(format!("sqlite-effect-{sequence}")),
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

    pub async fn reopen(self) -> Self {
        let id = self.identity.replica_id.value();
        let root = self.root.clone();
        let replicas = self.replicas;
        let old_session = self.session.id().clone();
        self.runtime.abort();
        drop(self);
        let pod = Self::new(id, root, replicas).await;
        assert_ne!(*pod.session.id(), old_session);
        let state = pod.store.load_state().await.unwrap();
        assert!(
            state.pending_effect.is_none(),
            "fixture restart requires a settled control effect"
        );
        pod.runtime
            .reconstruct(
                OpenMode::Existing,
                state.role,
                state.read_status,
                state.write_status,
                None,
            )
            .await
            .unwrap();
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

pub struct Routes(Vec<tokio::task::JoinHandle<()>>);
impl Drop for Routes {
    fn drop(&mut self) {
        for task in &self.0 {
            task.abort();
        }
    }
}

/// Deliver actual public wire messages and their explicit durable service ACKs.
pub fn route(source: &SqlitePod, targets: &[&SqlitePod]) -> Routes {
    let source = source.runtime.clone();
    let targets: Vec<_> = targets
        .iter()
        .map(|p| (p.identity.clone(), p.runtime.clone()))
        .collect();
    Routes(vec![tokio::spawn(async move {
        while let Some(outbound) = source.data_plane().next_outbound().await {
            match outbound {
                OutboundReplication::Replication(item) => {
                    let identity: ReplicaIdentity =
                        item.receiver.clone().unwrap().try_into().unwrap();
                    // The caller supplies reachable peers. An omitted failed
                    // replica receives neither delivery nor fabricated ACK credit.
                    let Some((_, target)) = targets.iter().find(|(id, _)| *id == identity) else {
                        continue;
                    };
                    let pending = target.data_plane().receive_replication(item).await.unwrap();
                    source
                        .data_plane()
                        .accept_acknowledgement(pending.received.clone())
                        .await
                        .unwrap();
                    let ack = pending.applied().await.unwrap();
                    source
                        .data_plane()
                        .accept_acknowledgement(ack)
                        .await
                        .unwrap();
                }
                other => panic!("unexpected routed operation: {other:?}"),
            }
        }
    })])
}

pub async fn wait_applied(pods: &[&SqlitePod], lsn: i64) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if pods.iter().all(|pod| {
                pod.application
                    .persistence()
                    .progress()
                    .unwrap()
                    .applied_lsn
                    >= lsn
            }) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("replicas applied the operation");
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
        loop {
            let item = prepared.items.next().await.unwrap().unwrap();
            let last = item.final_item;
            let ack = target
                .runtime
                .data_plane()
                .receive_copy_item(item)
                .await
                .unwrap();
            pods[0]
                .runtime
                .data_plane()
                .accept_copy_acknowledgement(ack)
                .await
                .unwrap();
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
