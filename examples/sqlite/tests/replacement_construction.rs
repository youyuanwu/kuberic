use std::sync::Arc;
use std::time::Duration;

use futures::TryStreamExt;
use kuberic_runtime::application::CopyChunk;
use kuberic_runtime::engine::DurableState;
use kuberic_runtime::host::{
    ApplicationStorageState, ReplicaEndpointResolver, ReplicaHost, ReplicaProcessConfig,
};
use kuberic_runtime::protocol::public_operations::{
    ProcessSessionRenewalRequired, PublicBuildAttemptReference, PublicBuildInput,
    PublicCatchUpMode, PublicConfiguration, PublicInboundBuildAuthority, PublicOperationClass,
    PublicOperationIntent, PublicOperationProgram, PublicReplicaDescriptor, PublicV1Identity,
};
use kuberic_runtime::protocol::types::{
    AccessStatus, AgentGeneration, ConfigurationDescriptor, ConfigurationMember, EffectivePolicy,
    Epoch, InitializationId, OperationId, PodUid, ProcessSessionEndpoint,
    ProcessSessionRenewalReason, PublicProtocolGeneration, PvcUid, ReplicaId, ReplicaIdentity,
    ReplicaInstanceId, ReplicaRole, ReplicaStorageIdentity, ReplicaStorageIncarnationContext,
    ReplicatorCreationId, ResourceUid, StorageGeneration, derive_bootstrap_history,
    derive_history_initialization_id,
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
    preview: &PublicV1Identity,
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
        identity: preview.clone(),
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
        .unwrap_or_else(|error| panic!("SQLite execute failed for {sql}: {error}"))
        .into_inner()
        .lsn
}

fn values(database: &rusqlite::Connection) -> Vec<String> {
    database
        .prepare("SELECT value FROM data ORDER BY id")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap()
}

async fn seed_retired_provider(
    root: &std::path::Path,
) -> kuberic_runtime::application::DurableApplicationProgress {
    let resource_uid = ResourceUid::new("sqlite-retired-seed");
    let preview = PublicV1Identity::new(1);
    let local_identity = identity(2, "sqlite-retired-seed");
    let local_storage = storage(local_identity.clone(), "sqlite-retired-seed");
    drop(
        SqliteStore::create_public_v1(
            SqliteStore::metadata_database_path(root),
            preview.clone(),
            store_identity(
                resource_uid.clone(),
                local_identity.clone(),
                "sqlite-retired-seed",
            ),
        )
        .unwrap(),
    );
    let replication_address = address();
    let persistence = Arc::new(SqlitePersistence::open(root.join("application")).unwrap());
    let service = Arc::new(
        SqliteService::new(persistence.clone(), format!("http://{replication_address}")).unwrap(),
    );
    let server = SqliteServer::new(service.clone());
    let mut running = ReplicaHost::new(
        ReplicaProcessConfig {
            resource_uid: resource_uid.clone(),
            replica_id: local_identity.replica_id,
            pod_uid: PodUid::new("sqlite-retired-seed-pod"),
            pvc_uid: PvcUid::new("sqlite-retired-seed-pvc"),
            data_root: root.to_path_buf(),
            replication_address,
            bearer_token: "unused".into(),
            rpc_deadline: Duration::from_secs(2),
            transport_window_capacity: 8,
        },
        service,
        ApplicationStorageState::FreshEmpty,
        Arc::new(Resolver),
    )
    .start(tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap())
    .await
    .unwrap();
    let handle = running.handle();
    let generation = PublicProtocolGeneration::new(1);
    let initialization = derive_history_initialization_id(&resource_uid, generation);
    let history = derive_bootstrap_history(&resource_uid, generation, &initialization);
    handle
        .initialize_public_v1_history(&initialization, &history, &local_storage)
        .await
        .unwrap();
    let epoch = Epoch::new(0, 1);
    let configuration = ConfigurationDescriptor::new(
        epoch,
        local_identity.replica_id,
        vec![ConfigurationMember {
            identity: local_identity.clone(),
            role: ReplicaRole::Primary,
        }],
        1,
    );
    for (revision, name, program) in [
        (
            1,
            "sqlite-retired-seed-open",
            PublicOperationProgram::Open {
                replica: local_identity.clone(),
                existing: true,
            },
        ),
        (
            2,
            "sqlite-retired-seed-role",
            PublicOperationProgram::Role {
                epoch,
                role: ReplicaRole::Primary,
            },
        ),
        (
            3,
            "sqlite-retired-seed-epoch",
            PublicOperationProgram::Epoch { epoch },
        ),
        (
            4,
            "sqlite-retired-seed-configuration",
            PublicOperationProgram::Configuration(PublicConfiguration {
                current: configuration,
                previous: None,
                replicas: Vec::new(),
            }),
        ),
    ] {
        handle
            .execute_public_v1(intent(
                &preview,
                handle.process_session_id(),
                revision,
                name,
                program,
            ))
            .await
            .unwrap();
    }
    assert_eq!(
        execute(
            &server,
            "CREATE TABLE stale(id INTEGER PRIMARY KEY, value TEXT)"
        )
        .await,
        1
    );
    for id in 1..=8 {
        assert_eq!(
            execute(
                &server,
                &format!("INSERT INTO stale VALUES({id},'must-not-return')")
            )
            .await,
            id + 1
        );
    }
    let progress = persistence.progress().unwrap();
    running.shutdown();
    running.wait().await.unwrap();
    drop(running);
    drop(server);
    drop(persistence);
    std::fs::remove_dir_all(root.join(".kuberic")).unwrap();
    progress
}

