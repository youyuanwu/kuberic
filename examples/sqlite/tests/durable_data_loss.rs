use std::sync::Arc;
use std::time::Duration;

use kuberic_runtime::host::{
    ApplicationStorageState, ReplicaEndpointResolver, ReplicaHost, ReplicaProcessConfig,
};
use kuberic_runtime::protocol::public_operations::{
    PreviewLifecycleBinding, ProcessSessionRenewalRequired, PublicBuildInput, PublicConfiguration,
    PublicOperationClass, PublicOperationIntent, PublicOperationPreviewIdentity,
    PublicOperationProgram, PublicReplicaDescriptor, StatePersistence,
};
use kuberic_runtime::protocol::types::{
    AgentGeneration, ConfigurationDescriptor, ConfigurationMember, EffectivePolicy, Epoch,
    InitializationId, OperationId, PodUid, ProcessSessionEndpoint, ProcessSessionRenewalReason,
    PublicProtocolGeneration, PvcUid, ReplicaId, ReplicaIdentity, ReplicaInstanceId, ReplicaRole,
    ReplicaStorageIdentity, ReplicaStorageIncarnationContext, ReplicatorCreationId, ResourceUid,
    StorageGeneration, derive_bootstrap_history, derive_history_initialization_id,
};
use kuberic_runtime::testing::sqlite_store::SqliteStore;
use kuberic_runtime::testing::state::{SCHEMA_VERSION, StorageIdentity};
use sqlite_replicated::SqlitePersistence;
use sqlite_replicated::proto;
use sqlite_replicated::proto::sqlite_store_server::SqliteStore as _;
use sqlite_replicated::server::SqliteServer;
use sqlite_replicated::service::SqliteService;
use sqlite_replicated::testing::{SqlitePod, scratch};

struct Resolver;

impl ReplicaEndpointResolver for Resolver {
    fn control_endpoint(&self, _identity: &ReplicaIdentity) -> String {
        "http://127.0.0.1:1".into()
    }

    fn replication_endpoint(&self, _identity: &ReplicaIdentity) -> String {
        "http://127.0.0.1:1".into()
    }
}

fn address() -> std::net::SocketAddr {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    address
}

fn identity(id: i64, name: &str) -> ReplicaIdentity {
    ReplicaIdentity {
        replica_id: ReplicaId::new(id),
        instance_id: ReplicaInstanceId::new(format!("{name}-pod")),
        agent_generation: AgentGeneration::new(format!("{name}-agent")),
    }
}

fn storage(identity: ReplicaIdentity, name: &str) -> ReplicaStorageIncarnationContext {
    ReplicaStorageIncarnationContext {
        replica: identity,
        storage: ReplicaStorageIdentity::Persistent {
            pod_uid: PodUid::new(format!("{name}-pod")),
            pvc_uid: PvcUid::new(format!("{name}-pvc")),
        },
        storage_generation: StorageGeneration::new(format!("{name}-storage")),
    }
}

fn store_identity(
    resource_uid: ResourceUid,
    identity: ReplicaIdentity,
    name: &str,
) -> StorageIdentity {
    StorageIdentity {
        schema_version: SCHEMA_VERSION,
        resource_uid,
        pod_uid: PodUid::new(format!("{name}-pod")),
        pvc_uid: PvcUid::new(format!("{name}-pvc")),
        initialization_id: InitializationId::new(format!("{name}-initialization")),
        local_identity: identity,
        effective_policy: EffectivePolicy::fixed(2, 30).unwrap(),
    }
}

