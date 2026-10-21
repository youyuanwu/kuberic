#![cfg(feature = "testing")]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use kuberic_runtime::RuntimeError;
use kuberic_runtime::engine::DurableState;
use kuberic_runtime::host::{
    ApplicationStorageState, ReplicaEndpointResolver, ReplicaHost, ReplicaProcessConfig,
    RunningReplica,
};
use kuberic_runtime::protocol::public_operations::*;
use kuberic_runtime::protocol::types::*;
use kuberic_runtime::replicator::TestingTcpRoute;
use kuberic_runtime::testing::sqlite_store::SqliteStore;
use kuberic_runtime::testing::state::{SCHEMA_VERSION, StorageIdentity};
use kvstore2::persistence::KvPersistence;
use kvstore2::service::KvService;
use kvstore2::state::CopyGate;

struct Resolver;

impl ReplicaEndpointResolver for Resolver {
    fn control_endpoint(&self, _: &ReplicaIdentity) -> String {
        "http://127.0.0.1:1".into()
    }
    fn replication_endpoint(&self, _: &ReplicaIdentity) -> String {
        "http://127.0.0.1:1".into()
    }
}

struct Replica {
    identity: ReplicaIdentity,
    storage: ReplicaStorageIncarnationContext,
    address: String,
    service: Arc<KvService>,
    running: RunningReplica,
}

struct Fixture {
    _root: tempfile::TempDir,
    _route: TestingTcpRoute,
    blocked: Arc<AtomicBool>,
    preview: PublicV1Identity,
    replicas: Vec<Replica>,
}

