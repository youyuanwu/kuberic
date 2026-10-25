use std::sync::Arc;
use std::time::Duration;

use kuberic_runtime::host::{
    ApplicationStorageState, ReplicaEndpointResolver, ReplicaHost, ReplicaProcessConfig,
    RunningReplica,
};
use kuberic_runtime::protocol::public_operations::{
    ORDINARY_ACTION_FORMAT, PublicCatchUpMode, PublicV1Identity, ReplicaAgentQueryResult,
    ReplicaAgentSnapshot, ReplicaConfigurationScope, ReplicaGoal, ReplicaGoalKind,
    ReplicaGoalStatus, ReplicaScope,
};
use kuberic_runtime::protocol::types::{
    AgentGeneration, ConfigurationDescriptor, ConfigurationMember, EffectivePolicy, Epoch,
    InitializationId, PodUid, PvcUid, ReplicaId, ReplicaIdentity, ReplicaInstanceId, ReplicaRole,
    ReplicaStorageIdentity, ReplicaStorageIncarnationContext, ResourceUid, StorageGeneration,
    derive_bootstrap_history, derive_history_initialization_id,
};
use kuberic_runtime::testing::sqlite_store::SqliteStore;
use kuberic_runtime::testing::state::{SCHEMA_VERSION, StorageIdentity};
use sqlite_replicated::SqlitePersistence;
use sqlite_replicated::proto;
use sqlite_replicated::proto::sqlite_store_server::SqliteStore as _;
use sqlite_replicated::server::SqliteServer;
use sqlite_replicated::service::SqliteService;

struct Resolver;

impl ReplicaEndpointResolver for Resolver {
    fn control_endpoint(&self, _identity: &ReplicaIdentity) -> String {
        "http://127.0.0.1:1".into()
    }

    fn replication_endpoint(&self, _identity: &ReplicaIdentity) -> String {
        "http://127.0.0.1:1".into()
    }
}

struct Fixture {
    root: std::path::PathBuf,
    persistence: Arc<SqlitePersistence>,
    service: Arc<SqliteService>,
    server: SqliteServer,
    running: RunningReplica,
}

