use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use kuberic_runtime::RuntimeError;
use kuberic_runtime::control::proto as control;
use kuberic_runtime::host::{
    ApplicationStorageState, ReplicaEndpointResolver, ReplicaHost, ReplicaProcessConfig,
    RunningReplica,
};
use kuberic_runtime::protocol::public_operations::*;
use kuberic_runtime::protocol::types::*;
use sqlite_replicated::SqlitePersistence;
use sqlite_replicated::proto;
use sqlite_replicated::proto::sqlite_store_server::SqliteStore as _;
use sqlite_replicated::server::SqliteServer;
use sqlite_replicated::service::SqliteService;

#[derive(Default)]
struct Resolver(Mutex<std::collections::BTreeMap<ReplicaId, String>>);

impl ReplicaEndpointResolver for Resolver {
    fn control_endpoint(&self, identity: &ReplicaIdentity) -> String {
        self.0
            .lock()
            .unwrap()
            .get(&identity.replica_id)
            .cloned()
            .unwrap_or_default()
    }
    fn replication_endpoint(&self, _: &ReplicaIdentity) -> String {
        "http://127.0.0.1:1".into()
    }
}

struct Node {
    running: RunningReplica,
    service: Arc<SqliteService>,
    identity: ReplicaIdentity,
    storage: ReplicaStorageIncarnationContext,
    history: HistoryContext,
    preview: PublicV1Identity,
    revision: u64,
    address: String,
    scope: Option<ReplicaScope>,
    last_build: Option<ReplicaGoal>,
}

fn authorized<T>(message: T) -> tonic::Request<T> {
    let mut request = tonic::Request::new(message);
    request.metadata_mut().insert(
        "authorization",
        "Bearer sqlite-construction-token".parse().unwrap(),
    );
    request
}

fn current_scope() -> &'static Mutex<Option<ReplicaConfigurationScope>> {
    static CURRENT: OnceLock<Mutex<Option<ReplicaConfigurationScope>>> = OnceLock::new();
    CURRENT.get_or_init(Default::default)
}