impl Fixture {
    async fn start(name: &str) -> Self {
        let root = tempfile::Builder::new().prefix(name).tempdir().unwrap();
        let resource = ResourceUid::new(name);
        let preview = PublicV1Identity::new(51);
        let blocked = Arc::new(AtomicBool::new(false));
        let blocked_route = blocked.clone();
        let route = TestingTcpRoute::register(format!("{name}-1-pod"), move |_, endpoint| {
            let blocked = blocked_route.load(Ordering::Acquire);
            Box::pin(async move {
                if blocked {
                    Err(std::io::Error::new(
                        std::io::ErrorKind::ConnectionRefused,
                        "SF-01 physical quorum loss",
                    ))
                } else {
                    Ok(endpoint)
                }
            })
        })
        .unwrap();
        let mut replicas = Vec::new();
        for id in 1..=3 {
            let name = format!("{name}-{id}");
            let path = root.path().join(&name);
            let identity = ReplicaIdentity {
                replica_id: ReplicaId::new(id),
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
                SqliteStore::create_public_v1_with_application_paths(
                    SqliteStore::metadata_database_path(&path),
                    preview.clone(),
                    StorageIdentity {
                        schema_version: SCHEMA_VERSION,
                        resource_uid: resource.clone(),
                        pod_uid: PodUid::new(format!("{name}-pod")),
                        pvc_uid: PvcUid::new(format!("{name}-pvc")),
                        initialization_id: InitializationId::new(format!("{name}-initialization")),
                        local_identity: identity.clone(),
                        effective_policy: EffectivePolicy::fixed(3, 30).unwrap(),
                    },
                    std::collections::BTreeMap::from([(
                        "application".into(),
                        path.join("application"),
                    )]),
                )
                .unwrap(),
            );
            let address = "127.0.0.1:0".parse().unwrap();
            let service = Arc::new(KvService::new(
                Arc::new(KvPersistence::open(path.join("application")).unwrap()),
                format!("http://{address}"),
                CopyGate::disabled(),
            ));
            let running = ReplicaHost::new(
                ReplicaProcessConfig {
                    resource_uid: resource.clone(),
                    replica_id: identity.replica_id,
                    pod_uid: PodUid::new(format!("{name}-pod")),
                    pvc_uid: PvcUid::new(format!("{name}-pvc")),
                    data_root: path.clone(),
                    replication_address: address,
                    bearer_token: "read-fence-token".into(),
                    rpc_deadline: Duration::from_secs(5),
                    transport_window_capacity: 8,
                },
                service.clone(),
                ApplicationStorageState::FreshEmpty,
                Arc::new(Resolver),
            )
            .with_application_storage_paths(std::collections::BTreeMap::from([(
                "application".into(),
                path.join("application"),
            )]))
            .with_public_v1_binding(PublicV1LifecycleBinding {
                identity: preview.clone(),
                resource_uid: resource.clone(),
                spec_generation: 1,
                state_persistence: StatePersistence::Persisted,
            })
            .start(tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap())
            .await
            .unwrap();
            replicas.push(Replica {
                identity,
                storage,
                address: format!("http://{address}"),
                service,
                running,
            });
        }
        let initialization =
            derive_history_initialization_id(&resource, PublicProtocolGeneration::new(51));
        let history = derive_bootstrap_history(
            &resource,
            PublicProtocolGeneration::new(51),
            &initialization,
        );
        let mut fixture = Self {
            _root: root,
            _route: route,
            blocked,
            preview,
            replicas,
        };
        let epoch = Epoch::new(0, 1);
        for index in 0..fixture.replicas.len() {
            let replica = &fixture.replicas[index];
            replica
                .running
                .handle()
                .initialize_public_v1_history(&initialization, &history, &replica.storage)
                .await
                .unwrap();
            fixture
                .execute(
                    index,
                    1,
                    PublicOperationProgram::Open {
                        replica: replica.identity.clone(),
                        existing: false,
                    },
                )
                .await;
            fixture.replicas[index].address = fixture.replicas[index]
                .running
                .handle()
                .public_v1_report()
                .await
                .unwrap()
                .replication_address
                .unwrap();
            fixture
                .execute(
                    index,
                    2,
                    PublicOperationProgram::Role {
                        epoch,
                        role: if index == 0 {
                            ReplicaRole::Primary
                        } else {
                            ReplicaRole::IdleSecondary
                        },
                    },
                )
                .await;
        }
        fixture
            .execute(0, 3, PublicOperationProgram::Epoch { epoch })
            .await;
        let single = ConfigurationDescriptor::new(
            epoch,
            fixture.replicas[0].identity.replica_id,
            vec![ConfigurationMember {
                identity: fixture.replicas[0].identity.clone(),
                role: ReplicaRole::Primary,
            }],
            1,
        );
        fixture
            .execute(
                0,
                4,
                PublicOperationProgram::Configuration(PublicConfiguration {
                    current: single.clone(),
                    previous: None,
                    replicas: Vec::new(),
                }),
            )
            .await;
        for index in [1, 2] {
            let source = &fixture.replicas[0];
            let target = &fixture.replicas[index];
            let source_address = source
                .running
                .handle()
                .public_v1_report()
                .await
                .unwrap()
                .replication_address
                .unwrap();
            let attempt = OperationId::new(format!("copy-{index}"));
            let revision = 4;
            target
                .running
                .handle()
                .install_public_v1_inbound_build_authority(&PublicInboundBuildAuthority {
                    operation_id: attempt.clone(),
                    operation_revision: revision,
                    attempt: PublicBuildAttemptReference {
                        resource_uid: resource.clone(),
                        epoch,
                        build_id: attempt.clone(),
                        source: source.identity.clone(),
                        source_storage: source.storage.clone(),
                        source_session: source.running.handle().process_session_id(),
                        target: target.identity.clone(),
                        target_storage: target.storage.clone(),
                        target_session: target.running.handle().process_session_id(),
                    },
                    history: history.clone(),
                    current_configuration_id: single.configuration_id.clone(),
                    source_address: source_address.clone(),
                    target_address: target.address.clone(),
                })
                .await
                .unwrap();
            fixture
                .execute(
                    0,
                    revision,
                    PublicOperationProgram::Build(PublicBuildInput {
                        attempt,
                        epoch,
                        source_address,
                        current_configuration_id: Some(single.configuration_id.clone()),
                        replica: target.identity.clone(),
                        target_storage: target.storage.clone(),
                        process_session_id: target.running.handle().process_session_id(),
                        replication_address: target.address.clone(),
                        history: Some(history.clone()),
                    }),
                )
                .await;
            fixture
                .execute(
                    index,
                    3,
                    PublicOperationProgram::Role {
                        epoch,
                        role: ReplicaRole::ActiveSecondary,
                    },
                )
                .await;
            fixture
                .execute(index, 4, PublicOperationProgram::Epoch { epoch })
                .await;
        }
        let configuration = ConfigurationDescriptor::new(
            epoch,
            fixture.replicas[0].identity.replica_id,
            fixture
                .replicas
                .iter()
                .enumerate()
                .map(|(index, replica)| ConfigurationMember {
                    identity: replica.identity.clone(),
                    role: if index == 0 {
                        ReplicaRole::Primary
                    } else {
                        ReplicaRole::ActiveSecondary
                    },
                })
                .collect(),
            2,
        );
        for index in [1, 2, 0] {
            let mut peers = Vec::new();
            for (peer_index, peer) in fixture.replicas.iter().enumerate() {
                if index == peer_index {
                    continue;
                }
                let report = peer.running.handle().public_v1_report().await.unwrap();
                peers.push(PublicReplicaDescriptor {
                    build_id: OperationId::default(),
                    identity: peer.identity.clone(),
                    replication_address: peer.address.clone(),
                    process_session_id: report.process_session_id,
                    storage_incarnation: Some(peer.storage.clone()),
                    role: if peer_index == 0 {
                        ReplicaRole::Primary
                    } else {
                        ReplicaRole::ActiveSecondary
                    },
                    current_progress: report.progress.current().unwrap(),
                    catch_up_capability: report.progress.capability().unwrap(),
                    must_catchup: false,
                });
            }
            fixture
                .execute(
                    index,
                    7,
                    PublicOperationProgram::Configuration(PublicConfiguration {
                        current: configuration.clone(),
                        previous: None,
                        replicas: peers,
                    }),
                )
                .await;
        }
        fixture.replicas[0]
            .service
            .replicate_put("baseline".into(), "committed".into())
            .await
            .unwrap();
        fixture.assert_committed("baseline", "committed").await;
        fixture
    }