fn intent(
    preview: &PublicOperationPreviewIdentity,
    session: kuberic_runtime::protocol::types::ProcessSessionId,
    revision: u64,
    name: &str,
    program: PublicOperationProgram,
) -> PublicOperationIntent {
    let class = match &program {
        PublicOperationProgram::Build(build) => PublicOperationClass::Build {
            target: build.replica.replica_id,
        },
        PublicOperationProgram::Remove(build) => PublicOperationClass::Remove {
            target: build.replica.replica_id,
        },
        _ => PublicOperationClass::Authority,
    };
    PublicOperationIntent {
        preview: preview.clone(),
        operation_id: OperationId::new(name),
        revision,
        process_session_id: session,
        class,
        input_digest: program.digest(),
        replay_disposition: program.replay_disposition(),
        lifecycle: None,
        program: Some(program),
    }
}

async fn execute(server: &SqliteServer, sql: &str) -> i64 {
    server
        .execute(tonic::Request::new(proto::ExecuteRequest {
            sql: sql.into(),
            params: Vec::new(),
        }))
        .await
        .unwrap()
        .into_inner()
        .lsn
}

#[tokio::test]
async fn v2_data_loss_callback_reports_unchanged_and_preserves_sql_and_progress() {
    let root = scratch();
    let pod = SqlitePod::singleton(root.path().join("replica")).await;
    pod.execute("CREATE TABLE data(id INTEGER)").await.unwrap();
    pod.execute("INSERT INTO data VALUES(1)").await.unwrap();
    let before = pod.application.persistence().progress().unwrap();
    let snapshot = pod
        .application
        .persistence()
        .snapshot(before.committed_lsn)
        .unwrap();
    assert!(
        !pod.runtime
            .primary_replicator()
            .await
            .unwrap()
            .on_data_loss()
            .await
            .unwrap()
    );
    assert_eq!(pod.application.persistence().progress().unwrap(), before);
    assert_eq!(
        pod.application
            .persistence()
            .snapshot(before.committed_lsn)
            .unwrap(),
        snapshot
    );
    assert_eq!(pod.count().await, 1);
}