impl Node {
    async fn start(root: &Path, id: i64, revision: u64) -> Self {
        let resource = ResourceUid::new("sqlite-native-construction");
        let preview = PublicV1Identity::new(1);
        let name = root.file_name().unwrap().to_str().unwrap();
        let pod = PodUid::new(format!("{name}-pod"));
        let pvc = PvcUid::new(format!("{name}-pvc"));
        let initialization = derive_initialization_id(&resource, ReplicaId::new(id), &pod, &pvc);
        let identity = ReplicaIdentity {
            replica_id: ReplicaId::new(id),
            instance_id: ReplicaInstanceId::new(format!("{name}-pod")),
            agent_generation: derive_agent_generation(&initialization),
        };
        let application = root.join("application");
        let fresh = SqlitePersistence::is_fresh_empty(&application).unwrap();
        let paths = std::collections::BTreeMap::from([("application".into(), application.clone())]);
        let service =
            Arc::new(SqliteService::deferred(application, "http://127.0.0.1:0".into()).unwrap());
        static RESOLVER: OnceLock<Arc<Resolver>> = OnceLock::new();
        let resolver = RESOLVER.get_or_init(Default::default).clone();
        let host = ReplicaHost::new(
            ReplicaProcessConfig {
                resource_uid: resource.clone(),
                replica_id: identity.replica_id,
                pod_uid: pod.clone(),
                pvc_uid: pvc.clone(),
                data_root: root.to_path_buf(),
                replication_address: "127.0.0.1:0".parse().unwrap(),
                bearer_token: "sqlite-construction-token".into(),
                rpc_deadline: Duration::from_secs(5),
                transport_window_capacity: 8,
            },
            service.clone(),
            if fresh {
                ApplicationStorageState::FreshEmpty
            } else {
                ApplicationStorageState::Established
            },
            resolver.clone(),
        )
        .with_application_storage_paths(paths);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = format!("http://{}", listener.local_addr().unwrap());
        resolver
            .0
            .lock()
            .unwrap()
            .insert(identity.replica_id, address.clone());
        let startup = tokio::spawn(host.start(listener));
        let mut client = loop {
            match control::agent_control_client::AgentControlClient::connect(address.clone()).await
            {
                Ok(client) => break client,
                Err(_) => tokio::time::sleep(Duration::from_millis(10)).await,
            }
        };
        let status = client
            .get_status(authorized(control::GetAgentStatusRequest {
                protocol_version: kuberic_runtime::protocol::PROTOCOL_VERSION,
                resource_uid: resource.to_string(),
                replica_id: id,
                expected_instance_id: identity.instance_id.to_string(),
            }))
            .await
            .unwrap()
            .into_inner();
        if status.storage_state == control::AgentStorageState::Uninitialized as i32 {
            let members = (1..=id)
                .map(|member| ConfigurationMember {
                    identity: if member == id {
                        identity.clone()
                    } else {
                        ReplicaIdentity {
                            replica_id: ReplicaId::new(member),
                            instance_id: ReplicaInstanceId::new(format!("bootstrap-{member}")),
                            agent_generation: AgentGeneration::new(format!("bootstrap-{member}")),
                        }
                    },
                    role: if member == 1 {
                        ReplicaRole::Primary
                    } else {
                        ReplicaRole::ActiveSecondary
                    },
                })
                .collect();
            client
                .execute(authorized(control::ExecuteCommandRequest {
                    protocol_version: kuberic_runtime::protocol::PROTOCOL_VERSION,
                    resource_uid: resource.to_string(),
                    target: Some(identity.clone().into()),
                    expected_process_session_id: status.process_session_id,
                    command: Some(
                        control::execute_command_request::Command::InitializeAgentStore(
                            control::InitializeAgentStoreCommand {
                                initialization_id: initialization.to_string(),
                                resource_uid: resource.to_string(),
                                local_replica_id: id,
                                expected_instance_id: identity.instance_id.to_string(),
                                expected_pod_uid: pod.to_string(),
                                expected_pvc_uid: pvc.to_string(),
                                assigned_agent_generation: identity.agent_generation.to_string(),
                                effective_policy: Some(
                                    EffectivePolicy::fixed(id as u32, 30).unwrap().into(),
                                ),
                                bootstrap_configuration: Some(
                                    ConfigurationDescriptor::new(
                                        Epoch::new(0, 1),
                                        ReplicaId::new(1),
                                        members,
                                        id as u32 / 2 + 1,
                                    )
                                    .into(),
                                ),
                                provisioning: None,
                            },
                        ),
                    ),
                }))
                .await
                .unwrap();
        }
        let running = startup.await.unwrap().unwrap();
        let response = client
            .get_snapshot(authorized(control::ReplicaSnapshotRequest {
                protocol_version: ORDINARY_ACTION_FORMAT,
                resource_uid: resource.to_string(),
                target: Some(identity.clone().into()),
                expected_process_session_id: String::new(),
                sample_native_progress: false,
            }))
            .await
            .unwrap()
            .into_inner();
        let ReplicaAgentQueryResult::Initialized(snapshot) =
            serde_json::from_slice(&response.canonical_json).unwrap()
        else {
            panic!("initialized snapshot required")
        };
        Self {
            running,
            service,
            identity,
            storage: snapshot.storage,
            history: snapshot.history,
            preview,
            revision,
            address,
            scope: None,
            last_build: None,
        }
    }

    async fn snapshot(&self) -> ReplicaAgentSnapshot {
        self.snapshot_sampling(false).await
    }