#[tokio::test]
async fn sqlite_operation_stream_copy_preserves_verified_uncommitted_suffix() {
    use kuberic_runtime::application::DurableApplicationProgress;
    use kuberic_runtime::control::proto as wire;
    use kuberic_runtime::replicator::stream::{OperationMetadata, StreamOperation};
    use sqlite_replicated::state::PersistenceFault;

    async fn send(
        client: &mut wire::replication_data_client::ReplicationDataClient<
            tonic::transport::Channel,
        >,
        frame: wire::CopyItem,
    ) -> tonic::Streaming<wire::CopyAck> {
        client
            .build(tonic::Request::new(futures::stream::iter([frame])))
            .await
            .unwrap()
            .into_inner()
    }

    async fn delivered(
        operations: &mut tokio::sync::mpsc::UnboundedReceiver<StreamOperation>,
    ) -> StreamOperation {
        tokio::time::timeout(Duration::from_secs(5), operations.recv())
            .await
            .unwrap()
            .unwrap()
    }

    async fn pending(response: &mut tonic::Streaming<wire::CopyAck>) {
        assert!(
            tokio::time::timeout(Duration::from_millis(40), response.message())
                .await
                .is_err(),
            "build completed before explicit durable service ACK"
        );
    }

    tokio::time::timeout(Duration::from_secs(90), async {
        for (last_lsn, first_retained_commitment) in [(2, 2), (3, 2), (3, 1)] {
            let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .parent()
                .unwrap()
                .parent()
                .unwrap()
                .join("target/sqlite-stream-retained-copy")
                .join(format!(
                    "{}-{last_lsn}-{first_retained_commitment}",
                    std::process::id()
                ));
            let _ = std::fs::remove_dir_all(&root);
            let resource = ResourceUid::new("sqlite-stream-retained-copy");
            let preview = PublicV1Identity::new(1);
            let source_identity = identity(1, "stream-source");
            let target_identity = identity(2, "stream-target");
            let source_storage = storage(source_identity.clone(), "stream-source");
            let target_storage = storage(target_identity.clone(), "stream-target");
            let source_address = address();
            let target_address = address();
            let source_state =
                Arc::new(SqlitePersistence::open(root.join("source/application")).unwrap());
            let target_state =
                Arc::new(SqlitePersistence::open(root.join("target/application")).unwrap());
            let source_service = Arc::new(
                SqliteService::new(source_state.clone(), format!("http://{source_address}"))
                    .unwrap(),
            );
            let target_service = Arc::new(
                SqliteService::new(target_state.clone(), format!("http://{target_address}"))
                    .unwrap(),
            );
            let mut operations = target_service.control_stream_operations_for_test();
            let mut running = Vec::new();
            for (name, local, service, replication_address) in [
                (
                    "stream-source",
                    source_identity.clone(),
                    source_service.clone(),
                    source_address,
                ),
                (
                    "stream-target",
                    target_identity.clone(),
                    target_service.clone(),
                    target_address,
                ),
            ] {
                let data_root = root.join(if name == "stream-source" {
                    "source"
                } else {
                    "target"
                });
                drop(
                    SqliteStore::create_public_v1(
                        SqliteStore::metadata_database_path(&data_root),
                        preview.clone(),
                        store_identity(resource.clone(), local.clone(), name),
                    )
                    .unwrap(),
                );
                running.push(
                    ReplicaHost::new(
                        ReplicaProcessConfig {
                            resource_uid: resource.clone(),
                            replica_id: local.replica_id,
                            pod_uid: PodUid::new(format!("{name}-pod")),
                            pvc_uid: PvcUid::new(format!("{name}-pvc")),
                            data_root,
                            replication_address,
                            bearer_token: "unused".into(),
                            rpc_deadline: Duration::from_secs(2),
                            transport_window_capacity: 8,
                        },
                        service,
                        ApplicationStorageState::FreshEmpty,
                        Arc::new(Resolver),
                    )
                    .start(tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap())
                    .await
                    .unwrap(),
                );
            }
            let source = running[0].handle();
            let target = running[1].handle();
            let generation = PublicProtocolGeneration::new(1);
            let initialization = derive_history_initialization_id(&resource, generation);
            let history = derive_bootstrap_history(&resource, generation, &initialization);
            for (handle, local_storage) in [(&source, &source_storage), (&target, &target_storage)]
            {
                handle
                    .initialize_public_v1_history(&initialization, &history, local_storage)
                    .await
                    .unwrap();
            }
            let epoch = Epoch::new(0, 1);
            let configuration = ConfigurationDescriptor::new(
                epoch,
                source_identity.replica_id,
                vec![ConfigurationMember {
                    identity: source_identity.clone(),
                    role: ReplicaRole::Primary,
                }],
                1,
            );
            for (revision, program) in [
                (
                    1,
                    PublicOperationProgram::Open {
                        replica: source_identity.clone(),
                        existing: true,
                    },
                ),
                (
                    2,
                    PublicOperationProgram::Role {
                        epoch,
                        role: ReplicaRole::Primary,
                    },
                ),
                (3, PublicOperationProgram::Epoch { epoch }),
            ] {
                source
                    .execute_public_v1(intent(
                        &preview,
                        source.process_session_id(),
                        revision,
                        &format!("stream-open-{revision}"),
                        program,
                    ))
                    .await
                    .unwrap();
            }
            source
                .execute_public_v1(intent(
                    &preview,
                    source.process_session_id(),
                    4,
                    "stream-source-configuration",
                    PublicOperationProgram::Configuration(PublicConfiguration {
                        current: configuration.clone(),
                        previous: None,
                        replicas: vec![],
                    }),
                ))
                .await
                .unwrap();
            let server = SqliteServer::new(source_service.clone());
            assert_eq!(
                execute(
                    &server,
                    "CREATE TABLE data(id INTEGER PRIMARY KEY, value TEXT)"
                )
                .await,
                1
            );
            assert_eq!(
                execute(&server, "INSERT INTO data VALUES(1,'retained')").await,
                2
            );
            if last_lsn == 3 {
                assert_eq!(
                    execute(&server, "INSERT INTO data VALUES(2,'later-retained')").await,
                    3
                );
            }
            let retained = source_state
                .get_replication_operations(1, last_lsn)
                .await
                .unwrap()
                .try_collect::<Vec<_>>()
                .await
                .unwrap();
            target_state.apply(retained[0].clone()).await.unwrap();
            target_state.commit(1).await.unwrap();
            for operation in &retained[1..] {
                let mut operation = operation.clone();
                operation.committed_lsn = 1;
                target_state.apply(operation).await.unwrap();
            }
            let seeded_suffix = target_state
                .get_replication_operations(2, last_lsn)
                .await
                .unwrap()
                .try_collect::<Vec<_>>()
                .await
                .unwrap();
            let suffix_progress = DurableApplicationProgress {
                applied_lsn: last_lsn,
                committed_lsn: 1,
            };
            assert_eq!(target_state.progress().unwrap(), suffix_progress);
            assert_eq!(target_state.stream_calls_for_test(), (last_lsn as u64, 1));
            assert_eq!(
                source_state.snapshot(1).unwrap(),
                target_state.snapshot(1).unwrap()
            );
            for (revision, program) in [
                (
                    1,
                    PublicOperationProgram::Open {
                        replica: target_identity.clone(),
                        existing: true,
                    },
                ),
                (
                    2,
                    PublicOperationProgram::Role {
                        epoch,
                        role: ReplicaRole::IdleSecondary,
                    },
                ),
                (3, PublicOperationProgram::Epoch { epoch }),
            ] {
                target
                    .execute_public_v1(intent(
                        &preview,
                        target.process_session_id(),
                        revision,
                        &format!("stream-open-{revision}"),
                        program,
                    ))
                    .await
                    .unwrap();
            }
            let authority = PublicInboundBuildAuthority {
                operation_id: OperationId::new("sqlite-stream-build"),
                operation_revision: 1,
                attempt: PublicBuildAttemptReference {
                    resource_uid: resource,
                    epoch,
                    build_id: OperationId::new("sqlite-stream-build"),
                    source: source_identity,
                    source_storage,
                    source_session: source.process_session_id(),
                    target: target_identity,
                    target_storage,
                    target_session: target.process_session_id(),
                },
                history,
                current_configuration_id: configuration.configuration_id,
                target_address: format!("http://{target_address}"),
            };
            target
                .install_public_v1_inbound_build_authority(&authority)
                .await
                .unwrap();
            let terminal_receipt = || {
                let database = rusqlite::Connection::open(SqliteStore::metadata_database_path(
                    &root.join("target"),
                ))
                .unwrap();
                let json: String = database
                    .query_row(
                        "SELECT state_json FROM agent_state WHERE singleton = 1",
                        [],
                        |row| row.get(0),
                    )
                    .unwrap();
                let state: serde_json::Value = serde_json::from_str(&json).unwrap();
                state["publicV1OperationState"]["terminalInboundBuildReceipts"]
                    .get(authority.attempt.build_id.as_str())
                    .cloned()
            };
            let wire_storage = |storage: &ReplicaStorageIncarnationContext| {
                let ReplicaStorageIdentity::Persistent { pod_uid, pvc_uid } = &storage.storage
                else {
                    unreachable!()
                };
                wire::PublicV1StorageIncarnation {
                    replica: Some(storage.replica.clone().into()),
                    kind: wire::PublicV1StorageKind::Persistent as i32,
                    pod_uid: pod_uid.to_string(),
                    pvc_uid: pvc_uid.to_string(),
                    volatile_identity: String::new(),
                    storage_generation: storage.storage_generation.to_string(),
                }
            };
            let frame = |sequence, snapshot_chunk, final_item, lsn, data| wire::CopyItem {
                protocol_version: 11,
                build_id: authority.attempt.build_id.to_string(),
                sender: Some(authority.attempt.source.clone().into()),
                receiver: Some(authority.attempt.target.clone().into()),
                epoch: Some(epoch.into()),
                current_configuration_id: authority.current_configuration_id.to_string(),
                sequence,
                lsn,
                committed_lsn: lsn,
                replication_boundary_lsn: 1,
                final_item,
                data,
                snapshot_chunk,
                sender_session_id: authority.attempt.source_session.to_string(),
                receiver_session_id: authority.attempt.target_session.to_string(),
                catch_up_boundary_lsn: final_item.then_some(lsn),
                source_storage: Some(wire_storage(&authority.attempt.source_storage)),
                target_storage: Some(wire_storage(&authority.attempt.target_storage)),
                history: Some(wire::PublicV1HistoryContext {
                    resource_uid: authority.history.resource_uid.to_string(),
                    protocol_generation: authority.history.protocol_generation.value(),
                    history_id: authority.history.history_id.to_string(),
                    data_loss_number: authority.history.data_loss_number,
                }),
                target_address: authority.target_address.clone(),
            };
            let mut client = wire::replication_data_client::ReplicationDataClient::connect(
                format!("http://{target_address}"),
            )
            .await
            .unwrap();
            let mut snapshot = send(
                &mut client,
                frame(0, true, false, 1, source_state.snapshot(1).unwrap()),
            )
            .await;
            let operation = delivered(&mut operations).await;
            assert!(matches!(
                operation.metadata,
                OperationMetadata::Copy { sequence: 0, .. }
            ));
            let progress = operation.apply_to(target_state.as_ref()).await.unwrap();
            assert_eq!(progress, suffix_progress);
            operation.acknowledge(progress).unwrap();
            assert_eq!(snapshot.message().await.unwrap().unwrap().durable_lsn, 1);

            let complete_frame = frame(1, false, false, 1, vec![]);
            let mut failed = send(&mut client, complete_frame.clone()).await;
            let complete = delivered(&mut operations).await;
            target_state.fail_once(PersistenceFault::AfterCopyInstall);
            let error = complete.apply_to(target_state.as_ref()).await.unwrap_err();
            complete.reject(error).unwrap();
            assert!(failed.message().await.is_err());
            assert_eq!(target_state.inspect_for_test().unwrap().base_lsn, 1);
            assert_eq!(target_state.progress().unwrap(), suffix_progress);

            let mut completion = send(&mut client, complete_frame.clone()).await;
            let complete = delivered(&mut operations).await;
            assert_eq!(
                complete.metadata,
                OperationMetadata::CopyComplete {
                    build_id: authority.attempt.build_id.clone(),
                    up_to_lsn: 1,
                    committed_lsn: 1,
                }
            );
            pending(&mut completion).await;
            let progress = complete.apply_to(target_state.as_ref()).await.unwrap();
            assert_eq!(progress, suffix_progress);
            pending(&mut completion).await;
            complete.acknowledge(progress).unwrap();
            assert_eq!(completion.message().await.unwrap().unwrap().durable_lsn, 1);
            let mut retry = send(&mut client, complete_frame.clone()).await;
            assert_eq!(retry.message().await.unwrap().unwrap().durable_lsn, 1);
            assert!(operations.try_recv().is_err());
            let mut unacknowledged_replay = send(
                &mut client,
                frame(
                    1,
                    false,
                    false,
                    last_lsn,
                    retained.last().unwrap().data.to_vec(),
                ),
            )
            .await;
            assert!(unacknowledged_replay.message().await.is_err());
            assert!(operations.try_recv().is_err());

            for retained_operation in &retained[1..] {
                let lsn = retained_operation.lsn;
                let committed_lsn = if lsn == 2 {
                    first_retained_commitment
                } else {
                    lsn
                };
                let mut retained_frame = frame(
                    lsn as u64,
                    false,
                    false,
                    lsn,
                    retained_operation.data.to_vec(),
                );
                retained_frame.committed_lsn = committed_lsn;
                let expected_calls = (
                    last_lsn as u64,
                    (lsn - i64::from(first_retained_commitment == 1)) as u64,
                );
                if lsn == 3 {
                    let mut failed = send(&mut client, retained_frame.clone()).await;
                    let operation = delivered(&mut operations).await;
                    assert_eq!(
                        operation.metadata,
                        OperationMetadata::Replication { lsn, committed_lsn }
                    );
                    pending(&mut failed).await;
                    target_state.fail_once(PersistenceFault::AfterCommit);
                    let error = operation.apply_to(target_state.as_ref()).await.unwrap_err();
                    assert_eq!(
                        target_state.progress().unwrap(),
                        DurableApplicationProgress {
                            applied_lsn: 3,
                            committed_lsn: 3,
                        }
                    );
                    assert_eq!(target_state.stream_calls_for_test(), expected_calls);
                    operation.reject(error).unwrap();
                    assert!(failed.message().await.is_err());
                    assert!(terminal_receipt().is_none());

                    let mut old_frame = frame(2, false, false, 2, retained[1].data.to_vec());
                    old_frame.committed_lsn = first_retained_commitment;
                    for replay in [old_frame, complete_frame.clone()] {
                        let mut response = send(&mut client, replay).await;
                        assert_eq!(
                            response.message().await.unwrap().unwrap().durable_lsn,
                            first_retained_commitment,
                            "exact replay advertised rejected physical commitment"
                        );
                        assert!(operations.try_recv().is_err());
                        assert!(terminal_receipt().is_none());
                        assert_eq!(target_state.stream_calls_for_test(), expected_calls);
                    }

                    let mut premature = send(&mut client, frame(3, false, true, 2, vec![])).await;
                    assert!(
                        premature.message().await.is_err(),
                        "live final admitted a rejected physical commitment"
                    );
                    assert!(operations.try_recv().is_err());
                    assert!(terminal_receipt().is_none());
                    assert_eq!(target_state.stream_calls_for_test(), expected_calls);
                    assert!(
                        target.public_v1_report().await.unwrap().progress.first
                            <= first_retained_commitment
                    );
                    assert!(
                        target.diagnostics().await.unwrap().committed_lsn
                            <= first_retained_commitment
                    );

                    for commitment in (first_retained_commitment + 1)..=3 {
                        let mut altered = frame(2, false, false, 2, retained[1].data.to_vec());
                        altered.committed_lsn = commitment;
                        let mut rejected = send(&mut client, altered).await;
                        assert!(
                            rejected.message().await.is_err(),
                            "retained replay credited rejected physical commitment {commitment}"
                        );
                        assert!(operations.try_recv().is_err());
                        assert!(terminal_receipt().is_none());
                        assert_eq!(target_state.stream_calls_for_test(), expected_calls);
                    }
                }
                let mut response = send(&mut client, retained_frame.clone()).await;
                let operation = delivered(&mut operations).await;
                assert_eq!(
                    operation.metadata,
                    OperationMetadata::Replication { lsn, committed_lsn }
                );
                pending(&mut response).await;
                let progress = operation.apply_to(target_state.as_ref()).await.unwrap();
                assert_eq!(
                    progress,
                    DurableApplicationProgress {
                        applied_lsn: last_lsn,
                        committed_lsn
                    }
                );
                assert_eq!(target_state.stream_calls_for_test(), expected_calls);
                pending(&mut response).await;
                assert!(terminal_receipt().is_none());
                if lsn == 3 {
                    assert!(
                        target.public_v1_report().await.unwrap().progress.first
                            <= first_retained_commitment
                    );
                }
                operation.acknowledge(progress).unwrap();
                assert_eq!(
                    response.message().await.unwrap().unwrap().durable_lsn,
                    committed_lsn
                );
                let mut retry = send(&mut client, retained_frame.clone()).await;
                assert_eq!(
                    retry.message().await.unwrap().unwrap().durable_lsn,
                    committed_lsn
                );
                assert!(operations.try_recv().is_err());
                assert_eq!(target_state.stream_calls_for_test(), expected_calls);
                for changed_commitment in [false, true] {
                    let mut corrupt = retained_frame.clone();
                    if changed_commitment {
                        corrupt.committed_lsn += 1;
                    } else {
                        corrupt.data.push(0);
                    }
                    let mut rejected = send(&mut client, corrupt).await;
                    assert!(rejected.message().await.is_err());
                    assert!(operations.try_recv().is_err());
                }
            }
            let inspection = target_state.inspect_for_test().unwrap();
            assert_eq!(inspection.operations, seeded_suffix);
            assert_eq!(
                inspection.image,
                source_state.image_at_for_test(last_lsn).unwrap()
            );
            assert_eq!(
                target_state.stream_calls_for_test(),
                (
                    last_lsn as u64,
                    (last_lsn - i64::from(first_retained_commitment == 1)) as u64
                )
            );
            let final_frame = frame(last_lsn as u64 + 1, false, true, last_lsn, vec![]);
            for _ in 0..2 {
                let mut final_response = send(&mut client, final_frame.clone()).await;
                let ack = final_response.message().await.unwrap().unwrap();
                assert!(ack.final_item);
                assert_eq!(ack.durable_lsn, last_lsn);
                assert!(operations.try_recv().is_err());
                assert_eq!(terminal_receipt().unwrap()["durableLsn"], last_lsn);
                assert_eq!(
                    target.public_v1_report().await.unwrap().progress.first,
                    last_lsn
                );
            }
            let database =
                rusqlite::Connection::open(target_state.materialize_committed().unwrap()).unwrap();
            assert_eq!(
                values(&database),
                if last_lsn == 2 {
                    vec!["retained"]
                } else {
                    vec!["retained", "later-retained"]
                }
            );
            drop(database);
            for replica in &mut running {
                replica.shutdown();
                replica.wait().await.unwrap();
            }
            drop(running);
            drop(server);
            drop(source_service);
            drop(target_service);
            drop(source_state);
            drop(target_state);
            std::fs::remove_dir_all(root).unwrap();
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn sqlite_fresh_restart_build_and_corrective_empty_replacement() {
    tokio::time::timeout(Duration::from_secs(270), async {
        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("target")
            .join("sqlite-public-v1-replacement")
            .join(std::process::id().to_string());
        let _ = std::fs::remove_dir_all(&root);
        let source_root = root.join("source");
        let replacement_root = root.join("replacement");
        let retired_root = root.join("retired-higher-progress");
        std::fs::create_dir_all(&source_root).unwrap();
        std::fs::create_dir_all(&replacement_root).unwrap();

        let retired_progress = seed_retired_provider(&retired_root).await;

        let resource_uid = ResourceUid::new("sqlite-public-v1-replacement");
        let preview = PublicV1Identity::new(1);
        let source_identity = identity(1, "sqlite-source");
        let divergent_identity = identity(2, "sqlite-divergent");
        let replacement_identity = identity(2, "sqlite-replacement");
        let source_storage = storage(source_identity.clone(), "sqlite-source");
        let divergent_storage = storage(divergent_identity.clone(), "sqlite-divergent");
        let replacement_storage = storage(replacement_identity.clone(), "sqlite-replacement");
        for (path, identity, name) in [
            (&source_root, source_identity.clone(), "sqlite-source"),
            (
                &retired_root,
                divergent_identity.clone(),
                "sqlite-divergent",
            ),
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
        let divergent_address = address();
        let replacement_address = address();
        let source_persistence =
            Arc::new(SqlitePersistence::open(source_root.join("application")).unwrap());
        let replacement_persistence =
            Arc::new(SqlitePersistence::open(replacement_root.join("application")).unwrap());
        let divergent_persistence =
            Arc::new(SqlitePersistence::open(retired_root.join("application")).unwrap());
        let source_service = Arc::new(
            SqliteService::new(
                source_persistence.clone(),
                format!("http://{source_address}"),
            )
            .unwrap(),
        );
        let replacement_service = Arc::new(
            SqliteService::new(
                replacement_persistence.clone(),
                format!("http://{replacement_address}"),
            )
            .unwrap(),
        );
        let divergent_service = Arc::new(
            SqliteService::new(
                divergent_persistence.clone(),
                format!("http://{divergent_address}"),
            )
            .unwrap(),
        );
        let source_server = SqliteServer::new(source_service.clone());
        let mut source = ReplicaHost::new(
            ReplicaProcessConfig {
                resource_uid: resource_uid.clone(),
                replica_id: ReplicaId::new(1),
                pod_uid: PodUid::new("sqlite-source-pod"),
                pvc_uid: PvcUid::new("sqlite-source-pvc"),
                data_root: source_root.clone(),
                replication_address: source_address,
                bearer_token: "unused".into(),
                rpc_deadline: Duration::from_secs(2),
                transport_window_capacity: 8,
            },
            source_service.clone(),
            ApplicationStorageState::FreshEmpty,
            Arc::new(Resolver),
        )
        .start(tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap())
        .await
        .unwrap();
        let mut replacement = ReplicaHost::new(
            ReplicaProcessConfig {
                resource_uid: resource_uid.clone(),
                replica_id: ReplicaId::new(2),
                pod_uid: PodUid::new("sqlite-replacement-pod"),
                pvc_uid: PvcUid::new("sqlite-replacement-pvc"),
                data_root: replacement_root.clone(),
                replication_address: replacement_address,
                bearer_token: "unused".into(),
                rpc_deadline: Duration::from_secs(2),
                transport_window_capacity: 8,
            },
            replacement_service.clone(),
            ApplicationStorageState::FreshEmpty,
            Arc::new(Resolver),
        )
        .start(tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap())
        .await
        .unwrap();
        let mut divergent = ReplicaHost::new(
            ReplicaProcessConfig {
                resource_uid: resource_uid.clone(),
                replica_id: ReplicaId::new(2),
                pod_uid: PodUid::new("sqlite-divergent-pod"),
                pvc_uid: PvcUid::new("sqlite-divergent-pvc"),
                data_root: retired_root.clone(),
                replication_address: divergent_address,
                bearer_token: "unused".into(),
                rpc_deadline: Duration::from_secs(2),
                transport_window_capacity: 8,
            },
            divergent_service,
            ApplicationStorageState::Established,
            Arc::new(Resolver),
        )
        .start(tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap())
        .await
        .unwrap();
        let source_handle = source.handle();
        let divergent_handle = divergent.handle();
        let replacement_handle = replacement.handle();
        let generation = PublicProtocolGeneration::new(1);
        let initialization = derive_history_initialization_id(&resource_uid, generation);
        let history = derive_bootstrap_history(&resource_uid, generation, &initialization);
        source_handle
            .initialize_public_v1_history(&initialization, &history, &source_storage)
            .await
            .unwrap();
        replacement_handle
            .initialize_public_v1_history(&initialization, &history, &replacement_storage)
            .await
            .unwrap();
        divergent_handle
            .initialize_public_v1_history(&initialization, &history, &divergent_storage)
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
                    current: source_configuration.clone(),
                    previous: None,
                    replicas: Vec::new(),
                }),
            ),
        ] {
            source_handle
                .execute_public_v1(intent(
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
                "sqlite-divergent-open",
                PublicOperationProgram::Open {
                    replica: divergent_identity.clone(),
                    existing: true,
                },
            ),
            (
                2,
                "sqlite-divergent-role",
                PublicOperationProgram::Role {
                    epoch,
                    role: ReplicaRole::IdleSecondary,
                },
            ),
            (
                3,
                "sqlite-divergent-epoch",
                PublicOperationProgram::Epoch { epoch },
            ),
        ] {
            divergent_handle
                .execute_public_v1(intent(
                    &preview,
                    divergent_handle.process_session_id(),
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
                .execute_public_v1(intent(
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
        let divergent_build = PublicBuildInput {
            attempt: OperationId::new("sqlite-divergent-build"),
            epoch,
            current_configuration_id: Some(source_configuration.configuration_id.clone()),
            replica: divergent_identity.clone(),
            target_storage: divergent_storage.clone(),
            process_session_id: divergent_handle.process_session_id(),
            replication_address: format!("http://{divergent_address}"),
            history: Some(history.clone()),
        };
        divergent_handle
            .install_public_v1_inbound_build_authority(&PublicInboundBuildAuthority {
                operation_id: OperationId::new("sqlite-divergent-build"),
                operation_revision: 4,
                attempt: PublicBuildAttemptReference {
                    resource_uid: resource_uid.clone(),
                    epoch,
                    build_id: divergent_build.attempt.clone(),
                    source: source_identity.clone(),
                    source_storage: source_storage.clone(),
                    source_session: source_handle.process_session_id(),
                    target: divergent_identity.clone(),
                    target_storage: divergent_storage.clone(),
                    target_session: divergent_handle.process_session_id(),
                },
                history: history.clone(),
                current_configuration_id: source_configuration.configuration_id.clone(),
                target_address: format!("http://{divergent_address}"),
            })
            .await
            .unwrap();
        assert!(
            source_handle
                .execute_public_v1(intent(
                    &preview,
                    source_handle.process_session_id(),
                    4,
                    "sqlite-divergent-build",
                    PublicOperationProgram::Build(divergent_build.clone()),
                ))
                .await
                .is_err()
        );
        assert!(
            source_handle
                .public_v1_report()
                .await
                .unwrap()
                .completed_operations
                .iter()
                .any(|completion| {
                    completion.operation_id == divergent_build.attempt
                        && matches!(
                        completion.result,
                        kuberic_runtime::protocol::public_operations::
                            PublicV1OperationResult::RebuildRequired(_)
                    )
                })
        );
        source_handle
            .execute_public_v1(intent(
                &preview,
                source_handle.process_session_id(),
                4,
                "sqlite-retire-divergent-build",
                PublicOperationProgram::Remove(divergent_build),
            ))
            .await
            .unwrap();
        divergent.shutdown();
        divergent.wait().await.unwrap();
        assert_eq!(divergent_persistence.progress().unwrap(), retired_progress);
        let source_predecessor = source_handle
            .public_v1_process_endpoint(ReplicaRole::Primary)
            .await
            .unwrap();
        source.shutdown();
        source.wait().await.unwrap();
        let source_service = Arc::new(
            SqliteService::new(
                source_persistence.clone(),
                format!("http://{source_address}"),
            )
            .unwrap(),
        );
        let source_server = SqliteServer::new(source_service.clone());
        let mut source = ReplicaHost::new(
            ReplicaProcessConfig {
                resource_uid: resource_uid.clone(),
                replica_id: ReplicaId::new(1),
                pod_uid: PodUid::new("sqlite-source-pod"),
                pvc_uid: PvcUid::new("sqlite-source-pvc"),
                data_root: source_root.clone(),
                replication_address: source_address,
                bearer_token: "unused".into(),
                rpc_deadline: Duration::from_secs(2),
                transport_window_capacity: 8,
            },
            source_service.clone(),
            ApplicationStorageState::Established,
            Arc::new(Resolver),
        )
        .start(tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap())
        .await
        .unwrap();
        let source_handle = source.handle();
        let epoch = Epoch::new(0, 2);
        let source_configuration = ConfigurationDescriptor::new(
            epoch,
            source_identity.replica_id,
            vec![ConfigurationMember {
                identity: source_identity.clone(),
                role: ReplicaRole::Primary,
            }],
            1,
        );
        source_handle
            .authorize_public_v1_session_renewal(ProcessSessionRenewalRequired {
                operation_id: OperationId::new("sqlite-source-after-divergent-retirement"),
                reason: ProcessSessionRenewalReason::PrimaryReplacement,
                resource_uid: resource_uid.clone(),
                history: history.clone(),
                previous_epoch: Epoch::new(0, 1),
                renewal_epoch: epoch,
                predecessor: source_predecessor,
                replacement: Some(ProcessSessionEndpoint {
                    identity: source_identity.clone(),
                    storage_incarnation: source_storage.clone(),
                    process_session_id: source_handle.process_session_id(),
                    replicator_creation_id: ReplicatorCreationId::new(
                        "sqlite-source-after-divergent-retirement-creation",
                    ),
                    role: ReplicaRole::None,
                }),
                surviving_sessions: Vec::new(),
                fresh_configuration: source_configuration.clone(),
                predecessor_contained: true,
                data_loss: None,
            })
            .await
            .unwrap();
        for (revision, name, program) in [
            (
                5,
                "sqlite-source-after-divergent-open",
                PublicOperationProgram::Open {
                    replica: source_identity.clone(),
                    existing: true,
                },
            ),
            (
                6,
                "sqlite-source-after-divergent-role",
                PublicOperationProgram::Role {
                    epoch,
                    role: ReplicaRole::Primary,
                },
            ),
            (
                7,
                "sqlite-source-after-divergent-epoch",
                PublicOperationProgram::Epoch { epoch },
            ),
            (
                8,
                "sqlite-source-after-divergent-configuration",
                PublicOperationProgram::Configuration(PublicConfiguration {
                    current: source_configuration.clone(),
                    previous: None,
                    replicas: Vec::new(),
                }),
            ),
        ] {
            source_handle
                .execute_public_v1(intent(
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
                4,
                "sqlite-replacement-recovery-epoch",
                PublicOperationProgram::Epoch { epoch },
            ),
            (
                5,
                "sqlite-replacement-recovery-role",
                PublicOperationProgram::Role {
                    epoch,
                    role: ReplicaRole::IdleSecondary,
                },
            ),
        ] {
            replacement_handle
                .execute_public_v1(intent(
                    &preview,
                    replacement_handle.process_session_id(),
                    revision,
                    name,
                    program,
                ))
                .await
                .unwrap();
        }
        let build_input = PublicBuildInput {
            attempt: OperationId::new("sqlite-corrective-build"),
            epoch,
            current_configuration_id: Some(source_configuration.configuration_id.clone()),
            replica: replacement_identity.clone(),
            target_storage: replacement_storage.clone(),
            process_session_id: replacement_handle.process_session_id(),
            replication_address: format!("http://{replacement_address}"),
            history: Some(history.clone()),
        };
        replacement_handle
            .install_public_v1_inbound_build_authority(&PublicInboundBuildAuthority {
                operation_id: OperationId::new("sqlite-corrective-build"),
                operation_revision: 8,
                attempt: PublicBuildAttemptReference {
                    resource_uid: resource_uid.clone(),
                    epoch,
                    build_id: build_input.attempt.clone(),
                    source: source_identity.clone(),
                    source_storage: source_storage.clone(),
                    source_session: source_handle.process_session_id(),
                    target: replacement_identity.clone(),
                    target_storage: replacement_storage.clone(),
                    target_session: replacement_handle.process_session_id(),
                },
                history: history.clone(),
                current_configuration_id: source_configuration.configuration_id.clone(),
                target_address: format!("http://{replacement_address}"),
            })
            .await
            .unwrap();
        source_service.copy_snapshot_gate().arm();
        let mut build = tokio::spawn({
            let handle = source_handle.clone();
            let preview = preview.clone();
            let input = build_input.clone();
            async move {
                handle
                    .execute_public_v1(intent(
                        &preview,
                        handle.process_session_id(),
                        8,
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
        assert_eq!(values(&database), ["snapshot", "retained"]);
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
            .execute_public_v1(intent(
                &preview,
                replacement_handle.process_session_id(),
                6,
                "sqlite-replacement-role-active",
                PublicOperationProgram::Role {
                    epoch,
                    role: ReplicaRole::ActiveSecondary,
                },
            ))
            .await
            .unwrap();
        replacement_handle
            .execute_public_v1(intent(
                &preview,
                replacement_handle.process_session_id(),
                7,
                "sqlite-replacement-configuration",
                PublicOperationProgram::Configuration(PublicConfiguration {
                    current: admitted.clone(),
                    previous: None,
                    replicas: vec![PublicReplicaDescriptor {
                        build_id: OperationId::default(),
                        identity: source_identity.clone(),
                        replication_address: format!("http://{source_address}"),
                        process_session_id: source_handle.process_session_id(),
                        storage_incarnation: Some(source_storage.clone()),
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
            .execute_public_v1(intent(
                &preview,
                source_handle.process_session_id(),
                9,
                "sqlite-source-configuration-admit",
                PublicOperationProgram::Configuration(PublicConfiguration {
                    current: admitted,
                    previous: None,
                    replicas: vec![PublicReplicaDescriptor {
                        build_id: OperationId::default(),
                        identity: replacement_identity.clone(),
                        replication_address: format!("http://{replacement_address}"),
                        process_session_id: replacement_handle.process_session_id(),
                        storage_incarnation: Some(replacement_storage.clone()),
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
        tokio::time::timeout(Duration::from_secs(10), async {
            while replacement_persistence.progress().unwrap().committed_lsn < 4 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("post-build write did not reach the replacement provider");
        assert_eq!(replacement_persistence.progress().unwrap().committed_lsn, 4);
        let inspection = replacement_persistence.inspect_for_test().unwrap();
        std::fs::write(&inspection_path, &inspection.image).unwrap();
        let database = rusqlite::Connection::open(&inspection_path).unwrap();
        assert_eq!(values(&database), ["snapshot", "retained", "after-build"]);
        drop(database);

        let query = source_server
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
                &source_server,
                "INSERT INTO data VALUES(4,'after-restart',zeroblob(1))"
            )
            .await,
            5
        );

        let replacement_predecessor = replacement_handle
            .public_v1_process_endpoint(ReplicaRole::ActiveSecondary)
            .await
            .unwrap();
        replacement.shutdown();
        replacement.wait().await.unwrap();
        let replacement_restart_address = address();
        let replacement_restart_persistence =
            Arc::new(SqlitePersistence::open(replacement_root.join("application")).unwrap());
        let replacement_restart_service = Arc::new(
            SqliteService::new(
                replacement_restart_persistence.clone(),
                format!("http://{replacement_restart_address}"),
            )
            .unwrap(),
        );
        let mut restarted_replacement = ReplicaHost::new(
            ReplicaProcessConfig {
                resource_uid: resource_uid.clone(),
                replica_id: ReplicaId::new(2),
                pod_uid: PodUid::new("sqlite-replacement-pod"),
                pvc_uid: PvcUid::new("sqlite-replacement-pvc"),
                data_root: replacement_root.clone(),
                replication_address: replacement_restart_address,
                bearer_token: "unused".into(),
                rpc_deadline: Duration::from_secs(2),
                transport_window_capacity: 8,
            },
            replacement_restart_service,
            ApplicationStorageState::Established,
            Arc::new(Resolver),
        )
        .start(tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap())
        .await
        .unwrap();
        let replacement_restart_handle = restarted_replacement.handle();
        let replacement_restart_epoch = Epoch::new(0, 4);
        let replacement_restart_configuration = ConfigurationDescriptor::new(
            replacement_restart_epoch,
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
        replacement_restart_handle
            .authorize_public_v1_session_renewal(ProcessSessionRenewalRequired {
                operation_id: OperationId::new("sqlite-replacement-restart"),
                reason: ProcessSessionRenewalReason::ActiveSecondaryReplacement,
                resource_uid: resource_uid.clone(),
                history: history.clone(),
                previous_epoch: epoch,
                renewal_epoch: replacement_restart_epoch,
                predecessor: replacement_predecessor,
                replacement: Some(ProcessSessionEndpoint {
                    identity: replacement_identity.clone(),
                    storage_incarnation: replacement_storage.clone(),
                    process_session_id: replacement_restart_handle.process_session_id(),
                    replicator_creation_id: ReplicatorCreationId::new(
                        "sqlite-replacement-restart-creation",
                    ),
                    role: ReplicaRole::None,
                }),
                surviving_sessions: vec![
                    source_handle
                        .public_v1_process_endpoint(ReplicaRole::Primary)
                        .await
                        .unwrap(),
                ],
                fresh_configuration: replacement_restart_configuration.clone(),
                predecessor_contained: true,
                data_loss: None,
            })
            .await
            .unwrap();
        source_handle
            .execute_public_v1(intent(
                &preview,
                source_handle.process_session_id(),
                14,
                "sqlite-source-replacement-restart-epoch",
                PublicOperationProgram::Epoch {
                    epoch: replacement_restart_epoch,
                },
            ))
            .await
            .unwrap();
        for (revision, name, program) in [
            (
                8,
                "sqlite-replacement-restart-open",
                PublicOperationProgram::Open {
                    replica: replacement_identity.clone(),
                    existing: true,
                },
            ),
            (
                9,
                "sqlite-replacement-restart-role",
                PublicOperationProgram::Role {
                    epoch: replacement_restart_epoch,
                    role: ReplicaRole::ActiveSecondary,
                },
            ),
            (
                10,
                "sqlite-replacement-restart-epoch",
                PublicOperationProgram::Epoch {
                    epoch: replacement_restart_epoch,
                },
            ),
        ] {
            replacement_restart_handle
                .execute_public_v1(intent(
                    &preview,
                    replacement_restart_handle.process_session_id(),
                    revision,
                    name,
                    program,
                ))
                .await
                .unwrap();
        }

        source_handle
            .execute_public_v1(intent(
                &preview,
                source_handle.process_session_id(),
                15,
                "sqlite-source-replacement-restart-configuration",
                PublicOperationProgram::Configuration(PublicConfiguration {
                    current: replacement_restart_configuration.clone(),
                    previous: None,
                    replicas: vec![PublicReplicaDescriptor {
                        build_id: OperationId::default(),
                        identity: replacement_identity.clone(),
                        replication_address: format!("http://{replacement_restart_address}"),
                        process_session_id: replacement_restart_handle.process_session_id(),
                        storage_incarnation: Some(replacement_storage.clone()),
                        role: ReplicaRole::ActiveSecondary,
                        current_progress: 5,
                        catch_up_capability: 5,
                        must_catchup: false,
                    }],
                }),
            ))
            .await
            .unwrap();
        replacement_restart_handle
            .execute_public_v1(intent(
                &preview,
                replacement_restart_handle.process_session_id(),
                11,
                "sqlite-replacement-restart-configuration",
                PublicOperationProgram::Configuration(PublicConfiguration {
                    current: replacement_restart_configuration.clone(),
                    previous: None,
                    replicas: vec![PublicReplicaDescriptor {
                        build_id: OperationId::default(),
                        identity: source_identity.clone(),
                        replication_address: format!("http://{source_address}"),
                        process_session_id: source_handle.process_session_id(),
                        storage_incarnation: Some(source_storage.clone()),
                        role: ReplicaRole::Primary,
                        current_progress: 5,
                        catch_up_capability: 5,
                        must_catchup: false,
                    }],
                }),
            ))
            .await
            .unwrap();
        let replacement_image = replacement_restart_persistence
            .inspect_for_test()
            .unwrap()
            .image;
        let replacement_image_path = root.join("replacement-restart-inspection.sqlite");
        std::fs::write(&replacement_image_path, replacement_image).unwrap();
        let replacement_database = rusqlite::Connection::open(&replacement_image_path).unwrap();
        assert_eq!(
            values(&replacement_database),
            ["snapshot", "retained", "after-build", "after-restart"]
        );
        let readmit = ConfigurationDescriptor::new(
            replacement_restart_epoch,
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
                ConfigurationMember {
                    identity: divergent_identity.clone(),
                    role: ReplicaRole::ActiveSecondary,
                },
            ],
            2,
        );
        assert!(
            source_handle
                .execute_public_v1(intent(
                    &preview,
                    source_handle.process_session_id(),
                    16,
                    "sqlite-reject-retired-divergent-after-restart",
                    PublicOperationProgram::Configuration(PublicConfiguration {
                        current: readmit,
                        previous: None,
                        replicas: vec![
                            PublicReplicaDescriptor {
                                build_id: OperationId::default(),
                                identity: replacement_identity.clone(),
                                replication_address: format!(
                                    "http://{replacement_restart_address}"
                                ),
                                process_session_id: replacement_restart_handle.process_session_id(),
                                storage_incarnation: Some(replacement_storage.clone()),
                                role: ReplicaRole::ActiveSecondary,
                                current_progress: 5,
                                catch_up_capability: 5,
                                must_catchup: false,
                            },
                            PublicReplicaDescriptor {
                                build_id: OperationId::default(),
                                identity: divergent_identity.clone(),
                                replication_address: format!("http://{divergent_address}"),
                                process_session_id: divergent_handle.process_session_id(),
                                storage_incarnation: Some(divergent_storage.clone()),
                                role: ReplicaRole::ActiveSecondary,
                                current_progress: 99,
                                catch_up_capability: 99,
                                must_catchup: false,
                            },
                        ],
                    }),
                ))
                .await
                .is_err()
        );

        source.shutdown();
        restarted_replacement.shutdown();
        source.wait().await.unwrap();
        restarted_replacement.wait().await.unwrap();
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

#[tokio::test]
async fn sqlite_real_provider_restart_settlement_and_handoff() {
    tokio::time::timeout(Duration::from_secs(180), async {
        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join("target")
            .join("sqlite-public-v1-composite")
            .join(std::process::id().to_string());
        let _ = std::fs::remove_dir_all(&root);
        let source_root = root.join("source");
        let secondary_root = root.join("secondary");
        std::fs::create_dir_all(&source_root).unwrap();
        std::fs::create_dir_all(&secondary_root).unwrap();

        let resource_uid = ResourceUid::new("sqlite-public-v1-composite");
        let preview = PublicV1Identity::new(1);
        let source_identity = identity(1, "sqlite-composite-source");
        let secondary_identity = identity(2, "sqlite-composite-secondary");
        let source_storage = storage(source_identity.clone(), "sqlite-composite-source");
        let secondary_storage = storage(secondary_identity.clone(), "sqlite-composite-secondary");
        for (path, identity, name) in [
            (
                &source_root,
                source_identity.clone(),
                "sqlite-composite-source",
            ),
            (
                &secondary_root,
                secondary_identity.clone(),
                "sqlite-composite-secondary",
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
        let secondary_address = address();
        let source_persistence =
            Arc::new(SqlitePersistence::open(source_root.join("application")).unwrap());
        let secondary_persistence =
            Arc::new(SqlitePersistence::open(secondary_root.join("application")).unwrap());
        let source_service = Arc::new(
            SqliteService::new(
                source_persistence.clone(),
                format!("http://{source_address}"),
            )
            .unwrap(),
        );
        let secondary_service = Arc::new(
            SqliteService::new(
                secondary_persistence.clone(),
                format!("http://{secondary_address}"),
            )
            .unwrap(),
        );
        let source_server = SqliteServer::new(source_service.clone());
        let mut source = ReplicaHost::new(
            ReplicaProcessConfig {
                resource_uid: resource_uid.clone(),
                replica_id: ReplicaId::new(1),
                pod_uid: PodUid::new("sqlite-composite-source-pod"),
                pvc_uid: PvcUid::new("sqlite-composite-source-pvc"),
                data_root: source_root.clone(),
                replication_address: source_address,
                bearer_token: "unused".into(),
                rpc_deadline: Duration::from_secs(2),
                transport_window_capacity: 8,
            },
            source_service.clone(),
            ApplicationStorageState::FreshEmpty,
            Arc::new(Resolver),
        )
        .start(tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap())
        .await
        .unwrap();
        let mut secondary = ReplicaHost::new(
            ReplicaProcessConfig {
                resource_uid: resource_uid.clone(),
                replica_id: ReplicaId::new(2),
                pod_uid: PodUid::new("sqlite-composite-secondary-pod"),
                pvc_uid: PvcUid::new("sqlite-composite-secondary-pvc"),
                data_root: secondary_root.clone(),
                replication_address: secondary_address,
                bearer_token: "unused".into(),
                rpc_deadline: Duration::from_secs(2),
                transport_window_capacity: 8,
            },
            secondary_service.clone(),
            ApplicationStorageState::FreshEmpty,
            Arc::new(Resolver),
        )
        .start(tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap())
        .await
        .unwrap();
        let source_handle = source.handle();
        let secondary_handle = secondary.handle();
        let generation = PublicProtocolGeneration::new(1);
        let initialization = derive_history_initialization_id(&resource_uid, generation);
        let history = derive_bootstrap_history(&resource_uid, generation, &initialization);
        source_handle
            .initialize_public_v1_history(&initialization, &history, &source_storage)
            .await
            .unwrap();
        secondary_handle
            .initialize_public_v1_history(&initialization, &history, &secondary_storage)
            .await
            .unwrap();

        let initial_epoch = Epoch::new(0, 1);
        let initial_configuration = ConfigurationDescriptor::new(
            initial_epoch,
            source_identity.replica_id,
            vec![
                ConfigurationMember {
                    identity: source_identity.clone(),
                    role: ReplicaRole::Primary,
                },
                ConfigurationMember {
                    identity: secondary_identity.clone(),
                    role: ReplicaRole::ActiveSecondary,
                },
            ],
            2,
        );
        for (handle, identity, role, peer, peer_address, peer_session, peer_storage) in [
            (
                &secondary_handle,
                secondary_identity.clone(),
                ReplicaRole::ActiveSecondary,
                source_identity.clone(),
                source_address,
                source_handle.process_session_id(),
                source_storage.clone(),
            ),
            (
                &source_handle,
                source_identity.clone(),
                ReplicaRole::Primary,
                secondary_identity.clone(),
                secondary_address,
                secondary_handle.process_session_id(),
                secondary_storage.clone(),
            ),
        ] {
            for (revision, name, program) in [
                (
                    1,
                    format!("{}-open", identity.instance_id),
                    PublicOperationProgram::Open {
                        replica: identity.clone(),
                        existing: true,
                    },
                ),
                (
                    2,
                    format!("{}-role", identity.instance_id),
                    PublicOperationProgram::Role {
                        epoch: initial_epoch,
                        role,
                    },
                ),
                (
                    3,
                    format!("{}-epoch", identity.instance_id),
                    PublicOperationProgram::Epoch {
                        epoch: initial_epoch,
                    },
                ),
                (
                    4,
                    format!("{}-configuration", identity.instance_id),
                    PublicOperationProgram::Configuration(PublicConfiguration {
                        current: initial_configuration.clone(),
                        previous: None,
                        replicas: vec![PublicReplicaDescriptor {
                            build_id: OperationId::default(),
                            identity: peer,
                            replication_address: format!("http://{peer_address}"),
                            process_session_id: peer_session,
                            storage_incarnation: Some(peer_storage),
                            role: if role == ReplicaRole::Primary {
                                ReplicaRole::ActiveSecondary
                            } else {
                                ReplicaRole::Primary
                            },
                            current_progress: 0,
                            catch_up_capability: 0,
                            must_catchup: role == ReplicaRole::Primary,
                        }],
                    }),
                ),
            ] {
                handle
                    .execute_public_v1(intent(
                        &preview,
                        handle.process_session_id(),
                        revision,
                        &name,
                        program,
                    ))
                    .await
                    .unwrap();
            }
        }
        assert_eq!(
            execute(
                &source_server,
                "CREATE TABLE data(id INTEGER PRIMARY KEY, value TEXT)"
            )
            .await,
            1
        );
        assert_eq!(
            execute(&source_server, "INSERT INTO data VALUES(1,'baseline')").await,
            2
        );
        tokio::time::timeout(Duration::from_secs(10), async {
            while secondary_persistence.progress().unwrap().committed_lsn < 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("baseline did not reach the real secondary provider");
        let baseline_snapshot = source_persistence.snapshot(2).unwrap();
        let source_predecessor = source_handle
            .public_v1_process_endpoint(ReplicaRole::Primary)
            .await
            .unwrap();
        let secondary_predecessor = secondary_handle
            .public_v1_process_endpoint(ReplicaRole::ActiveSecondary)
            .await
            .unwrap();

        assert_eq!(
            execute(&source_server, "INSERT INTO data VALUES(2,'unsettled')").await,
            3
        );
        let mut unsettled = source_persistence
            .get_replication_operations(3, 3)
            .await
            .unwrap()
            .try_collect::<Vec<_>>()
            .await
            .unwrap();
        assert_eq!(unsettled.len(), 1);
        source.shutdown();
        secondary.shutdown();
        source.wait().await.unwrap();
        secondary.wait().await.unwrap();
        drop(source_server);
        drop(source_service);
        drop(secondary_service);
        drop(source_persistence);
        drop(secondary_persistence);
        std::fs::remove_dir_all(source_root.join("application")).unwrap();
        std::fs::remove_dir_all(secondary_root.join("application")).unwrap();
        let unsettled = unsettled.remove(0);
        for (path, build) in [
            (
                source_root.join("application"),
                OperationId::new("sqlite-composite-source-prepare"),
            ),
            (
                secondary_root.join("application"),
                OperationId::new("sqlite-composite-secondary-prepare"),
            ),
        ] {
            let persistence = SqlitePersistence::open(path).unwrap();
            persistence
                .apply_copy_chunk(
                    &build,
                    1,
                    CopyChunk {
                        data: baseline_snapshot.clone().into(),
                    },
                )
                .await
                .unwrap();
            persistence.finish_copy(&build, 2, 2).await.unwrap();
            persistence.apply(unsettled.clone()).await.unwrap();
            assert_eq!(
                persistence.progress().unwrap(),
                kuberic_runtime::application::DurableApplicationProgress {
                    applied_lsn: 3,
                    committed_lsn: 2,
                }
            );
        }

        let source_address = address();
        let secondary_address = address();
        let source_persistence =
            Arc::new(SqlitePersistence::open(source_root.join("application")).unwrap());
        let secondary_persistence =
            Arc::new(SqlitePersistence::open(secondary_root.join("application")).unwrap());
        assert_eq!(
            source_persistence.progress().unwrap(),
            kuberic_runtime::application::DurableApplicationProgress {
                applied_lsn: 3,
                committed_lsn: 2,
            }
        );
        assert_eq!(
            secondary_persistence.progress().unwrap(),
            kuberic_runtime::application::DurableApplicationProgress {
                applied_lsn: 3,
                committed_lsn: 2,
            }
        );
        let source_service = Arc::new(
            SqliteService::new(
                source_persistence.clone(),
                format!("http://{source_address}"),
            )
            .unwrap(),
        );
        let secondary_service = Arc::new(
            SqliteService::new(
                secondary_persistence.clone(),
                format!("http://{secondary_address}"),
            )
            .unwrap(),
        );
        let source_server = SqliteServer::new(source_service.clone());
        let secondary_server = SqliteServer::new(secondary_service.clone());
        let mut source = ReplicaHost::new(
            ReplicaProcessConfig {
                resource_uid: resource_uid.clone(),
                replica_id: ReplicaId::new(1),
                pod_uid: PodUid::new("sqlite-composite-source-pod"),
                pvc_uid: PvcUid::new("sqlite-composite-source-pvc"),
                data_root: source_root.clone(),
                replication_address: source_address,
                bearer_token: "unused".into(),
                rpc_deadline: Duration::from_secs(2),
                transport_window_capacity: 8,
            },
            source_service,
            ApplicationStorageState::Established,
            Arc::new(Resolver),
        )
        .start(tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap())
        .await
        .unwrap();
        let mut secondary = ReplicaHost::new(
            ReplicaProcessConfig {
                resource_uid: resource_uid.clone(),
                replica_id: ReplicaId::new(2),
                pod_uid: PodUid::new("sqlite-composite-secondary-pod"),
                pvc_uid: PvcUid::new("sqlite-composite-secondary-pvc"),
                data_root: secondary_root.clone(),
                replication_address: secondary_address,
                bearer_token: "unused".into(),
                rpc_deadline: Duration::from_secs(2),
                transport_window_capacity: 8,
            },
            secondary_service,
            ApplicationStorageState::Established,
            Arc::new(Resolver),
        )
        .start(tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap())
        .await
        .unwrap();
        let source_handle = source.handle();
        let secondary_handle = secondary.handle();
        assert!(
            source_server
                .query(tonic::Request::new(proto::QueryRequest {
                    sql: "SELECT value FROM data WHERE id = 2".into(),
                    params: Vec::new(),
                }))
                .await
                .is_err()
        );
        assert!(
            secondary_server
                .query(tonic::Request::new(proto::QueryRequest {
                    sql: "SELECT value FROM data WHERE id = 2".into(),
                    params: Vec::new(),
                }))
                .await
                .is_err()
        );

        let restart_epoch = Epoch::new(0, 2);
        let source_only_configuration = ConfigurationDescriptor::new(
            restart_epoch,
            source_identity.replica_id,
            vec![ConfigurationMember {
                identity: source_identity.clone(),
                role: ReplicaRole::Primary,
            }],
            1,
        );
        source_handle
            .authorize_public_v1_session_renewal(ProcessSessionRenewalRequired {
                operation_id: OperationId::new("sqlite-composite-renew-source"),
                reason: ProcessSessionRenewalReason::PrimaryReplacement,
                resource_uid: resource_uid.clone(),
                history: history.clone(),
                previous_epoch: initial_epoch,
                renewal_epoch: restart_epoch,
                predecessor: source_predecessor,
                replacement: Some(ProcessSessionEndpoint {
                    identity: source_identity.clone(),
                    storage_incarnation: source_storage.clone(),
                    process_session_id: source_handle.process_session_id(),
                    replicator_creation_id: ReplicatorCreationId::new(
                        "sqlite-composite-renew-source-creation",
                    ),
                    role: ReplicaRole::None,
                }),
                surviving_sessions: Vec::new(),
                fresh_configuration: source_only_configuration,
                predecessor_contained: true,
                data_loss: None,
            })
            .await
            .unwrap();
        for (revision, name, program) in [
            (
                5,
                "sqlite-composite-source-reopen",
                PublicOperationProgram::Open {
                    replica: source_identity.clone(),
                    existing: true,
                },
            ),
            (
                6,
                "sqlite-composite-source-rerole",
                PublicOperationProgram::Role {
                    epoch: restart_epoch,
                    role: ReplicaRole::Primary,
                },
            ),
            (
                7,
                "sqlite-composite-source-reepoch",
                PublicOperationProgram::Epoch {
                    epoch: restart_epoch,
                },
            ),
        ] {
            source_handle
                .execute_public_v1(intent(
                    &preview,
                    source_handle.process_session_id(),
                    revision,
                    name,
                    program,
                ))
                .await
                .unwrap_or_else(|error| panic!("{name} failed: {error}"));
        }
        let renewed_source = source_handle
            .public_v1_process_endpoint(ReplicaRole::Primary)
            .await
            .unwrap();
        let restart_configuration = ConfigurationDescriptor::new(
            restart_epoch,
            source_identity.replica_id,
            vec![
                ConfigurationMember {
                    identity: source_identity.clone(),
                    role: ReplicaRole::Primary,
                },
                ConfigurationMember {
                    identity: secondary_identity.clone(),
                    role: ReplicaRole::ActiveSecondary,
                },
            ],
            2,
        );
        secondary_handle
            .authorize_public_v1_session_renewal(ProcessSessionRenewalRequired {
                operation_id: OperationId::new("sqlite-composite-renew-secondary"),
                reason: ProcessSessionRenewalReason::ActiveSecondaryReplacement,
                resource_uid: resource_uid.clone(),
                history: history.clone(),
                previous_epoch: initial_epoch,
                renewal_epoch: restart_epoch,
                predecessor: secondary_predecessor,
                replacement: Some(ProcessSessionEndpoint {
                    identity: secondary_identity.clone(),
                    storage_incarnation: secondary_storage.clone(),
                    process_session_id: secondary_handle.process_session_id(),
                    replicator_creation_id: ReplicatorCreationId::new(
                        "sqlite-composite-renew-secondary-creation",
                    ),
                    role: ReplicaRole::None,
                }),
                surviving_sessions: vec![renewed_source],
                fresh_configuration: restart_configuration.clone(),
                predecessor_contained: true,
                data_loss: None,
            })
            .await
            .unwrap();
        for (revision, name, program) in [
            (
                5,
                "sqlite-composite-secondary-reopen",
                PublicOperationProgram::Open {
                    replica: secondary_identity.clone(),
                    existing: true,
                },
            ),
            (
                6,
                "sqlite-composite-secondary-rerole",
                PublicOperationProgram::Role {
                    epoch: restart_epoch,
                    role: ReplicaRole::ActiveSecondary,
                },
            ),
            (
                7,
                "sqlite-composite-secondary-reepoch",
                PublicOperationProgram::Epoch {
                    epoch: restart_epoch,
                },
            ),
        ] {
            secondary_handle
                .execute_public_v1(intent(
                    &preview,
                    secondary_handle.process_session_id(),
                    revision,
                    name,
                    program,
                ))
                .await
                .unwrap();
        }
        let source_configuration = PublicConfiguration {
            current: restart_configuration.clone(),
            previous: None,
            replicas: vec![PublicReplicaDescriptor {
                build_id: OperationId::default(),
                identity: secondary_identity.clone(),
                replication_address: format!("http://{secondary_address}"),
                process_session_id: secondary_handle.process_session_id(),
                storage_incarnation: Some(secondary_storage.clone()),
                role: ReplicaRole::ActiveSecondary,
                current_progress: 2,
                catch_up_capability: 2,
                must_catchup: true,
            }],
        };
        let secondary_configuration = PublicConfiguration {
            current: restart_configuration.clone(),
            previous: None,
            replicas: vec![PublicReplicaDescriptor {
                build_id: OperationId::default(),
                identity: source_identity.clone(),
                replication_address: format!("http://{source_address}"),
                process_session_id: source_handle.process_session_id(),
                storage_incarnation: Some(source_storage.clone()),
                role: ReplicaRole::Primary,
                current_progress: 2,
                catch_up_capability: 2,
                must_catchup: false,
            }],
        };
        secondary_handle
            .execute_public_v1(intent(
                &preview,
                secondary_handle.process_session_id(),
                8,
                "sqlite-composite-secondary-reconfiguration",
                PublicOperationProgram::Configuration(secondary_configuration),
            ))
            .await
            .unwrap();
        source_handle
            .execute_public_v1(intent(
                &preview,
                source_handle.process_session_id(),
                8,
                "sqlite-composite-source-reconfiguration",
                PublicOperationProgram::Configuration(source_configuration.clone()),
            ))
            .await
            .unwrap();
        assert_ne!(
            source_handle.public_v1_report().await.unwrap().write_status,
            AccessStatus::Granted
        );
        assert!(
            source_server
                .execute(tonic::Request::new(proto::ExecuteRequest {
                    sql: "INSERT INTO data VALUES(99,'must-stay-closed')".into(),
                    params: Vec::new(),
                }))
                .await
                .is_err()
        );
        source_handle
            .execute_public_v1(intent(
                &preview,
                source_handle.process_session_id(),
                9,
                "sqlite-composite-settle-suffix",
                PublicOperationProgram::CatchUp {
                    configuration: source_configuration,
                    mode: PublicCatchUpMode::WriteQuorum,
                },
            ))
            .await
            .unwrap();
        assert_eq!(source_persistence.progress().unwrap().committed_lsn, 3);
        assert_eq!(secondary_persistence.progress().unwrap().committed_lsn, 3);
        let settled = source_server
            .query(tonic::Request::new(proto::QueryRequest {
                sql: "SELECT value FROM data WHERE id = 2".into(),
                params: Vec::new(),
            }))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(settled.rows.len(), 1);
        let source_report = source_handle.public_v1_report().await.unwrap();
        let secondary_report = secondary_handle.public_v1_report().await.unwrap();
        assert_eq!(source_report.write_status, AccessStatus::Granted);
        assert_ne!(secondary_report.write_status, AccessStatus::Granted);

        let handoff_epoch = Epoch::new(0, 3);
        let handoff_configuration = ConfigurationDescriptor::new(
            handoff_epoch,
            secondary_identity.replica_id,
            vec![
                ConfigurationMember {
                    identity: source_identity.clone(),
                    role: ReplicaRole::ActiveSecondary,
                },
                ConfigurationMember {
                    identity: secondary_identity.clone(),
                    role: ReplicaRole::Primary,
                },
            ],
            2,
        );
        for (handle, revision, name, role) in [
            (
                &source_handle,
                10,
                "sqlite-composite-source-handoff-role",
                ReplicaRole::ActiveSecondary,
            ),
            (
                &secondary_handle,
                9,
                "sqlite-composite-secondary-handoff-role",
                ReplicaRole::Primary,
            ),
        ] {
            handle
                .execute_public_v1(intent(
                    &preview,
                    handle.process_session_id(),
                    revision,
                    &format!("{name}-epoch"),
                    PublicOperationProgram::Epoch {
                        epoch: handoff_epoch,
                    },
                ))
                .await
                .unwrap();
            handle
                .execute_public_v1(intent(
                    &preview,
                    handle.process_session_id(),
                    revision + 1,
                    name,
                    PublicOperationProgram::Role {
                        epoch: handoff_epoch,
                        role,
                    },
                ))
                .await
                .unwrap();
        }
        let source_handoff = PublicConfiguration {
            current: handoff_configuration.clone(),
            previous: None,
            replicas: vec![PublicReplicaDescriptor {
                build_id: OperationId::default(),
                identity: secondary_identity.clone(),
                replication_address: format!("http://{secondary_address}"),
                process_session_id: secondary_handle.process_session_id(),
                storage_incarnation: Some(secondary_storage.clone()),
                role: ReplicaRole::Primary,
                current_progress: 3,
                catch_up_capability: 3,
                must_catchup: false,
            }],
        };
        let secondary_handoff = PublicConfiguration {
            current: handoff_configuration,
            previous: None,
            replicas: vec![PublicReplicaDescriptor {
                build_id: OperationId::default(),
                identity: source_identity.clone(),
                replication_address: format!("http://{source_address}"),
                process_session_id: source_handle.process_session_id(),
                storage_incarnation: Some(source_storage.clone()),
                role: ReplicaRole::ActiveSecondary,
                current_progress: 3,
                catch_up_capability: 3,
                must_catchup: false,
            }],
        };
        source_handle
            .execute_public_v1(intent(
                &preview,
                source_handle.process_session_id(),
                12,
                "sqlite-composite-source-handoff-configuration",
                PublicOperationProgram::Configuration(source_handoff),
            ))
            .await
            .unwrap();
        secondary_handle
            .execute_public_v1(intent(
                &preview,
                secondary_handle.process_session_id(),
                11,
                "sqlite-composite-secondary-handoff-configuration",
                PublicOperationProgram::Configuration(secondary_handoff),
            ))
            .await
            .unwrap();
        let source_report = source_handle.public_v1_report().await.unwrap();
        let secondary_report = secondary_handle.public_v1_report().await.unwrap();
        assert_ne!(source_report.write_status, AccessStatus::Granted);
        assert_eq!(secondary_report.write_status, AccessStatus::Granted);
        assert!(
            source_server
                .execute(tonic::Request::new(proto::ExecuteRequest {
                    sql: "INSERT INTO data VALUES(99,'old-primary')".into(),
                    params: Vec::new(),
                }))
                .await
                .is_err()
        );
        assert_eq!(
            execute(
                &secondary_server,
                "INSERT INTO data VALUES(3,'after-handoff')"
            )
            .await,
            4
        );
        tokio::time::timeout(Duration::from_secs(10), async {
            while source_persistence.progress().unwrap().committed_lsn < 4 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("post-handoff write did not reach the old primary provider");
        for (name, persistence) in [
            ("source", &source_persistence),
            ("secondary", &secondary_persistence),
        ] {
            let inspection = persistence.inspect_for_test().unwrap();
            let inspection_path = root.join(format!("{name}-inspection.sqlite"));
            std::fs::write(&inspection_path, inspection.image).unwrap();
            let database = rusqlite::Connection::open(&inspection_path).unwrap();
            assert_eq!(
                values(&database),
                ["baseline", "unsettled", "after-handoff"],
                "{name} provider retained a different history"
            );
        }

        source.shutdown();
        secondary.shutdown();
        source.wait().await.unwrap();
        secondary.wait().await.unwrap();
        std::fs::remove_dir_all(root).unwrap();
    })
    .await
    .unwrap();
}
