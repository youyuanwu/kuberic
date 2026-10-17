#![cfg(feature = "testing")]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use kuberic_runtime::RuntimeError;
use kuberic_runtime::application::DurableApplicationProgress;
use kuberic_runtime::engine::DurableState;
use kuberic_runtime::host::{
    ApplicationStorageState, ReplicaEndpointResolver, ReplicaHost, ReplicaProcessConfig,
    RunningReplica,
};
use kuberic_runtime::protocol::public_operations::{
    PublicCatchUpMode, PublicConfiguration, PublicOperationClass, PublicOperationIntent,
    PublicOperationProgram, PublicReplicaDescriptor, PublicV1Identity, PublicV1LifecycleBinding,
    StatePersistence,
};
use kuberic_runtime::protocol::types::*;
use kuberic_runtime::replicator::stateless::TestingTcpRoute;
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
    address: std::net::SocketAddr,
    service: Arc<KvService>,
    running: RunningReplica,
}

struct Fixture {
    _root: tempfile::TempDir,
    _route: TestingTcpRoute,
    blocked: Arc<AtomicBool>,
    unavailable_attempts: Arc<AtomicUsize>,
    preview: PublicV1Identity,
    configuration: ConfigurationDescriptor,
    replicas: Vec<Replica>,
}

impl Fixture {
    async fn start(name: &str) -> Self {
        let scratch = std::env::current_dir().unwrap().join("target");
        std::fs::create_dir_all(&scratch).unwrap();
        let root = tempfile::Builder::new()
            .prefix(name)
            .tempdir_in(&scratch)
            .unwrap();
        let resource = ResourceUid::new(name);
        let preview = PublicV1Identity::new(51);
        let blocked = Arc::new(AtomicBool::new(false));
        let unavailable_attempts = Arc::new(AtomicUsize::new(0));
        let blocked_route = blocked.clone();
        let attempts = unavailable_attempts.clone();
        let unavailable_instance = format!("{name}-3-pod");
        let route =
            TestingTcpRoute::register(format!("{name}-1-pod"), move |destination, endpoint| {
                let blocked = blocked_route.load(Ordering::Acquire);
                let unavailable = destination == unavailable_instance;
                let attempts = attempts.clone();
                Box::pin(async move {
                    if blocked && unavailable {
                        attempts.fetch_add(1, Ordering::AcqRel);
                        Err(std::io::Error::new(
                            std::io::ErrorKind::ConnectionRefused,
                            "SF-01 test transport partition",
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
                SqliteStore::create_public_v1(
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
                )
                .unwrap(),
            );
            let reservation = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let address = reservation.local_addr().unwrap();
            drop(reservation);
            let persistence = Arc::new(KvPersistence::open(path.join("application")).unwrap());
            let service = Arc::new(KvService::new(
                persistence,
                format!("http://{address}"),
                CopyGate::disabled(),
            ));
            let running = ReplicaHost::new(
                ReplicaProcessConfig {
                    resource_uid: resource.clone(),
                    replica_id: ReplicaId::new(id),
                    pod_uid: PodUid::new(format!("{name}-pod")),
                    pvc_uid: PvcUid::new(format!("{name}-pvc")),
                    data_root: path,
                    replication_address: address,
                    bearer_token: "read-fence-token".into(),
                    rpc_deadline: Duration::from_secs(5),
                    transport_window_capacity: 8,
                },
                service.clone(),
                ApplicationStorageState::FreshEmpty,
                Arc::new(Resolver),
            )
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
                address,
                service,
                running,
            });
        }
        let generation = PublicProtocolGeneration::new(51);
        let initialization = derive_history_initialization_id(&resource, generation);
        let history = derive_bootstrap_history(&resource, generation, &initialization);
        for replica in &replicas {
            replica
                .running
                .handle()
                .initialize_public_v1_history(&initialization, &history, &replica.storage)
                .await
                .unwrap();
        }
        let epoch = Epoch::new(0, 1);
        let descriptor = ConfigurationDescriptor::new(
            epoch,
            ReplicaId::new(1),
            replicas
                .iter()
                .enumerate()
                .map(|(i, replica)| ConfigurationMember {
                    identity: replica.identity.clone(),
                    role: Self::role(i),
                })
                .collect(),
            3,
        );
        let fixture = Self {
            _root: root,
            _route: route,
            blocked,
            unavailable_attempts,
            preview,
            configuration: descriptor.clone(),
            replicas,
        };
        // Receivers are ready before the primary can send any data.
        for index in [1, 2, 0] {
            let replica = &fixture.replicas[index];
            for (revision, program) in [
                (
                    1,
                    PublicOperationProgram::Open {
                        replica: replica.identity.clone(),
                        existing: true,
                    },
                ),
                (
                    2,
                    PublicOperationProgram::Role {
                        epoch,
                        role: Self::role(index),
                    },
                ),
                (3, PublicOperationProgram::Epoch { epoch }),
                (
                    4,
                    PublicOperationProgram::Configuration(PublicConfiguration {
                        current: descriptor.clone(),
                        previous: None,
                        replicas: fixture
                            .replicas
                            .iter()
                            .enumerate()
                            .filter(|(i, _)| *i != index)
                            .map(|(i, peer)| PublicReplicaDescriptor {
                                build_id: OperationId::default(),
                                identity: peer.identity.clone(),
                                replication_address: format!("http://{}", peer.address),
                                process_session_id: peer.running.handle().process_session_id(),
                                storage_incarnation: Some(peer.storage.clone()),
                                role: Self::role(i),
                                current_progress: 0,
                                catch_up_capability: 0,
                                must_catchup: false,
                            })
                            .collect(),
                    }),
                ),
            ] {
                fixture.execute(index, revision, program).await;
            }
        }
        fixture.replicas[0]
            .service
            .replicate_put("baseline".into(), "committed".into())
            .await
            .unwrap();
        fixture.wait_readable("baseline", "committed").await;
        fixture
    }

    fn role(index: usize) -> ReplicaRole {
        if index == 0 {
            ReplicaRole::Primary
        } else {
            ReplicaRole::ActiveSecondary
        }
    }

    async fn execute(&self, index: usize, revision: u64, program: PublicOperationProgram) {
        let handle = self.replicas[index].running.handle();
        tokio::time::timeout(
            Duration::from_secs(15),
            handle.execute_public_v1(PublicOperationIntent {
                identity: self.preview.clone(),
                operation_id: OperationId::new(format!("fence-{index}-{revision}")),
                revision,
                process_session_id: handle.process_session_id(),
                class: if matches!(program, PublicOperationProgram::Abort) {
                    PublicOperationClass::Abort
                } else {
                    PublicOperationClass::Authority
                },
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

    async fn denied(&self, index: usize) {
        let read = tokio::time::timeout(
            Duration::from_secs(2),
            self.replicas[index].service.get("visible"),
        )
        .await
        .unwrap();
        assert!(
            matches!(
                read,
                Err(RuntimeError::ReadClosed(AccessStatus::NotPrimary))
            ),
            "normal read must be admission-denied on replica {index}, got {read:?}"
        );
    }

    async fn wait_readable(&self, key: &str, value: &str) {
        let outcome = tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                let mut ready = true;
                for replica in &self.replicas {
                    ready &=
                        matches!(replica.service.get(key).await, Ok(Some(found)) if found == value);
                }
                if ready {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await;
        if outcome.is_err() {
            for replica in &self.replicas {
                eprintln!(
                    "{:?}: read={:?}, durable={:?}, report={:?}",
                    replica.identity,
                    replica.service.get(key).await,
                    replica.service.persistence().durable_progress().await,
                    replica.running.handle().public_v1_report().await,
                );
            }
        }
        outcome.expect("normal reads did not restore after genuine commitment");
    }

    async fn settle(&self) {
        self.blocked.store(false, Ordering::Release);
        self.execute(
            0,
            5,
            PublicOperationProgram::CatchUp {
                configuration: PublicConfiguration {
                    current: self.configuration.clone(),
                    previous: None,
                    replicas: self
                        .replicas
                        .iter()
                        .enumerate()
                        .skip(1)
                        .map(|(i, peer)| PublicReplicaDescriptor {
                            build_id: OperationId::default(),
                            identity: peer.identity.clone(),
                            replication_address: format!("http://{}", peer.address),
                            process_session_id: peer.running.handle().process_session_id(),
                            storage_incarnation: Some(peer.storage.clone()),
                            role: Self::role(i),
                            current_progress: 1,
                            catch_up_capability: 1,
                            must_catchup: false,
                        })
                        .collect(),
                },
                mode: PublicCatchUpMode::WriteQuorum,
            },
        )
        .await;
        for index in [1, 2] {
            self.execute(
                index,
                5,
                PublicOperationProgram::Progress { capability: false },
            )
            .await;
        }
        self.wait_readable("visible", "uncommitted").await;
        for replica in &self.replicas {
            assert_eq!(
                replica
                    .service
                    .persistence()
                    .durable_progress()
                    .await
                    .unwrap(),
                DurableApplicationProgress {
                    applied_lsn: 2,
                    committed_lsn: 2
                }
            );
        }
    }

    async fn finish(mut self) {
        for replica in &self.replicas {
            replica.running.shutdown();
        }
        for replica in &mut self.replicas {
            tokio::time::timeout(Duration::from_secs(15), replica.running.wait())
                .await
                .unwrap()
                .unwrap();
            tokio::time::timeout(Duration::from_secs(5), replica.service.shutdown_and_drain())
                .await
                .unwrap()
                .unwrap();
        }
    }
}

async fn visible_apply_case(secondary: bool) {
    let fixture = Fixture::start(if secondary {
        "sf01-secondary"
    } else {
        "sf01-primary"
    })
    .await;
    fixture.blocked.store(true, Ordering::Release);
    let index = usize::from(secondary);
    let mut gate = fixture.replicas[index]
        .service
        .persistence()
        .pause_next_apply();
    let primary = fixture.replicas[0].service.clone();
    let mut writes = tokio::task::JoinSet::new();
    writes.spawn(async move {
        primary
            .replicate_put("visible".into(), "uncommitted".into())
            .await
    });
    tokio::time::timeout(Duration::from_secs(10), gate.entered())
        .await
        .unwrap();
    if secondary {
        tokio::time::timeout(Duration::from_secs(2), async {
            while fixture.unavailable_attempts.load(Ordering::Acquire) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the independently required third member was not unavailable at the barrier");
    }
    let persistence = fixture.replicas[index].service.persistence();
    assert_eq!(persistence.get("visible").as_deref(), Some("uncommitted"));
    assert_eq!(
        persistence.durable_progress().await.unwrap(),
        DurableApplicationProgress {
            applied_lsn: 2,
            committed_lsn: 1
        }
    );
    fixture.denied(0).await;
    fixture.denied(index).await;
    let report = fixture.replicas[index]
        .running
        .handle()
        .public_v1_report()
        .await
        .unwrap();
    assert_eq!(
        report.progress.last, 1,
        "unreturned/unacknowledged apply is not progress"
    );
    assert_eq!(report.progress.first, 1);
    gate.release(false);
    assert!(
        tokio::time::timeout(Duration::from_secs(10), writes.join_next())
            .await
            .unwrap()
            .unwrap()
            .unwrap()
            .is_err()
    );
    assert!(fixture.unavailable_attempts.load(Ordering::Acquire) > 0);
    fixture.denied(0).await;
    if secondary {
        fixture.denied(1).await;
    }
    fixture.settle().await;
    fixture.finish().await;
}

#[tokio::test]
async fn sf01_primary_visible_apply_denies_normal_reads_until_quorum_settlement() {
    visible_apply_case(false).await;
}

#[tokio::test]
async fn sf01_secondary_visible_apply_denies_normal_reads_with_required_peer_unavailable() {
    visible_apply_case(true).await;
}

#[derive(Clone, Copy)]
enum Incomplete {
    Error,
    Drop,
    Abort,
}

async fn incomplete_apply_case(case: Incomplete) {
    let name = match case {
        Incomplete::Error => "sf01-error",
        Incomplete::Drop => "sf01-drop",
        Incomplete::Abort => "sf01-abort",
    };
    let fixture = Fixture::start(name).await;
    let mut gate = fixture.replicas[0].service.persistence().pause_next_apply();
    let primary = fixture.replicas[0].service.clone();
    let mut writes = tokio::task::JoinSet::new();
    writes.spawn(async move {
        primary
            .replicate_put("visible".into(), "uncommitted".into())
            .await
    });
    tokio::time::timeout(Duration::from_secs(10), gate.entered())
        .await
        .unwrap();
    assert_eq!(
        fixture.replicas[0]
            .service
            .persistence()
            .get("visible")
            .as_deref(),
        Some("uncommitted")
    );
    fixture.denied(0).await;
    match case {
        Incomplete::Drop => {
            writes.abort_all();
            assert!(
                tokio::time::timeout(Duration::from_secs(5), writes.join_next())
                    .await
                    .unwrap()
                    .unwrap()
                    .unwrap_err()
                    .is_cancelled()
            );
            fixture.denied(0).await;
            gate.release(false);
        }
        Incomplete::Error | Incomplete::Abort => {
            if matches!(case, Incomplete::Abort) {
                fixture.execute(0, 5, PublicOperationProgram::Abort).await;
            }
            gate.release(matches!(case, Incomplete::Error));
            assert!(
                tokio::time::timeout(Duration::from_secs(10), writes.join_next())
                    .await
                    .unwrap()
                    .unwrap()
                    .unwrap()
                    .is_err()
            );
            if matches!(case, Incomplete::Abort) {
                assert!(matches!(
                    fixture.replicas[0].service.get("visible").await,
                    Err(RuntimeError::NotOpen)
                ));
            } else {
                fixture.denied(0).await;
            }
        }
    }
    if matches!(case, Incomplete::Error) {
        let persistence = fixture.replicas[0].service.persistence();
        persistence.commit(2).await.unwrap();
        assert_eq!(
            persistence.durable_progress().await.unwrap(),
            DurableApplicationProgress {
                applied_lsn: 2,
                committed_lsn: 2
            }
        );
        fixture
            .execute(0, 5, PublicOperationProgram::Progress { capability: false })
            .await;
        fixture.denied(0).await;
    }
    fixture.finish().await;
}

#[tokio::test]
async fn sf01_apply_error_and_equal_provider_counters_do_not_reopen_normal_reads() {
    incomplete_apply_case(Incomplete::Error).await;
}

#[tokio::test]
async fn sf01_dropped_visible_apply_retains_normal_read_fence() {
    incomplete_apply_case(Incomplete::Drop).await;
}

#[tokio::test]
async fn sf01_abort_during_visible_apply_cannot_reopen_normal_reads() {
    incomplete_apply_case(Incomplete::Abort).await;
}

async fn incomplete_secondary_apply(drop_caller: bool) {
    let fixture = Fixture::start(if drop_caller {
        "sf01-secondary-drop"
    } else {
        "sf01-secondary-error"
    })
    .await;
    let mut gate = fixture.replicas[1].service.persistence().pause_next_apply();
    let primary = fixture.replicas[0].service.clone();
    let mut writes = tokio::task::JoinSet::new();
    writes.spawn(async move {
        primary
            .replicate_put("visible".into(), "uncommitted".into())
            .await
    });
    tokio::time::timeout(Duration::from_secs(10), gate.entered())
        .await
        .unwrap();
    assert_eq!(
        fixture.replicas[1]
            .service
            .persistence()
            .get("visible")
            .as_deref(),
        Some("uncommitted")
    );
    fixture.denied(1).await;
    if drop_caller {
        writes.abort_all();
        assert!(
            tokio::time::timeout(Duration::from_secs(5), writes.join_next())
                .await
                .unwrap()
                .unwrap()
                .unwrap_err()
                .is_cancelled()
        );
        tokio::time::timeout(Duration::from_secs(5), gate.cancelled())
            .await
            .expect("caller loss did not cancel secondary provider work");
        fixture.denied(1).await;
        gate.release(false);
    } else {
        gate.release(true);
        assert!(
            tokio::time::timeout(Duration::from_secs(10), writes.join_next())
                .await
                .unwrap()
                .unwrap()
                .unwrap()
                .is_err()
        );
        fixture.denied(1).await;
    }
    let report = fixture.replicas[1]
        .running
        .handle()
        .public_v1_report()
        .await
        .unwrap();
    assert_eq!((report.progress.last, report.progress.first), (1, 1));
    fixture.settle().await;
    fixture.finish().await;
}

#[tokio::test]
async fn sf01_secondary_apply_error_keeps_normal_reads_fenced_until_fresh_replay() {
    incomplete_secondary_apply(false).await;
}

#[tokio::test]
async fn sf01_secondary_caller_drop_keeps_normal_reads_fenced_until_fresh_replay() {
    incomplete_secondary_apply(true).await;
}