    async fn snapshot_sampling(&self, sample_native_progress: bool) -> ReplicaAgentSnapshot {
        let mut client =
            control::agent_control_client::AgentControlClient::connect(self.address.clone())
                .await
                .unwrap();
        let response = client
            .get_snapshot(authorized(control::ReplicaSnapshotRequest {
                protocol_version: ORDINARY_ACTION_FORMAT,
                resource_uid: self.history.resource_uid.to_string(),
                target: Some(self.identity.clone().into()),
                expected_process_session_id: self.running.handle().process_session_id().to_string(),
                sample_native_progress,
            }))
            .await
            .unwrap()
            .into_inner();
        let ReplicaAgentQueryResult::Initialized(snapshot) =
            serde_json::from_slice(&response.canonical_json).unwrap()
        else {
            panic!("ordinary snapshot required")
        };
        *snapshot
    }

    async fn goal(
        &mut self,
        desired: ReplicaGoalKind,
        configuration: ReplicaConfigurationScope,
    ) -> ReplicaGoal {
        let snapshot = self.snapshot().await;
        self.revision = self.revision.max(
            snapshot
                .goals
                .iter()
                .map(|entry| entry.goal.revision)
                .max()
                .unwrap_or(0),
        ) + 1;
        let goal = ReplicaGoal {
            format: ORDINARY_ACTION_FORMAT,
            identity: self.preview.clone(),
            operation_id: OperationId::new(format!(
                "ordinary-{}-{}-{}",
                self.identity.instance_id, snapshot.process_session_id, self.revision
            )),
            revision: self.revision,
            scope: ReplicaScope {
                history: self.history.clone(),
                storage: self.storage.clone(),
                process_session_id: snapshot.process_session_id,
                creation_id: snapshot.creation_id,
                epoch: configuration.current.epoch,
                configuration,
            },
            desired,
        };
        let mut client =
            control::agent_control_client::AgentControlClient::connect(self.address.clone())
                .await
                .unwrap();
        client
            .submit_goal(authorized(control::ReplicaGoalRequest {
                protocol_version: ORDINARY_ACTION_FORMAT,
                canonical_json: serde_json::to_vec(&goal).unwrap(),
            }))
            .await
            .unwrap_or_else(|error| panic!("{goal:?}: {error}"));
        tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                let snapshot = self.snapshot().await;
                if let Some(entry) = snapshot.goals.iter().find(|entry| entry.goal == goal)
                    && entry.status != ReplicaGoalStatus::Pending
                {
                    assert_eq!(entry.status, ReplicaGoalStatus::Achieved, "{goal:?}");
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("ordinary application goal settles");
        self.scope = Some(goal.scope.clone());
        if matches!(
            goal.desired,
            ReplicaGoalKind::Prepare {
                role: ReplicaRole::Primary
            } | ReplicaGoalKind::Reconfigure {
                role: ReplicaRole::Primary,
                ..
            }
        ) {
            *current_scope().lock().unwrap() = Some(goal.scope.configuration.clone());
        }
        goal
    }

    async fn open(&mut self, role: ReplicaRole, epoch: Epoch) {
        let configuration = if role == ReplicaRole::Primary {
            ReplicaConfigurationScope {
                current: ConfigurationDescriptor::new(
                    epoch,
                    self.identity.replica_id,
                    vec![ConfigurationMember {
                        identity: self.identity.clone(),
                        role,
                    }],
                    1,
                ),
                previous: None,
                replicas: Vec::new(),
            }
        } else {
            current_scope().lock().unwrap().clone().unwrap()
        };
        self.goal(ReplicaGoalKind::Prepare { role }, configuration)
            .await;
    }

    async fn descriptor(&self, role: ReplicaRole) -> ReplicaPeerDescription {
        let sampled = self.snapshot_sampling(true).await;
        let publication = sampled.publication.unwrap();
        if matches!(
            publication.role,
            ReplicaRole::Primary | ReplicaRole::ActiveSecondary
        ) {
            assert!(sampled.progress.value().is_some(), "{:?}", sampled.progress);
            assert!(
                sampled.capability.value().is_some(),
                "{:?}",
                sampled.capability
            );
        }
        ReplicaPeerDescription {
            is_up: true,
            is_ready: true,
            identity: self.identity.clone(),
            replication_address: publication.replication_address,
            process_session_id: publication.scope.process_session_id,
            storage_incarnation: self.storage.clone(),
            role,
            current_progress: sampled.progress.value().unwrap_or(INVALID_LSN),
            catch_up_capability: sampled.capability.value().unwrap_or(INVALID_LSN),
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
        self.goal(
            ReplicaGoalKind::Reconfigure {
                role: ReplicaRole::Primary,
                catch_up: Some(PublicCatchUpMode::WriteQuorum),
            },
            ReplicaConfigurationScope {
                current: current.clone(),
                previous: None,
                replicas: Vec::new(),
            },
        )
        .await;
        current
    }

    async fn copy(
        &mut self,
        target: &mut Self,
        epoch: Epoch,
        current: &ConfigurationDescriptor,
    ) -> ReplicaGoal {
        let source = self.snapshot().await.publication.unwrap();
        let target_publication = target.snapshot().await.publication.unwrap();
        assert_eq!(source.scope.epoch, epoch);
        assert_eq!(&source.scope.configuration.current, current);
        let build = self
            .goal(
                ReplicaGoalKind::Build {
                    remote: Box::new(RemoteReplicaSelection {
                        source: source.clone(),
                        target: target_publication,
                        target_control_address: target.address.clone(),
                        generation: 1,
                    }),
                },
                source.scope.configuration,
            )
            .await;
        self.last_build = Some(build.clone());
        build
    }

    async fn pair(&mut self, target: &mut Self, epoch: Epoch) -> ConfigurationDescriptor {
        self.pair_with_activation(target, epoch, true).await
    }

    async fn pair_with_activation(
        &mut self,
        target: &mut Self,
        epoch: Epoch,
        activate: bool,
    ) -> ConfigurationDescriptor {
        let current = ConfigurationDescriptor::new(
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
        if let Some(build) = self.last_build.take() {
            let mut client =
                control::agent_control_client::AgentControlClient::connect(self.address.clone())
                    .await
                    .unwrap();
            client
                .acknowledge_goal(authorized(control::ReplicaGoalRequest {
                    protocol_version: ORDINARY_ACTION_FORMAT,
                    canonical_json: serde_json::to_vec(&ReplicaGoalAcknowledgement {
                        goal: build,
                        logically_retired: false,
                    })
                    .unwrap(),
                }))
                .await
                .unwrap();
        }
        let previous = self.scope.as_ref().unwrap().configuration.current.clone();
        let source_peer = self.descriptor(ReplicaRole::Primary).await;
        let target_peer = target.descriptor(ReplicaRole::ActiveSecondary).await;
        let stages = if previous == current {
            vec![None]
        } else {
            vec![Some(previous), None]
        };
        for previous in stages {
            self.goal(
                ReplicaGoalKind::Reconfigure {
                    role: ReplicaRole::Primary,
                    catch_up: (activate && previous.is_none())
                        .then_some(PublicCatchUpMode::WriteQuorum),
                },
                ReplicaConfigurationScope {
                    current: current.clone(),
                    previous: previous.clone(),
                    replicas: vec![target_peer.clone()],
                },
            )
            .await;
            target
                .goal(
                    ReplicaGoalKind::Reconfigure {
                        role: ReplicaRole::ActiveSecondary,
                        catch_up: None,
                    },
                    ReplicaConfigurationScope {
                        current: current.clone(),
                        previous,
                        replicas: vec![source_peer.clone()],
                    },
                )
                .await;
        }
        current
    }

    async fn close(mut self) {
        self.goal(
            ReplicaGoalKind::Close,
            self.scope.as_ref().unwrap().configuration.clone(),
        )
        .await;
        self.running.shutdown();
        self.running.wait().await.unwrap();
        self.service.shutdown_and_drain().await.unwrap();
    }

    async fn stop(mut self) -> (u64, Option<RuntimeError>) {
        self.running.shutdown();
        self.running.wait().await.unwrap();
        let diagnostic = self.service.shutdown_and_drain().await.err();
        (self.revision, diagnostic)
    }

    async fn stop_failed_publication(mut self) -> (String, RuntimeError) {
        self.running.shutdown();
        let host = self
            .running
            .wait()
            .await
            .expect_err("failed publication must remain failed in the actual host join")
            .to_string();
        let service = self
            .service
            .shutdown_and_drain()
            .await
            .expect_err("failed publication must remain failed in the actual service join");
        assert!(host.contains("injected SQLite service cut"), "{host}");
        assert!(
            service.to_string().contains("injected SQLite service cut"),
            "{service}"
        );
        let repeated = self.service.shutdown_and_drain().await.unwrap_err();
        assert_eq!(repeated.to_string(), service.to_string());
        (host, service)
    }
}

fn root() -> tempfile::TempDir {
    let parent = std::env::var_os("TMPDIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap().join("target"));
    root_in(&parent)
}

fn root_in(parent: &std::path::Path) -> tempfile::TempDir {
    std::fs::create_dir_all(parent).unwrap();
    tempfile::Builder::new()
        .prefix("sqlite-native-")
        .tempdir_in(parent)
        .unwrap()
}

#[test]
fn sqlite_fixture_creates_missing_scratch_parent() {
    let owner = tempfile::tempdir().unwrap();
    let parent = owner.path().join("missing").join("nested");
    assert!(!parent.exists());
    let root = root_in(&parent);
    assert!(root.path().is_dir());
    assert_eq!(root.path().parent(), Some(parent.as_path()));
    assert!(owner.path().is_dir());
}

async fn write(server: &SqliteServer, sql: &str) -> Result<i64, tonic::Status> {
    server
        .execute(tonic::Request::new(proto::ExecuteRequest {
            sql: sql.into(),
            params: Vec::new(),
        }))
        .await
        .map(|response| response.into_inner().lsn)
}

async fn values(server: &SqliteServer) -> Result<Vec<String>, tonic::Status> {
    let response = server
        .query(tonic::Request::new(proto::QueryRequest {
            sql: "SELECT value FROM data ORDER BY id".into(),
            params: Vec::new(),
        }))
        .await?
        .into_inner();
    Ok(response
        .rows
        .into_iter()
        .map(|row| match row.values[0].kind.as_ref().unwrap() {
            proto::value::Kind::TextValue(value) => value.clone(),
            other => panic!("unexpected SQL result: {other:?}"),
        })
        .collect())
}

async fn baseline(source: &Node) -> i64 {
    let server = SqliteServer::new(source.service.clone());
    write(
        &server,
        "CREATE TABLE data(id INTEGER PRIMARY KEY, value TEXT NOT NULL)",
    )
    .await
    .unwrap();
    write(&server, "INSERT INTO data VALUES(1,'acknowledged')")
        .await
        .unwrap()
}

#[tokio::test]
async fn sqlite_native_copy_waits_for_service_commit_and_copy_eof_before_activation() {
    tokio::time::timeout(Duration::from_secs(90), async {
        let root = root();
        let epoch = Epoch::new(0, 1);
        let mut source = Node::start(&root.path().join("source"), 1, 0).await;
        let mut target = Node::start(&root.path().join("target"), 2, 0).await;
        source.open(ReplicaRole::Primary, epoch).await;
        let single = source.single(epoch).await;
        target.open(ReplicaRole::IdleSecondary, epoch).await;
        source.copy(&mut target, epoch, &single).await;
        let current = source.pair(&mut target, epoch).await;
        let baseline = baseline(&source).await;
        let server = SqliteServer::new(source.service.clone());
        let committer = source.service.committer_for_test().unwrap();
        committer.before_commit().arm();
        let client = server.clone();
        let write =
            tokio::spawn(
                async move { write(&client, "INSERT INTO data VALUES(2,'isolated')").await },
            );
        committer.before_commit().wait_entered().await;
        assert!(!write.is_finished());
        assert!(values(&server).await.is_err());
        assert_eq!(
            source
                .service
                .persistence()
                .progress()
                .unwrap()
                .committed_lsn,
            baseline
        );
        let report = source.snapshot().await;
        assert_eq!(report.read_status, AccessStatus::Granted);
        assert!(matches!(
            report.progress,
            ProgressQueryResult::NotQueried(_)
        ));
        let mut replacement = Node::start(&root.path().join("replacement"), 3, 0).await;
        replacement.open(ReplicaRole::IdleSecondary, epoch).await;
        source.service.copy_snapshot_gate().arm();
        let service = source.service.clone();
        let committed = {
            let copy = source.copy(&mut replacement, epoch, &current);
            tokio::pin!(copy);
            assert!(
                tokio::time::timeout(Duration::from_millis(75), &mut copy)
                    .await
                    .is_err(),
                "copy cannot snapshot ahead of primary service-local commitment"
            );
            committer.before_commit().release();
            let committed = write.await.unwrap().unwrap();
            assert!(committed > baseline);
            assert!(
                tokio::time::timeout(Duration::from_millis(75), &mut copy)
                    .await
                    .is_err(),
                "copy must await the real snapshot stream and EOF"
            );
            service.copy_snapshot_gate().wait_entered().await;
            service.copy_snapshot_gate().release();
            copy.await;
            committed
        };
        assert_eq!(
            replacement
                .service
                .persistence()
                .progress()
                .unwrap()
                .committed_lsn,
            committed
        );
        assert_eq!(
            replacement
                .service
                .persistence()
                .snapshot(committed)
                .unwrap(),
            source.service.persistence().snapshot(committed).unwrap()
        );
        assert!(
            values(&SqliteServer::new(replacement.service.clone()))
                .await
                .is_err(),
            "stable native secondary reads remain NotPrimary"
        );
        assert_eq!(values(&server).await.unwrap(), ["acknowledged", "isolated"]);
        tokio::join!(source.close(), target.close(), replacement.close());
    })
    .await
    .expect("bounded native SQLite copy/local commitment journey");
}

#[tokio::test]
async fn sqlite_fresh_restart_build_and_corrective_empty_replacement() {
    tokio::time::timeout(Duration::from_secs(120), async {
        let root = root();
        let source_root = root.path().join("source");
        let retired_root = root.path().join("retired");
        let mut source = Node::start(&source_root, 1, 0).await;
        let mut retired = Node::start(&retired_root, 2, 0).await;
        let epoch = Epoch::new(0, 1);
        source.open(ReplicaRole::Primary, epoch).await;
        let single = source.single(epoch).await;
        retired.open(ReplicaRole::IdleSecondary, epoch).await;
        source.copy(&mut retired, epoch, &single).await;
        source.pair(&mut retired, epoch).await;
        let acknowledged = baseline(&source).await;
        let old_service = retired.service.clone();
        let old_session = retired.running.handle().process_session_id();
        let (retired_revision, diagnostic) = retired.stop().await;
        assert!(diagnostic.is_none(), "{diagnostic:?}");
        let (source_revision, diagnostic) = source.stop().await;
        assert!(diagnostic.is_none(), "{diagnostic:?}");
        let mut source = Node::start(&source_root, 1, source_revision).await;
        let mut retired = Node::start(&retired_root, 2, retired_revision).await;
        assert_ne!(retired.running.handle().process_session_id(), old_session);
        let epoch = Epoch::new(0, 2);
        source.open(ReplicaRole::Primary, epoch).await;
        let single = source.single(epoch).await;
        retired.open(ReplicaRole::IdleSecondary, epoch).await;
        source.copy(&mut retired, epoch, &single).await;
        source.pair(&mut retired, epoch).await;
        let server = SqliteServer::new(source.service.clone());
        assert_eq!(values(&server).await.unwrap(), ["acknowledged"]);
        assert!(
            write(
                &SqliteServer::new(old_service.clone()),
                "INSERT INTO data VALUES(99,'retired')"
            )
            .await
            .is_err()
        );
        assert!(
            values(&SqliteServer::new(old_service.clone()))
                .await
                .is_err()
        );
        let (_, diagnostic) = retired.stop().await;
        assert!(diagnostic.is_none(), "{diagnostic:?}");
        let (source_revision, diagnostic) = source.stop().await;
        assert!(diagnostic.is_none(), "{diagnostic:?}");
        let mut source = Node::start(&source_root, 1, source_revision).await;
        let epoch = Epoch::new(0, 3);
        source.open(ReplicaRole::Primary, epoch).await;
        let single = source.single(epoch).await;
        let server = SqliteServer::new(source.service.clone());
        let mut replacement = Node::start(&root.path().join("fresh-replacement"), 2, 0).await;
        assert_ne!(replacement.identity, old_service_identity(&retired_root, 2));
        replacement.open(ReplicaRole::IdleSecondary, epoch).await;
        source.copy(&mut replacement, epoch, &single).await;
        source.pair(&mut replacement, epoch).await;
        assert_eq!(
            replacement
                .service
                .persistence()
                .progress()
                .unwrap()
                .committed_lsn,
            acknowledged
        );
        let later = write(&server, "INSERT INTO data VALUES(2,'replacement')")
            .await
            .unwrap();
        assert!(later > acknowledged);
        assert_eq!(
            replacement.service.persistence().snapshot(later).unwrap(),
            source.service.persistence().snapshot(later).unwrap()
        );
        assert!(
            write(
                &SqliteServer::new(old_service),
                "INSERT INTO data VALUES(100,'late-old-owner')"
            )
            .await
            .is_err()
        );
        tokio::join!(source.close(), replacement.close());
    })
    .await
    .expect("bounded restart and fresh storage replacement journey");
}

fn old_service_identity(root: &Path, id: i64) -> ReplicaIdentity {
    let name = root.file_name().unwrap().to_str().unwrap();
    ReplicaIdentity {
        replica_id: ReplicaId::new(id),
        instance_id: ReplicaInstanceId::new(format!("{name}-pod")),
        agent_generation: derive_agent_generation(&derive_initialization_id(
            &ResourceUid::new("sqlite-native-construction"),
            ReplicaId::new(id),
            &PodUid::new(format!("{name}-pod")),
            &PvcUid::new(format!("{name}-pvc")),
        )),
    }
}

#[tokio::test]
async fn sqlite_real_provider_restart_settlement_and_handoff() {
    use sqlite_replicated::v1_test_bridge::FailureCut;
    tokio::time::timeout(Duration::from_secs(120), async {
        let root = root();
        let a = root.path().join("source");
        let b = root.path().join("secondary");
        let mut source = Node::start(&a, 1, 0).await;
        let mut target = Node::start(&b, 2, 0).await;
        let epoch = Epoch::new(0, 1);
        source.open(ReplicaRole::Primary, epoch).await;
        let single = source.single(epoch).await;
        target.open(ReplicaRole::IdleSecondary, epoch).await;
        source.copy(&mut target, epoch, &single).await;
        source.pair(&mut target, epoch).await;
        let acknowledged = baseline(&source).await;
        let old_service = source.service.clone();
        let (ar, diagnostic) = source.stop().await;
        assert!(diagnostic.is_none(), "{diagnostic:?}");
        let (br, diagnostic) = target.stop().await;
        assert!(diagnostic.is_none(), "{diagnostic:?}");
        let mut source = Node::start(&a, 1, ar).await;
        let mut target = Node::start(&b, 2, br).await;
        let epoch = Epoch::new(0, 2);
        source.open(ReplicaRole::Primary, epoch).await;
        let single = source.single(epoch).await;
        target.open(ReplicaRole::IdleSecondary, epoch).await;
        source.copy(&mut target, epoch, &single).await;
        source.pair(&mut target, epoch).await;
        assert_eq!(
            values(&SqliteServer::new(source.service.clone()))
                .await
                .unwrap(),
            ["acknowledged"]
        );
        assert_eq!(
            source
                .service
                .persistence()
                .progress()
                .unwrap()
                .committed_lsn,
            acknowledged
        );
        let epoch = Epoch::new(0, 3);
        let promoted = ConfigurationDescriptor::new(
            epoch,
            target.identity.replica_id,
            vec![
                ConfigurationMember {
                    identity: source.identity.clone(),
                    role: ReplicaRole::ActiveSecondary,
                },
                ConfigurationMember {
                    identity: target.identity.clone(),
                    role: ReplicaRole::Primary,
                },
            ],
            2,
        );
        source
            .goal(
                ReplicaGoalKind::Reconfigure {
                    role: ReplicaRole::ActiveSecondary,
                    catch_up: None,
                },
                ReplicaConfigurationScope {
                    current: promoted.clone(),
                    previous: None,
                    replicas: vec![target.descriptor(ReplicaRole::Primary).await],
                },
            )
            .await;
        target
            .goal(
                ReplicaGoalKind::Reconfigure {
                    role: ReplicaRole::Primary,
                    catch_up: None,
                },
                ReplicaConfigurationScope {
                    current: promoted,
                    previous: None,
                    replicas: vec![source.descriptor(ReplicaRole::ActiveSecondary).await],
                },
            )
            .await;
        let current = target.pair_with_activation(&mut source, epoch, false).await;
        assert_eq!(
            target
                .running
                .handle()
                .public_v1_report()
                .await
                .unwrap()
                .write_status,
            AccessStatus::ReconfigurationPending
        );
        target
            .goal(
                ReplicaGoalKind::Reconfigure {
                    role: ReplicaRole::Primary,
                    catch_up: Some(PublicCatchUpMode::All),
                },
                ReplicaConfigurationScope {
                    current,
                    previous: None,
                    replicas: vec![source.descriptor(ReplicaRole::ActiveSecondary).await],
                },
            )
            .await;
        let server = SqliteServer::new(target.service.clone());
        let promoted = write(&server, "INSERT INTO data VALUES(2,'promoted')")
            .await
            .unwrap();
        assert!(promoted > acknowledged);
        assert_eq!(values(&server).await.unwrap(), ["acknowledged", "promoted"]);
        assert!(
            values(&SqliteServer::new(source.service.clone()))
                .await
                .is_err()
        );
        assert!(
            write(
                &SqliteServer::new(source.service.clone()),
                "INSERT INTO data VALUES(99,'old-primary')"
            )
            .await
            .is_err()
        );
        assert!(
            write(
                &SqliteServer::new(old_service),
                "INSERT INTO data VALUES(100,'retired-service')"
            )
            .await
            .is_err()
        );
        target
            .service
            .committer_for_test()
            .unwrap()
            .fail_once(FailureCut::Publication);
        assert!(
            write(&server, "INSERT INTO data VALUES(3,'ambiguous')")
                .await
                .is_err()
        );
        assert!(values(&server).await.is_err());
        assert!(
            target
                .service
                .persistence()
                .progress()
                .unwrap()
                .committed_lsn
                > promoted
        );
        assert!(
            write(&server, "INSERT INTO data VALUES(4,'late-success')")
                .await
                .is_err()
        );
        let (host_diagnostic, diagnostic) = target.stop_failed_publication().await;
        assert!(
            host_diagnostic.contains("Application Close failed"),
            "{host_diagnostic}"
        );
        let RuntimeError::Application(diagnostic) = diagnostic else {
            panic!("publication failure lost its application diagnostic: {diagnostic:?}");
        };
        assert!(
            diagnostic.contains("injected SQLite service cut"),
            "{diagnostic}"
        );
        source.close().await;
    })
    .await
    .expect("bounded publication failure/restart/handoff journey");
}