#[tokio::test]
async fn sqlite_fresh_restart_build_and_corrective_empty_replacement() {
    tokio::time::timeout(Duration::from_secs(180), async {
        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("target")
            .join("sqlite-public-v1-preview")
            .join(std::process::id().to_string());
        let _ = std::fs::remove_dir_all(&root);
        let source_root = root.join("source");
        let replacement_root = root.join("replacement");
        let retired_root = root.join("retired-higher-progress");
        std::fs::create_dir_all(&source_root).unwrap();
        std::fs::create_dir_all(&replacement_root).unwrap();

        let retired = SqlitePod::singleton(retired_root.clone()).await;
        retired
            .execute("CREATE TABLE stale(id INTEGER PRIMARY KEY, value TEXT)")
            .await
            .unwrap();
        for id in 1..=8 {
            retired
                .execute(&format!("INSERT INTO stale VALUES({id},'must-not-return')"))
                .await
                .unwrap();
        }
        let retired_progress = retired.application.persistence().progress().unwrap();
        drop(retired);

        let resource_uid = ResourceUid::new("sqlite-public-v1-preview");
        let preview = PublicOperationPreviewIdentity::public_v1(52);
        let binding = PreviewLifecycleBinding {
            preview: preview.clone(),
            resource_uid: resource_uid.clone(),
            spec_generation: 1,
            state_persistence: StatePersistence::Persisted,
        };
        let source_identity = identity(1, "sqlite-source");
        let replacement_identity = identity(2, "sqlite-replacement");
        let source_storage = storage(source_identity.clone(), "sqlite-source");
        let replacement_storage = storage(replacement_identity.clone(), "sqlite-replacement");
        for (path, identity, name) in [
            (&source_root, source_identity.clone(), "sqlite-source"),
            (
                &replacement_root,
                replacement_identity.clone(),
                "sqlite-replacement",
            ),
        ] {
            drop(
                SqliteStore::create_public_v1(
                    SqliteStore::metadata_database_path(path),
                    preview.clone(),
                    store_identity(resource_uid.clone(), identity, name),
                )
                .unwrap(),
            );
        }
        let source_address = address();
        let replacement_address = address();
        let source_persistence =
            Arc::new(SqlitePersistence::open(source_root.join("application")).unwrap());
        let replacement_persistence =
            Arc::new(SqlitePersistence::open(replacement_root.join("application")).unwrap());
        let source_service = Arc::new(
            SqliteService::new(
                source_persistence.clone(),
                format!("http://{source_address}"),
            )
            .unwrap()
            .with_stateless_preview(),
        );
        let replacement_service = Arc::new(
            SqliteService::new(
                replacement_persistence.clone(),
                format!("http://{replacement_address}"),
            )
            .unwrap()
            .with_stateless_preview(),
        );
        let source_server = SqliteServer::new(source_service.clone());
        let mut source = ReplicaHost::new(
            ReplicaProcessConfig {
                resource_uid: resource_uid.clone(),
                replica_id: ReplicaId::new(1),
                pod_uid: PodUid::new("sqlite-source-pod"),
                pvc_uid: PvcUid::new("sqlite-source-pvc"),
                data_root: source_root.clone(),
                control_address: address(),
                replication_address: source_address,
                bearer_token: "preview-token".into(),
                rpc_deadline: Duration::from_secs(2),
                transport_window_capacity: 8,
            },
            source_service.clone(),
            ApplicationStorageState::FreshEmpty,
            Arc::new(Resolver),
        )
        .with_stateless_preview(binding.clone())
        .start()
        .await
        .unwrap();
        let mut replacement = ReplicaHost::new(
            ReplicaProcessConfig {
                resource_uid: resource_uid.clone(),
                replica_id: ReplicaId::new(2),
                pod_uid: PodUid::new("sqlite-replacement-pod"),
                pvc_uid: PvcUid::new("sqlite-replacement-pvc"),
                data_root: replacement_root.clone(),
                control_address: address(),
                replication_address: replacement_address,
                bearer_token: "preview-token".into(),
                rpc_deadline: Duration::from_secs(2),
                transport_window_capacity: 8,
            },
            replacement_service.clone(),
            ApplicationStorageState::FreshEmpty,
            Arc::new(Resolver),
        )
        .with_stateless_preview(binding.clone())
        .start()
        .await
        .unwrap();
        let source_handle = source.handle();
        let replacement_handle = replacement.handle();
        let generation = PublicProtocolGeneration::new(52);
        let initialization = derive_history_initialization_id(&resource_uid, generation);
        let history = derive_bootstrap_history(&resource_uid, generation, &initialization);
        source_handle
            .initialize_stateless_preview_history(&initialization, &history, &source_storage)
            .await
            .unwrap();
        replacement_handle
            .initialize_stateless_preview_history(&initialization, &history, &replacement_storage)
            .await
            .unwrap();
        let epoch = Epoch::new(0, 1);
        let source_configuration = ConfigurationDescriptor::new(
            epoch,
            source_identity.replica_id,
            vec![ConfigurationMember {
                identity: source_identity.clone(),
                role: ReplicaRole::Primary,
            }],
            1,
        );
        for (revision, name, program) in [
            (
                1,
                "sqlite-source-open",
                PublicOperationProgram::Open {
                    replica: source_identity.clone(),
                    existing: true,
                },
            ),
            (
                2,
                "sqlite-source-role",
                PublicOperationProgram::Role {
                    epoch,
                    role: ReplicaRole::Primary,
                },
            ),
            (
                3,
                "sqlite-source-epoch",
                PublicOperationProgram::Epoch { epoch },
            ),
            (
                4,
                "sqlite-source-configuration",
                PublicOperationProgram::Configuration(PublicConfiguration {
                    current: source_configuration,
                    previous: None,
                    replicas: Vec::new(),
                }),
            ),
        ] {
            source_handle
                .execute_stateless_preview(intent(
                    &preview,
                    source_handle.process_session_id(),
                    revision,
                    name,
                    program,
                ))
                .await
                .unwrap();
        }
        for (revision, name, program) in [
            (
                1,
                "sqlite-replacement-open",
                PublicOperationProgram::Open {
                    replica: replacement_identity.clone(),
                    existing: true,
                },
            ),
            (
                2,
                "sqlite-replacement-role",
                PublicOperationProgram::Role {
                    epoch,
                    role: ReplicaRole::IdleSecondary,
                },
            ),
            (
                3,
                "sqlite-replacement-epoch",
                PublicOperationProgram::Epoch { epoch },
            ),
        ] {
            replacement_handle
                .execute_stateless_preview(intent(
                    &preview,
                    replacement_handle.process_session_id(),
                    revision,
                    name,
                    program,
                ))
                .await
                .unwrap();
        }

        assert_eq!(
            execute(
                &source_server,
                "CREATE TABLE data(id INTEGER PRIMARY KEY, value TEXT, padding BLOB)"
            )
            .await,
            1
        );
        assert_eq!(
            execute(
                &source_server,
                "INSERT INTO data VALUES(1,'snapshot',zeroblob(524288))"
            )
            .await,
            2
        );
        let build_input = PublicBuildInput {
            attempt: OperationId::new("sqlite-corrective-build"),
            epoch,
            replica: replacement_identity.clone(),
            target_storage: replacement_storage.clone(),
            process_session_id: replacement_handle.process_session_id(),
            replication_address: format!("http://{replacement_address}"),
            history: Some(history.clone()),
        };
        source_service.copy_snapshot_gate().arm();
        let mut build = tokio::spawn({
            let handle = source_handle.clone();
            let preview = preview.clone();
            let input = build_input.clone();
            async move {
                handle
                    .execute_stateless_preview(intent(
                        &preview,
                        handle.process_session_id(),
                        4,
                        "sqlite-corrective-build",
                        PublicOperationProgram::Build(input),
                    ))
                    .await
            }
        });
        source_service.copy_snapshot_gate().wait_entered().await;
        assert!(
            !build.is_finished(),
            "large SQLite copy finished before the retained write"
        );
        assert_eq!(
            execute(
                &source_server,
                "INSERT INTO data VALUES(2,'retained',zeroblob(1))"
            )
            .await,
            3
        );
        source_service.copy_snapshot_gate().release();
        let build_result = tokio::time::timeout(Duration::from_secs(30), &mut build)
            .await
            .unwrap()
            .unwrap();
        if let Err(error) = build_result {
            panic!(
                "SQLite build failed: {error}; target={:?}",
                replacement_persistence.inspect_for_test()
            );
        }
        assert_eq!(replacement_persistence.progress().unwrap().committed_lsn, 3);
        let inspection = replacement_persistence.inspect_for_test().unwrap();
        let inspection_path = root.join("replacement-inspection.sqlite");
        std::fs::write(&inspection_path, &inspection.image).unwrap();
        let database = rusqlite::Connection::open(&inspection_path).unwrap();
        let rows = database
            .query_row("SELECT count(*) FROM data", [], |row| row.get::<_, i64>(0))
            .unwrap();
        assert_eq!(rows, 2);
        drop(database);

        let admitted = ConfigurationDescriptor::new(
            epoch,
            source_identity.replica_id,
            vec![
                ConfigurationMember {
                    identity: source_identity.clone(),
                    role: ReplicaRole::Primary,
                },
                ConfigurationMember {
                    identity: replacement_identity.clone(),
                    role: ReplicaRole::ActiveSecondary,
                },
            ],
            2,
        );
        replacement_handle
            .execute_stateless_preview(intent(
                &preview,
                replacement_handle.process_session_id(),
                4,
                "sqlite-replacement-role-active",
                PublicOperationProgram::Role {
                    epoch,
                    role: ReplicaRole::ActiveSecondary,
                },
            ))
            .await
            .unwrap();
        replacement_handle
            .execute_stateless_preview(intent(
                &preview,
                replacement_handle.process_session_id(),
                5,
                "sqlite-replacement-configuration",
                PublicOperationProgram::Configuration(PublicConfiguration {
                    current: admitted.clone(),
                    previous: None,
                    replicas: vec![PublicReplicaDescriptor {
                        build_id: OperationId::default(),
                        identity: source_identity.clone(),
                        replication_address: format!("http://{source_address}"),
                        process_session_id: source_handle.process_session_id(),
                        role: ReplicaRole::Primary,
                        current_progress: 3,
                        catch_up_capability: 3,
                        must_catchup: false,
                    }],
                }),
            ))
            .await
            .unwrap();
        source_handle
            .execute_stateless_preview(intent(
                &preview,
                source_handle.process_session_id(),
                5,
                "sqlite-source-configuration-admit",
                PublicOperationProgram::Configuration(PublicConfiguration {
                    current: admitted,
                    previous: None,
                    replicas: vec![PublicReplicaDescriptor {
                        build_id: OperationId::default(),
                        identity: replacement_identity.clone(),
                        replication_address: format!("http://{replacement_address}"),
                        process_session_id: replacement_handle.process_session_id(),
                        role: ReplicaRole::ActiveSecondary,
                        current_progress: 3,
                        catch_up_capability: 3,
                        must_catchup: false,
                    }],
                }),
            ))
            .await
            .unwrap();
        assert_eq!(
            execute(
                &source_server,
                "INSERT INTO data VALUES(3,'after-build',zeroblob(1))"
            )
            .await,
            4
        );
        assert_eq!(replacement_persistence.progress().unwrap().committed_lsn, 4);
        let inspection = replacement_persistence.inspect_for_test().unwrap();
        std::fs::write(&inspection_path, &inspection.image).unwrap();
        let database = rusqlite::Connection::open(&inspection_path).unwrap();
        assert_eq!(
            database
                .query_row("SELECT count(*) FROM data", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            3
        );
        drop(database);

        source_handle
            .execute_stateless_preview(intent(
                &preview,
                source_handle.process_session_id(),
                5,
                "sqlite-retire-built-incarnation",
                PublicOperationProgram::Remove(build_input),
            ))
            .await
            .unwrap();
        source_handle
            .execute_stateless_preview(intent(
                &preview,
                source_handle.process_session_id(),
                6,
                "sqlite-exclude-retired-incarnation",
                PublicOperationProgram::Configuration(PublicConfiguration {
                    current: ConfigurationDescriptor::new(
                        epoch,
                        source_identity.replica_id,
                        vec![ConfigurationMember {
                            identity: source_identity.clone(),
                            role: ReplicaRole::Primary,
                        }],
                        1,
                    ),
                    previous: None,
                    replicas: Vec::new(),
                }),
            ))
            .await
            .unwrap();

        let predecessor = source_handle
            .stateless_preview_process_endpoint(ReplicaRole::Primary)
            .await
            .unwrap();
        let predecessor_session = predecessor.process_session_id.clone();
        source.shutdown();
        source.wait().await.unwrap();
        let restarted_address = address();
        let restarted_persistence =
            Arc::new(SqlitePersistence::open(source_root.join("application")).unwrap());
        let restarted_service = Arc::new(
            SqliteService::new(
                restarted_persistence.clone(),
                format!("http://{restarted_address}"),
            )
            .unwrap()
            .with_stateless_preview(),
        );
        let restarted_server = SqliteServer::new(restarted_service.clone());
        let mut restarted = ReplicaHost::new(
            ReplicaProcessConfig {
                resource_uid: resource_uid.clone(),
                replica_id: ReplicaId::new(1),
                pod_uid: PodUid::new("sqlite-source-pod"),
                pvc_uid: PvcUid::new("sqlite-source-pvc"),
                data_root: source_root.clone(),
                control_address: address(),
                replication_address: restarted_address,
                bearer_token: "preview-token".into(),
                rpc_deadline: Duration::from_secs(2),
                transport_window_capacity: 8,
            },
            restarted_service,
            ApplicationStorageState::Established,
            Arc::new(Resolver),
        )
        .with_stateless_preview(binding)
        .start()
        .await
        .unwrap();
        let restarted_handle = restarted.handle();
        let renewal_epoch = Epoch::new(0, 2);
        let renewal_configuration = ConfigurationDescriptor::new(
            renewal_epoch,
            source_identity.replica_id,
            vec![ConfigurationMember {
                identity: source_identity.clone(),
                role: ReplicaRole::Primary,
            }],
            1,
        );
        restarted_handle
            .authorize_stateless_preview_session_renewal(ProcessSessionRenewalRequired {
                operation_id: OperationId::new("sqlite-source-restart"),
                reason: ProcessSessionRenewalReason::PrimaryReplacement,
                resource_uid: resource_uid.clone(),
                history: history.clone(),
                previous_epoch: epoch,
                renewal_epoch,
                predecessor,
                replacement: Some(ProcessSessionEndpoint {
                    identity: source_identity.clone(),
                    storage_incarnation: source_storage.clone(),
                    process_session_id: restarted_handle.process_session_id(),
                    replicator_creation_id: ReplicatorCreationId::new(
                        "sqlite-source-restart-creation",
                    ),
                    role: ReplicaRole::None,
                }),
                surviving_sessions: Vec::new(),
                fresh_configuration: renewal_configuration.clone(),
                predecessor_contained: true,
                data_loss: None,
            })
            .await
            .unwrap();
        for (revision, name, program) in [
            (
                7,
                "sqlite-restart-open",
                PublicOperationProgram::Open {
                    replica: source_identity.clone(),
                    existing: true,
                },
            ),
            (
                8,
                "sqlite-restart-role",
                PublicOperationProgram::Role {
                    epoch: renewal_epoch,
                    role: ReplicaRole::Primary,
                },
            ),
            (
                9,
                "sqlite-restart-epoch",
                PublicOperationProgram::Epoch {
                    epoch: renewal_epoch,
                },
            ),
            (
                10,
                "sqlite-restart-configuration",
                PublicOperationProgram::Configuration(PublicConfiguration {
                    current: renewal_configuration,
                    previous: None,
                    replicas: Vec::new(),
                }),
            ),
        ] {
            restarted_handle
                .execute_stateless_preview(intent(
                    &preview,
                    restarted_handle.process_session_id(),
                    revision,
                    name,
                    program,
                ))
                .await
                .unwrap();
        }
        let query = restarted_server
            .query(tonic::Request::new(proto::QueryRequest {
                sql: "SELECT value FROM data ORDER BY id".into(),
                params: Vec::new(),
            }))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(query.rows.len(), 3);
        assert_eq!(
            execute(
                &restarted_server,
                "INSERT INTO data VALUES(4,'after-restart',zeroblob(1))"
            )
            .await,
            5
        );
        let before = restarted_persistence.progress().unwrap();
        assert!(
            restarted_handle
                .execute_stateless_preview(intent(
                    &preview,
                    predecessor_session,
                    11,
                    "sqlite-predecessor-session-progress",
                    PublicOperationProgram::Progress { capability: false },
                ))
                .await
                .is_err()
        );
        assert_eq!(restarted_persistence.progress().unwrap(), before);

        restarted.shutdown();
        replacement.shutdown();
        restarted.wait().await.unwrap();
        replacement.wait().await.unwrap();
        assert_eq!(
            SqlitePersistence::open(retired_root.join("application"))
                .unwrap()
                .progress()
                .unwrap(),
            retired_progress
        );
        std::fs::remove_dir_all(root).unwrap();
    })
    .await
    .unwrap();
}