impl Fixture {
    async fn start(name: &str) -> Self {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/sqlite-publication-receipts")
            .join(format!("{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();

        let resource_uid = ResourceUid::new(format!("sqlite-{name}"));
        let preview = PublicV1Identity::new(1);
        let identity = ReplicaIdentity {
            replica_id: ReplicaId::new(1),
            instance_id: ReplicaInstanceId::new(format!("{name}-pod")),
            agent_generation: AgentGeneration::new(format!("{name}-agent")),
        };
        let storage = ReplicaStorageIncarnationContext {
            replica: identity.clone(),
            storage: ReplicaStorageIdentity::Persistent {
                pod_uid: PodUid::new(format!("{name}-pod")),
                pvc_uid: PvcUid::new(format!("{name}-pvc")),
            },
            storage_generation: StorageGeneration::new(format!("{name}-storage")),
        };
        drop(
            SqliteStore::create_public_v1(
                SqliteStore::metadata_database_path(&root),
                preview.clone(),
                StorageIdentity {
                    schema_version: SCHEMA_VERSION,
                    resource_uid: resource_uid.clone(),
                    pod_uid: PodUid::new(format!("{name}-pod")),
                    pvc_uid: PvcUid::new(format!("{name}-pvc")),
                    initialization_id: InitializationId::new(format!("{name}-initialization")),
                    local_identity: identity.clone(),
                    effective_policy: EffectivePolicy::fixed(1, 30).unwrap(),
                },
            )
            .unwrap(),
        );

        let replication_address = address();
        let persistence = Arc::new(SqlitePersistence::open(root.join("application")).unwrap());
        let service = Arc::new(
            SqliteService::new(persistence.clone(), format!("http://{replication_address}"))
                .unwrap(),
        );
        let server = SqliteServer::new(service.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let control_address = format!("http://{}", listener.local_addr().unwrap());
        let running = ReplicaHost::new(
            ReplicaProcessConfig {
                resource_uid: resource_uid.clone(),
                replica_id: identity.replica_id,
                pod_uid: PodUid::new(format!("{name}-pod")),
                pvc_uid: PvcUid::new(format!("{name}-pvc")),
                data_root: root.clone(),
                replication_address,
                bearer_token: "unused".into(),
                rpc_deadline: Duration::from_secs(2),
                transport_window_capacity: 8,
            },
            service.clone(),
            ApplicationStorageState::FreshEmpty,
            Arc::new(Resolver),
        )
        .start(listener)
        .await
        .unwrap();
        let handle = running.handle();
        let generation = kuberic_runtime::protocol::types::PublicProtocolGeneration::new(1);
        let initialization = derive_history_initialization_id(&resource_uid, generation);
        let history = derive_bootstrap_history(&resource_uid, generation, &initialization);
        handle
            .initialize_public_v1_history(&initialization, &history, &storage)
            .await
            .unwrap();
        let epoch = Epoch::new(0, 1);
        let configuration = ConfigurationDescriptor::new(
            epoch,
            identity.replica_id,
            vec![ConfigurationMember {
                identity: identity.clone(),
                role: ReplicaRole::Primary,
            }],
            1,
        );
        let mut client =
            kuberic_runtime::control::proto::agent_control_client::AgentControlClient::connect(
                control_address,
            )
            .await
            .unwrap();
        let response = client
            .get_snapshot(authorized(
                kuberic_runtime::control::proto::ReplicaSnapshotRequest {
                    protocol_version: ORDINARY_ACTION_FORMAT,
                    resource_uid: resource_uid.to_string(),
                    target: Some(identity.into()),
                    expected_process_session_id: handle.process_session_id().to_string(),
                },
            ))
            .await
            .unwrap()
            .into_inner();
        let ReplicaAgentQueryResult::Initialized(snapshot) =
            serde_json::from_slice(&response.canonical_json).unwrap()
        else {
            panic!("ordinary publication fixture snapshot required")
        };
        let goal = ReplicaGoal {
            format: ORDINARY_ACTION_FORMAT,
            identity: preview,
            operation_id: kuberic_runtime::protocol::types::OperationId::new(format!(
                "{name}-prepare"
            )),
            revision: 1,
            scope: ReplicaScope {
                history: snapshot.history,
                storage: snapshot.storage,
                process_session_id: snapshot.process_session_id,
                creation_id: snapshot.creation_id,
                epoch,
                configuration: ReplicaConfigurationScope {
                    current: configuration,
                    previous: None,
                    replicas: Vec::new(),
                },
            },
            desired: ReplicaGoalKind::Prepare {
                role: ReplicaRole::Primary,
            },
        };
        for goal in [
            goal.clone(),
            ReplicaGoal {
                operation_id: kuberic_runtime::protocol::types::OperationId::new(format!(
                    "{name}-admit"
                )),
                revision: 2,
                desired: ReplicaGoalKind::Reconfigure {
                    role: ReplicaRole::Primary,
                    catch_up: Some(PublicCatchUpMode::WriteQuorum),
                },
                ..goal
            },
        ] {
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    let response = client
                        .submit_goal(authorized(
                            kuberic_runtime::control::proto::ReplicaGoalRequest {
                                protocol_version: ORDINARY_ACTION_FORMAT,
                                canonical_json: serde_json::to_vec(&goal).unwrap(),
                            },
                        ))
                        .await
                        .unwrap()
                        .into_inner();
                    let snapshot: ReplicaAgentSnapshot =
                        serde_json::from_slice(&response.canonical_json).unwrap();
                    if let Some(entry) = snapshot.goals.iter().find(|entry| entry.goal == goal)
                        && entry.status != ReplicaGoalStatus::Pending
                    {
                        assert_eq!(entry.status, ReplicaGoalStatus::Achieved);
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
        }

        Self {
            root,
            persistence,
            service,
            server,
            running,
        }
    }

    async fn execute(&self, sql: &str) -> Result<proto::ExecuteResponse, tonic::Status> {
        self.server
            .execute(tonic::Request::new(proto::ExecuteRequest {
                sql: sql.into(),
                params: Vec::new(),
            }))
            .await
            .map(tonic::Response::into_inner)
    }

    async fn stop(mut self, expected: Option<&str>) {
        self.running.shutdown();
        let actual_close = self.running.wait().await;
        match expected {
            None => actual_close.unwrap(),
            Some(message) => assert!(
                actual_close
                    .as_ref()
                    .is_err_and(|error| error.to_string().contains(message)),
                "actual host Close hid publication failure: {actual_close:?}"
            ),
        }
        let diagnostic = self.service.shutdown_and_drain().await;
        match expected {
            None => diagnostic.unwrap(),
            Some(message) => assert!(
                matches!(
                    diagnostic,
                    Err(kuberic_runtime::RuntimeError::Application(ref reason)) if reason.contains(message)
                ),
                "{diagnostic:?}"
            ),
        }
        drop(self.server);
        drop(self.service);
        drop(self.persistence);
        drop(self.running);
        std::fs::remove_dir_all(self.root).unwrap();
    }
}

fn authorized<T>(message: T) -> tonic::Request<T> {
    let mut request = tonic::Request::new(message);
    request
        .metadata_mut()
        .insert("authorization", "Bearer unused".parse().unwrap());
    request
}

fn address() -> std::net::SocketAddr {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    address
}

#[tokio::test]
async fn successful_noop_does_not_advance_or_fence_the_next_write() {
    let fixture = Fixture::start("noop").await;
    let created = fixture
        .execute("CREATE TABLE data(id INTEGER PRIMARY KEY, value TEXT)")
        .await
        .unwrap();

    let noop = fixture
        .execute("UPDATE data SET value = value WHERE id = -1")
        .await
        .unwrap();
    assert_eq!(noop.rows_affected, 0);
    assert_eq!(noop.lsn, created.lsn);
    assert_eq!(
        fixture.persistence.progress().unwrap().committed_lsn,
        created.lsn
    );

    let inserted = fixture
        .execute("INSERT INTO data VALUES(1, 'published')")
        .await
        .unwrap();
    assert_eq!(inserted.lsn, created.lsn + 1);
    assert_eq!(
        fixture.persistence.progress().unwrap().committed_lsn,
        inserted.lsn
    );

    fixture.stop(None).await;
}

#[tokio::test]
async fn failed_local_publication_returns_no_success_and_fences() {
    let fixture = Fixture::start("stale").await;
    fixture
        .execute("CREATE TABLE data(id INTEGER PRIMARY KEY, value TEXT)")
        .await
        .unwrap();
    fixture
        .service
        .committer_for_test()
        .unwrap()
        .fail_once(sqlite_replicated::v1_test_bridge::FailureCut::Publication);

    let error = fixture
        .execute("INSERT INTO data VALUES(1, 'ambiguous')")
        .await
        .unwrap_err();
    assert_eq!(error.code(), tonic::Code::Unknown);
    assert!(error.message().contains("injected SQLite service cut"));
    assert!(
        fixture
            .execute("INSERT INTO data VALUES(2, 'must-not-succeed')")
            .await
            .is_err()
    );

    fixture.stop(Some("injected SQLite service cut")).await;
}