    async fn execute(&self, index: usize, revision: u64, program: PublicOperationProgram) {
        let handle = self.replicas[index].running.handle();
        let class = match &program {
            PublicOperationProgram::Build(build) => PublicOperationClass::Build {
                target: build.replica.replica_id,
            },
            PublicOperationProgram::Abort => PublicOperationClass::Abort,
            PublicOperationProgram::Close => PublicOperationClass::Close,
            _ => PublicOperationClass::Authority,
        };
        let operation_id = match &program {
            PublicOperationProgram::Build(build) | PublicOperationProgram::Remove(build) => {
                build.attempt.clone()
            }
            _ => OperationId::new(format!("fence-{index}-{revision}")),
        };
        tokio::time::timeout(
            Duration::from_secs(15),
            handle.execute_public_v1(PublicOperationIntent {
                identity: self.preview.clone(),
                operation_id,
                revision,
                process_session_id: handle.process_session_id(),
                class,
                input_digest: program.digest(),
                replay_disposition: program.replay_disposition(),
                lifecycle: None,
                program: Some(program),
            }),
        )
        .await
        .unwrap()
        .unwrap();
    }

    async fn assert_committed(&self, key: &str, value: &str) {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if self.replicas[0]
                    .service
                    .get(key)
                    .await
                    .ok()
                    .flatten()
                    .as_deref()
                    == Some(value)
                    && self.replicas.iter().all(|replica| {
                        replica.service.persistence().get(key).as_deref() == Some(value)
                    })
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("service-owned committed views did not converge");
        assert_eq!(
            self.replicas[0].service.get(key).await.unwrap().as_deref(),
            Some(value)
        );
        for replica in &self.replicas[1..] {
            assert!(matches!(
                replica.service.get(key).await,
                Err(RuntimeError::ReadClosed(AccessStatus::NotPrimary))
            ));
        }
    }

    async fn pending_is_hidden(&self) {
        let primary = &self.replicas[0];
        assert_eq!(primary.service.get("visible").await.unwrap(), None);
        assert_eq!(
            primary.service.get("baseline").await.unwrap().as_deref(),
            Some("committed")
        );
        let durable = primary
            .service
            .persistence()
            .durable_progress()
            .await
            .unwrap();
        assert_eq!((durable.applied_lsn, durable.committed_lsn), (2, 1));
        let report = primary.running.handle().public_v1_report().await.unwrap();
        assert_eq!(report.read_status, AccessStatus::Granted);
        assert_eq!(report.progress.current(), Some(2));
    }

    async fn finish(mut self, aborted: bool) {
        if !aborted {
            for index in 0..3 {
                self.execute(index, 20, PublicOperationProgram::Close).await;
            }
        }
        for replica in &self.replicas {
            replica.running.shutdown();
        }
        for replica in &mut self.replicas {
            tokio::time::timeout(Duration::from_secs(10), replica.running.wait())
                .await
                .unwrap()
                .unwrap();
            let result =
                tokio::time::timeout(Duration::from_secs(5), replica.service.shutdown_and_drain())
                    .await
                    .unwrap();
            if aborted {
                assert!(
                    matches!(result, Err(RuntimeError::OperationCancelled)),
                    "intentional Abort must retain typed cancellation, got {result:?}"
                );
            } else {
                result.unwrap();
            }
        }
    }
}

#[tokio::test]
async fn sf15_trait_close_retains_cancelled_consumer_drain() {
    use kuberic_runtime::StatefulServiceReplica;
    let fixture = Fixture::start("sf15-close").await;
    let service = fixture.replicas[1].service.clone();
    assert!(service.retained_consumer_count().await > 0);
    let gate = service.pause_next_stream_exit();
    let closing = service.clone();
    let close = tokio::spawn(async move { StatefulServiceReplica::close(closing.as_ref()).await });
    tokio::time::timeout(Duration::from_secs(5), gate.entered())
        .await
        .unwrap();
    assert!(
        !close.is_finished(),
        "trait Close returned before its owned stream consumer exited"
    );
    close.abort();
    assert!(close.await.unwrap_err().is_cancelled());
    let closing = service.clone();
    let mut retry =
        tokio::spawn(async move { StatefulServiceReplica::close(closing.as_ref()).await });
    assert!(
        tokio::time::timeout(Duration::from_millis(100), &mut retry)
            .await
            .is_err(),
        "cancelled Close detached consumer ownership"
    );
    gate.release();
    tokio::time::timeout(Duration::from_secs(5), retry)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(service.retained_consumer_count().await, 0);
    StatefulServiceReplica::close(service.as_ref())
        .await
        .unwrap();
    // The trait boundary above is observed before host shutdown. Afterwards use
    // forceful host cleanup: native graceful Close cannot require EOF consumption
    // from an application whose stream consumers were explicitly already closed.
    let mut fixture = fixture;
    for replica in &fixture.replicas {
        replica.running.shutdown();
    }
    for replica in &mut fixture.replicas {
        tokio::time::timeout(Duration::from_secs(10), replica.running.wait())
            .await
            .unwrap()
            .unwrap();
        let result = replica.service.shutdown_and_drain().await;
        assert!(
            matches!(result, Ok(()) | Err(RuntimeError::OperationCancelled)),
            "unexpected forceful fixture cleanup diagnostic: {result:?}"
        );
    }
}

async fn gated_write(
    fixture: &Fixture,
) -> (
    kvstore2::v1_test_bridge::CommitGate,
    tokio::task::JoinHandle<kuberic_runtime::Result<i64>>,
) {
    let mut gate = fixture.replicas[0].service.pause_next_commit().unwrap();
    let primary = fixture.replicas[0].service.clone();
    let write = tokio::spawn(async move {
        primary
            .replicate_put("visible".into(), "pending".into())
            .await
    });
    tokio::time::timeout(Duration::from_secs(10), gate.entered())
        .await
        .unwrap();
    (gate, write)
}

#[tokio::test]
async fn sf01_primary_local_commit_hides_speculation_after_native_completion() {
    let fixture = Fixture::start("sf01-local").await;
    let (gate, write) = gated_write(&fixture).await;
    fixture.pending_is_hidden().await;
    assert!(
        !write.is_finished(),
        "native completion is not client success"
    );
    gate.release();
    assert_eq!(write.await.unwrap().unwrap(), 2);
    fixture.assert_committed("visible", "pending").await;
    fixture.finish(false).await;
}

#[tokio::test]
async fn sf01_secondary_ack_does_not_publish_the_primary_application_view() {
    let fixture = Fixture::start("sf01-secondary").await;
    let (gate, write) = gated_write(&fixture).await;
    fixture.pending_is_hidden().await;
    assert!(!write.is_finished());
    assert!(
        fixture.replicas[1..].iter().any(|replica| replica
            .service
            .persistence()
            .get("visible")
            .as_deref()
            == Some("pending"))
    );
    for replica in &fixture.replicas[1..] {
        assert!(matches!(
            replica.service.get("visible").await,
            Err(RuntimeError::ReadClosed(AccessStatus::NotPrimary))
        ));
    }
    gate.release();
    write.await.unwrap().unwrap();
    fixture.assert_committed("visible", "pending").await;
    fixture.finish(false).await;
}

#[tokio::test]
async fn sf01_dropped_client_retains_service_owned_commit_and_observation() {
    let fixture = Fixture::start("sf01-drop").await;
    let (gate, write) = gated_write(&fixture).await;
    fixture.pending_is_hidden().await;
    write.abort();
    assert!(write.await.unwrap_err().is_cancelled());
    gate.release();
    fixture.assert_committed("visible", "pending").await;
    assert_eq!(
        fixture.replicas[0]
            .service
            .replicate_put("later".into(), "live".into())
            .await
            .unwrap(),
        3
    );
    fixture.finish(false).await;
}

#[tokio::test]
async fn sf01_abort_during_local_commit_never_late_grants_success() {
    let fixture = Fixture::start("sf01-abort").await;
    let (gate, write) = gated_write(&fixture).await;
    fixture.pending_is_hidden().await;
    fixture.execute(0, 8, PublicOperationProgram::Abort).await;
    gate.release();
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(5), write)
            .await
            .unwrap()
            .unwrap(),
        Err(RuntimeError::AdmittedOperationFailed { lsn: 2, .. })
    ));
    let durable = fixture.replicas[0]
        .service
        .persistence()
        .durable_progress()
        .await
        .unwrap();
    assert_eq!(durable.committed_lsn, 1);
    assert!(fixture.replicas[0].service.get("visible").await.is_err());
    assert!(
        fixture.replicas[0]
            .service
            .replicate_put("late".into(), "forbidden".into())
            .await
            .is_err()
    );
    fixture.finish(true).await;
}

#[tokio::test]
async fn sf01_physical_quorum_loss_cannot_complete_or_publish_an_admitted_write() {
    let fixture = Fixture::start("sf01-quorum").await;
    fixture.blocked.store(true, Ordering::Release);
    for replica in &fixture.replicas[1..] {
        replica.running.shutdown();
    }
    tokio::time::timeout(Duration::from_secs(5), async {
        while fixture.replicas[1..]
            .iter()
            .any(|replica| !replica.running.is_finished())
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let primary = fixture.replicas[0].service.clone();
    let mut write = tokio::spawn(async move {
        primary
            .replicate_put("visible".into(), "pending".into())
            .await
    });
    let completed = match tokio::time::timeout(Duration::from_millis(150), &mut write).await {
        Ok(result) => {
            assert!(
                result.unwrap().is_err(),
                "quorum loss cannot acknowledge a write"
            );
            true
        }
        Err(_) => false,
    };
    let progress = fixture.replicas[0]
        .service
        .persistence()
        .durable_progress()
        .await
        .unwrap();
    assert_eq!(progress.committed_lsn, 1);
    if let Ok(visible) = fixture.replicas[0].service.get("visible").await {
        assert_eq!(visible, None);
    }
    fixture.execute(0, 8, PublicOperationProgram::Abort).await;
    if !completed {
        assert!(
            tokio::time::timeout(Duration::from_secs(5), write)
                .await
                .unwrap()
                .unwrap()
                .is_err()
        );
    }
    assert!(fixture.replicas[0].service.get("visible").await.is_err());
    fixture.finish(true).await;
}
