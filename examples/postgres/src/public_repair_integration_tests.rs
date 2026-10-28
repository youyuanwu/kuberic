//! Manually selected canonical goals, not controller handoff selection.
use super::*;
use crate::service::{PgService, PgServiceConfig};
use kuberic_runtime::control::proto;
use kuberic_runtime::host::{
    ReplicaEndpointResolver, ReplicaHost, ReplicaProcessConfig, RunningReplica,
};
use kuberic_runtime::protocol::public_operations::*;
use kuberic_runtime::protocol::types::*;
use std::mem::ManuallyDrop;

const TOKEN: &str = "native-repair-owned-fixture";

#[derive(Default)]
struct Routes(std::sync::Mutex<BTreeMap<ReplicaId, String>>);
impl ReplicaEndpointResolver for Routes {
    fn control_endpoint(&self, identity: &ReplicaIdentity) -> String {
        self.0
            .lock()
            .unwrap()
            .get(&identity.replica_id)
            .cloned()
            .unwrap_or_default()
    }
    fn replication_endpoint(&self, identity: &ReplicaIdentity) -> String {
        self.control_endpoint(identity)
    }
}

fn auth<T>(value: T) -> tonic::Request<T> {
    let mut request = tonic::Request::new(value);
    request
        .metadata_mut()
        .insert("authorization", format!("Bearer {TOKEN}").parse().unwrap());
    request
}

struct Member {
    running: RunningReplica,
    service: Arc<PgService>,
    control: String,
    goal: ReplicaGoal,
}

impl Member {
    async fn new(root: std::path::PathBuf, id: i64, routes: Arc<Routes>) -> Self {
        let resource = ResourceUid::new("native-repair-canonical");
        let pod = PodUid::new(format!("repair-pod-{id}"));
        let pvc = PvcUid::new(format!("repair-pvc-{id}"));
        let identity = native_identity(id, pod.as_str());
        let preview = PublicV1Identity::new(1);
        let config = PgServiceConfig {
            resource_uid: resource.clone(),
            application_root: root.join("a"),
            pg_data: root.join("d"),
            pg_bin: find_pg_bin(),
            pg_port: allocate_port().await,
            replication_address: format!("http://{}", host_address()),
        };
        let paths = PgService::storage_paths(&config);
        let database =
            kuberic_runtime::testing::sqlite_store::SqliteStore::metadata_database_path(&root);
        drop(kuberic_runtime::testing::sqlite_store::SqliteStore::create_public_v1_with_application_paths(
            &database, preview.clone(),
            kuberic_runtime::testing::state::StorageIdentity {
                schema_version: kuberic_runtime::testing::state::SCHEMA_VERSION,
                resource_uid: resource.clone(), pod_uid: pod.clone(), pvc_uid: pvc.clone(),
                initialization_id: derive_initialization_id(&resource, identity.replica_id, &pod, &pvc),
                local_identity: identity.clone(), effective_policy: EffectivePolicy::fixed(2, 30).unwrap(),
            }, paths.clone(),
        ).unwrap());
        let service = Arc::new(PgService::deferred(config).with_coordination_token(TOKEN.into()));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let control = format!("http://{}", listener.local_addr().unwrap());
        routes
            .0
            .lock()
            .unwrap()
            .insert(identity.replica_id, control.clone());
        let running = ReplicaHost::new(
            ReplicaProcessConfig {
                resource_uid: resource.clone(),
                replica_id: identity.replica_id,
                pod_uid: pod.clone(),
                pvc_uid: pvc.clone(),
                data_root: root,
                replication_address: host_address(),
                bearer_token: TOKEN.into(),
                rpc_deadline: Duration::from_secs(2),
                transport_window_capacity: 8,
            },
            service.clone(),
            kuberic_runtime::host::ApplicationStorageState::FreshEmpty,
            routes,
        )
        .with_application_storage_paths(paths)
        .start(listener)
        .await
        .unwrap();
        let handle = running.handle();
        let generation = PublicProtocolGeneration::new(1);
        let initialization = derive_history_initialization_id(&resource, generation);
        let history = derive_bootstrap_history(&resource, generation, &initialization);
        let storage = ReplicaStorageIncarnationContext {
            replica: identity.clone(),
            storage: ReplicaStorageIdentity::Persistent {
                pod_uid: pod,
                pvc_uid: pvc,
            },
            storage_generation: StorageGeneration::new(format!("repair-storage-{id}")),
        };
        handle
            .initialize_public_v1_history(&initialization, &history, &storage)
            .await
            .unwrap();
        let endpoint = handle
            .public_v1_process_endpoint(ReplicaRole::None)
            .await
            .unwrap();
        let goal = ReplicaGoal {
            format: ORDINARY_ACTION_FORMAT,
            identity: preview,
            operation_id: OperationId::new(format!("prepare-{id}")),
            revision: 1,
            scope: ReplicaScope {
                history,
                storage,
                process_session_id: handle.process_session_id(),
                creation_id: endpoint.replicator_creation_id,
                epoch: Epoch::new(0, 1),
                configuration: ReplicaConfigurationScope {
                    current: native_configuration(std::slice::from_ref(&identity), 0, 1),
                    previous: None,
                    replicas: Vec::new(),
                },
            },
            desired: ReplicaGoalKind::Prepare {
                role: ReplicaRole::Primary,
                rebuild: None,
            },
        };
        Self {
            running,
            service,
            control,
            goal,
        }
    }

    fn driver(&self) -> &Arc<PgReplicator> {
        self.service.native_driver()
    }

    async fn application(&self) -> (tokio_postgres::Client, crate::owned_task::PgTask) {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Ok(connection) = self.service.instance().connect_application().await {
                    return connection;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap()
    }

    async fn snapshot(&self) -> ReplicaAgentSnapshot {
        let mut client =
            proto::agent_control_client::AgentControlClient::connect(self.control.clone())
                .await
                .unwrap();
        let reply = client
            .get_snapshot(auth(proto::ReplicaSnapshotRequest {
                protocol_version: ORDINARY_ACTION_FORMAT,
                resource_uid: self.goal.scope.history.resource_uid.to_string(),
                target: Some(self.goal.scope.storage.replica.clone().into()),
                expected_process_session_id: self.goal.scope.process_session_id.to_string(),
                sample_native_progress: false,
            }))
            .await
            .unwrap()
            .into_inner();
        let ReplicaAgentQueryResult::Initialized(snapshot) =
            serde_json::from_slice(&reply.canonical_json).unwrap()
        else {
            panic!("initialized owner required")
        };
        *snapshot
    }

    async fn run(&mut self, configuration: ReplicaConfigurationScope, desired: ReplicaGoalKind) {
        let status = self.run_result(configuration, desired).await;
        assert_eq!(
            status,
            ReplicaGoalStatus::Achieved,
            "canonical repair goal failed: {:?}; goal={:?}",
            status,
            self.goal
        );
    }

    async fn run_result(
        &mut self,
        mut configuration: ReplicaConfigurationScope,
        desired: ReplicaGoalKind,
    ) -> ReplicaGoalStatus {
        configuration
            .replicas
            .retain(|peer| peer.identity != self.goal.scope.storage.replica);
        if self.goal.revision > 1
            || self
                .snapshot()
                .await
                .goals
                .iter()
                .any(|entry| entry.goal == self.goal)
        {
            self.goal.revision += 1;
            self.goal.operation_id = OperationId::new(format!(
                "repair-{}-{}",
                self.goal.scope.storage.replica.replica_id, self.goal.revision
            ));
        }
        self.goal.scope.epoch = configuration.current.epoch;
        self.goal.scope.configuration = configuration;
        self.goal.desired = desired;
        self.goal.validate().unwrap();
        let mut client =
            proto::agent_control_client::AgentControlClient::connect(self.control.clone())
                .await
                .unwrap();
        client
            .submit_goal(auth(proto::ReplicaGoalRequest {
                protocol_version: ORDINARY_ACTION_FORMAT,
                canonical_json: serde_json::to_vec(&self.goal).unwrap(),
            }))
            .await
            .unwrap();
        let status = tokio::time::timeout(Duration::from_secs(90), async {
            loop {
                let snapshot = self.snapshot().await;
                if let Some(entry) = snapshot.goals.iter().find(|entry| entry.goal == self.goal) {
                    match &entry.status {
                        ReplicaGoalStatus::Achieved => break entry.status.clone(),
                        ReplicaGoalStatus::Pending => {}
                        other => break other.clone(),
                    }
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        if status != ReplicaGoalStatus::Achieved {
            return status;
        }
        client
            .acknowledge_goal(auth(proto::ReplicaGoalRequest {
                protocol_version: ORDINARY_ACTION_FORMAT,
                canonical_json: serde_json::to_vec(&ReplicaGoalAcknowledgement {
                    goal: self.goal.clone(),
                    logically_retired: false,
                })
                .unwrap(),
            }))
            .await
            .unwrap();
        status
    }

    async fn stop(mut self) {
        self.running.shutdown();
        self.running.wait().await.unwrap();
    }
}

async fn scope(
    current: ConfigurationDescriptor,
    previous: Option<ConfigurationDescriptor>,
    members: &[(&Member, ReplicaRole)],
) -> ReplicaConfigurationScope {
    let mut peers = Vec::new();
    for (member, role) in members {
        let current_progress = member.driver().current_progress().await.unwrap();
        let catch_up_capability = member.driver().catch_up_capability().await.unwrap();
        peers.push(ReplicaPeerDescription {
            identity: member.goal.scope.storage.replica.clone(),
            replication_address: member.driver().coordination.local_endpoint.clone(),
            process_session_id: member.goal.scope.process_session_id.clone(),
            storage_incarnation: member.goal.scope.storage.clone(),
            role: *role,
            current_progress,
            catch_up_capability,
            is_up: member.service.instance().is_running().await,
            is_ready: current_progress >= 0 && catch_up_capability >= 0,
            must_catchup: false,
        });
    }
    ReplicaConfigurationScope {
        current,
        previous,
        replicas: peers,
    }
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
#[serial_test::serial]
async fn m3_pg_selected_native_repair_canonical_pair_preserves_full_quorum_sql() {
    canonical_repair(RepairFault::None).await;
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
#[serial_test::serial]
async fn m3_pg_selected_native_repair_real_helper_fault_retains_original_and_sticky_failure() {
    canonical_repair(RepairFault::MissingCandidateWal).await;
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
#[serial_test::serial]
async fn m3_pg_selected_native_repair_cancellation_joins_actual_helper_before_successor() {
    canonical_repair(RepairFault::CancelHelper).await;
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
#[serial_test::serial]
async fn m3_pg_selected_native_repair_false_candidate_inode_never_grants_file_read() {
    canonical_repair(RepairFault::FalseCandidate).await;
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
#[serial_test::serial]
async fn m3_pg_selected_native_repair_original_move_gap_reopens_exact_paths_without_readiness() {
    canonical_repair(RepairFault::OriginalMoveGap).await;
}

#[test_log::test(tokio::test(flavor = "multi_thread", worker_threads = 4))]
#[serial_test::serial]
async fn m3_pg_selected_native_repair_install_move_gap_reopens_exact_paths_without_rollback() {
    canonical_repair(RepairFault::InstallMoveGap).await;
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RepairFault {
    None,
    MissingCandidateWal,
    CancelHelper,
    FalseCandidate,
    OriginalMoveGap,
    InstallMoveGap,
}

async fn reopen_closed_repair(
    root: std::path::PathBuf,
    identity: PgDurableIdentity,
    offer: session::repair::RepairOffer,
    original_tree: String,
    expected_inode: u64,
) {
    let durable = Arc::new(
        PgDurableStore::open(root.join("a"), identity.clone(), StorageMode::Established)
            .await
            .unwrap(),
    );
    let instance = Arc::new(PgInstanceManager::new(
        root.join("d"),
        find_pg_bin(),
        allocate_port().await,
    ));
    let (fault_tx, _) = mpsc::channel(8);
    let address = format!("http://{}", host_address());
    let driver = Arc::new(
        PgReplicator::new(
            instance.clone(),
            durable.clone(),
            fault_tx,
            Weak::new(),
            CancellationToken::new(),
            crate::data_service::PgCoordination {
                local_endpoint: address.clone(),
                bearer_token: TOKEN.into(),
                listen_address: None,
            },
            false,
        )
        .await,
    );
    assert!(
        driver
            .public_v1
            .set(PublicPg::new(
                identity.replica,
                ProcessSessionId::new("repair-resolver-reopened"),
                address
            ))
            .is_ok()
    );
    driver
        .public_owner()
        .driver
        .set(Arc::downgrade(&driver))
        .unwrap();
    Replicator::open(driver.as_ref()).await.unwrap();
    let state = durable.revalidate().await.unwrap();
    assert!(state.quiescence.is_some() && state.external_access_closed && state.postgres_stopped);
    let record = serde_json::to_value(state.native_reformations.last().unwrap()).unwrap();
    assert_eq!(record["repair"]["stage"], "installed");
    assert!(record["repair"]["failure"].is_string());
    let archive = std::path::PathBuf::from(record["repair"]["archivePath"].as_str().unwrap());
    assert_eq!(
        session::repair::retained_tree_digest(&archive).unwrap(),
        original_tree
    );
    {
        use std::os::unix::fs::MetadataExt;
        assert_eq!(
            std::fs::metadata(instance.data_dir()).unwrap().ino(),
            expected_inode
        );
    }
    assert!(!instance.is_running().await);
    assert!(driver.public_start().await.is_err());
    assert!(
        driver
            .public_request(Request::RepairConfirm(offer))
            .await
            .is_err()
    );
    driver.pg_recover_repair_moves().await.unwrap();
    {
        use std::os::unix::fs::MetadataExt;
        assert_eq!(
            std::fs::metadata(instance.data_dir()).unwrap().ino(),
            expected_inode
        );
    }
    Replicator::close(driver.as_ref()).await.unwrap();
    eprintln!(
        "matching native metadata2 repair resolver reopened with access/old authority closed; root={}",
        root.display()
    );
}

async fn offer_for(target: &PgReplicator) -> (session::repair::RepairOffer, serde_json::Value) {
    let durable = target.durable.revalidate().await.unwrap();
    let record = serde_json::to_value(durable.native_reformations.last().unwrap()).unwrap();
    let id: session::PgSessionId = serde_json::from_value(record["session"].clone()).unwrap();
    let binding = PeerBinding {
        epoch: id.source.scope.epoch,
        previous_configuration_id: id
            .source
            .scope
            .configuration
            .previous
            .as_ref()
            .map(|previous| previous.configuration_id.clone()),
        current_configuration_id: id
            .source
            .scope
            .configuration
            .current
            .configuration_id
            .clone(),
        primary: NativeEndpoint {
            replica: id.source.scope.storage.replica.clone(),
            process_session_id: id.source.scope.process_session_id.clone(),
            replication_address: id.source.replication_address.clone(),
            role: ReplicaRole::Primary,
        },
        secondary: NativeEndpoint {
            replica: id.target.scope.storage.replica.clone(),
            process_session_id: id.target.scope.process_session_id.clone(),
            replication_address: id.target.replication_address.clone(),
            role: ReplicaRole::ActiveSecondary,
        },
        session: Some(Box::new(id)),
    };
    let offer = serde_json::json!({
        "binding": binding,
        "source_lineage": record["sourceLineage"],
        "retained_lineage": record["retainedLineage"],
        "certified_lsn": record["requiredCertifiedLsn"],
        "source_boundary": record["sourceBoundary"],
        "candidate_nonce": record["repair"]["nonce"],
    });
    (serde_json::from_value(offer).unwrap(), record)
}

async fn actual_rewind_child() -> u32 {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let mut owners = vec![std::process::id()];
            while let Some(parent) = owners.pop() {
                let Ok(threads) = std::fs::read_dir(format!("/proc/{parent}/task")) else {
                    continue;
                };
                for thread in threads.flatten() {
                    let children =
                        std::fs::read_to_string(thread.path().join("children")).unwrap_or_default();
                    for pid in children
                        .split_whitespace()
                        .filter_map(|pid| pid.parse::<u32>().ok())
                    {
                        if std::fs::read_to_string(format!("/proc/{pid}/comm"))
                            .is_ok_and(|name| name.trim() == "pg_rewind")
                        {
                            return pid;
                        }
                        owners.push(pid);
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap()
}

async fn canonical_repair(fault: RepairFault) {
    // Keep every store/candidate/original, including failed test attempts.
    let root = ManuallyDrop::new(crate::testing::TestDataDir::new("pg-owner-repair"));
    eprintln!("native repair retained root {}", root.path().display());
    let routes = Arc::new(Routes::default());
    let mut old = Member::new(root.path().join("o"), 1, routes.clone()).await;
    let mut new = Member::new(root.path().join("n"), 2, routes).await;
    let singleton = old.goal.scope.configuration.clone();
    old.run(
        singleton.clone(),
        ReplicaGoalKind::Prepare {
            role: ReplicaRole::Primary,
            rebuild: None,
        },
    )
    .await;
    // Retention is a test prerequisite, set before copy/divergence; never a
    // production retention override or a rewind fallback.
    let (admin, connection) = old.service.instance().connect().await.unwrap();
    admin
        .simple_query("ALTER SYSTEM SET wal_keep_size = '256MB'")
        .await
        .unwrap();
    admin.simple_query("SELECT pg_reload_conf()").await.unwrap();
    drop(admin);
    connection.await.unwrap();
    old.run(
        singleton.clone(),
        ReplicaGoalKind::Reconfigure {
            role: ReplicaRole::Primary,
            catch_up: Some(PublicCatchUpMode::WriteQuorum),
        },
    )
    .await;
    let (sql, connection) = tokio_postgres::connect(
        &old.service
            .instance()
            .connection_string()
            .replace("dbname=postgres", "dbname=kuberic"),
        tokio_postgres::NoTls,
    )
    .await
    .unwrap();
    let connection = tokio::spawn(async move {
        connection.await.unwrap();
    });
    sql.batch_execute("SET ROLE kuberic_app; CREATE TABLE repair_prefix(id integer PRIMARY KEY, value text); INSERT INTO repair_prefix VALUES(1, 'acknowledged-before-copy')").await.unwrap();
    drop(sql);
    connection.await.unwrap();
    new.run(
        singleton.clone(),
        ReplicaGoalKind::Prepare {
            role: ReplicaRole::IdleSecondary,
            rebuild: None,
        },
    )
    .await;
    let source = old.snapshot().await.publication.unwrap();
    let target = new.snapshot().await.publication.unwrap();
    let remote = RemoteReplicaSelection {
        original_member: None,
        source,
        target,
        target_control_address: new.control.clone(),
        generation: 1,
    };
    old.run(
        singleton,
        ReplicaGoalKind::Build {
            remote: Box::new(remote),
        },
    )
    .await;
    let ids = [
        old.goal.scope.storage.replica.clone(),
        new.goal.scope.storage.replica.clone(),
    ];
    let pair = native_configuration(&ids, 0, 1);
    let joint = scope(
        pair.clone(),
        Some(old.goal.scope.configuration.current.clone()),
        &[
            (&old, ReplicaRole::Primary),
            (&new, ReplicaRole::ActiveSecondary),
        ],
    )
    .await;
    old.run(
        joint.clone(),
        ReplicaGoalKind::Reconfigure {
            role: ReplicaRole::Primary,
            catch_up: Some(PublicCatchUpMode::WriteQuorum),
        },
    )
    .await;
    new.run(
        joint,
        ReplicaGoalKind::Reconfigure {
            role: ReplicaRole::ActiveSecondary,
            catch_up: None,
        },
    )
    .await;
    let current = scope(
        pair.clone(),
        None,
        &[
            (&old, ReplicaRole::Primary),
            (&new, ReplicaRole::ActiveSecondary),
        ],
    )
    .await;
    new.run(
        current.clone(),
        ReplicaGoalKind::Reconfigure {
            role: ReplicaRole::ActiveSecondary,
            catch_up: None,
        },
    )
    .await;
    old.run(
        current,
        ReplicaGoalKind::Reconfigure {
            role: ReplicaRole::Primary,
            catch_up: Some(PublicCatchUpMode::WriteQuorum),
        },
    )
    .await;
    let (sql, connection) = old.application().await;
    sql.batch_execute("INSERT INTO repair_prefix VALUES(2, 'acknowledged-full-original-quorum')")
        .await
        .unwrap();
    drop(sql);
    connection.await.unwrap();
    old.driver().public_snapshot_and_persist().await.unwrap();
    let certified = old
        .driver()
        .durable
        .revalidate()
        .await
        .unwrap()
        .policy_certified_lsn;
    assert!(certified > 0);
    let previous_sessions = old.driver().pg_session_ids_for_target(ReplicaId::new(2));
    let native = ProcessProbe::postgres(old.service.instance().data_dir());
    let selected = native_configuration(&ids, 1, 2);
    let selected_scope = scope(
        selected.clone(),
        Some(pair.clone()),
        &[
            (&old, ReplicaRole::ActiveSecondary),
            (&new, ReplicaRole::Primary),
        ],
    )
    .await;
    // Canonical source handoff runs actual All, native stop and separate
    // application demotion. The test, not the controller, selects promotion.
    old.run(
        selected_scope.clone(),
        ReplicaGoalKind::Reconfigure {
            role: ReplicaRole::ActiveSecondary,
            catch_up: None,
        },
    )
    .await;
    native.assert_reaped();
    assert_eq!(old.driver().current_progress().await.unwrap(), -1);
    assert!(old.service.instance().connect_application().await.is_err());
    assert!(
        old.driver()
            .durable
            .revalidate()
            .await
            .unwrap()
            .quiescence
            .is_some()
    );
    let original_tree =
        session::repair::retained_tree_digest(old.service.instance().data_dir()).unwrap();
    let original_inode = {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata(old.service.instance().data_dir())
            .unwrap()
            .ino()
    };
    let selected_scope = scope(
        selected.clone(),
        Some(pair.clone()),
        &[
            (&old, ReplicaRole::ActiveSecondary),
            (&new, ReplicaRole::Primary),
        ],
    )
    .await;
    assert!(
        selected_scope
            .replicas
            .iter()
            .find(|peer| peer.identity == ids[0])
            .is_some_and(|peer| !peer.is_up && !peer.is_ready && peer.current_progress == -1)
    );
    let target_driver = old.driver().clone();
    let source_driver = new.driver().clone();
    let missing = ReplicaSetConfiguration {
        configuration: selected.clone(),
        replicas: Vec::new(),
    };
    let previous = ReplicaSetConfiguration {
        configuration: pair.clone(),
        replicas: Vec::new(),
    };
    let before_selection = source_driver.durable.revalidate().await.unwrap();
    assert!(
        source_driver
            .public_current_native_peers(&missing, Some(&previous))
            .await
            .is_err(),
        "an unselected future configuration cannot recover a stopped peer"
    );
    assert!(
        source_driver
            .public_current_native_peers(&missing, None)
            .await
            .is_err(),
        "current-only input cannot infer a missing former Primary"
    );
    assert_eq!(
        before_selection,
        source_driver.durable.revalidate().await.unwrap()
    );
    let selected_gate = target_driver.pause_native_cut("repair-before-clone");
    let cloned_gate = target_driver.pause_native_cut("repair-cloned");
    let fault_gate = (fault == RepairFault::MissingCandidateWal)
        .then(|| target_driver.pause_native_cut("repair-rewinding"));
    let move_gate = match fault {
        RepairFault::OriginalMoveGap => {
            Some(target_driver.pause_native_cut("repair-original-move-effect"))
        }
        RepairFault::InstallMoveGap => {
            Some(target_driver.pause_native_cut("repair-install-move-effect"))
        }
        _ => None,
    };
    let mut effect = Box::pin(new.run_result(
        selected_scope.clone(),
        ReplicaGoalKind::Reconfigure {
            role: ReplicaRole::Primary,
            catch_up: Some(PublicCatchUpMode::WriteQuorum),
        },
    ));
    tokio::select! {
        _ = selected_gate.entered.notified() => {}
        status = &mut effect => panic!(
            "canonical promotion finished before selecting its repair helper: {status:?}"
        ),
    }
    let witness = async {
        let (offer, _) = offer_for(&target_driver).await;
        assert!(
            source_driver
                .public_request(Request::RepairSource(offer))
                .await
                .is_err(),
            "a selected nonce without an actually owned faithful clone granted normal file read"
        );
        selected_gate.release.notify_one();
        cloned_gate.entered.notified().await;
        let native = ProcessProbe::postgres(source_driver.instance.data_dir());
        let (offer, record) = offer_for(&target_driver).await;
        target_driver
            .pg_confirm_repair_candidate(&offer)
            .await
            .unwrap();
        for field in [
            "nonce",
            "mode",
            "generation",
            "history",
            "creation",
            "storage",
            "certified",
            "lineage",
            "source_timeline",
        ] {
            let mut crossed = serde_json::to_value(&offer).unwrap();
            match field {
                "nonce" => {
                    crossed["candidate_nonce"][0] =
                        serde_json::json!(255 - crossed["candidate_nonce"][0].as_u64().unwrap())
                }
                "generation" => {
                    crossed["binding"]["session"]["generation"] = serde_json::json!(999999)
                }
                "mode" => crossed["binding"]["session"]["mode"] = serde_json::json!("copy"),
                "history" => {
                    crossed["binding"]["session"]["target"]["scope"]["history"]["resourceUid"] =
                        serde_json::json!("crossed-resource")
                }
                "creation" => {
                    crossed["binding"]["session"]["target"]["scope"]["creationId"] =
                        serde_json::json!("crossed-creation")
                }
                "storage" => {
                    crossed["binding"]["session"]["target"]["scope"]["storage"]["storageGeneration"] =
                        serde_json::json!("foreign-storage")
                }
                "certified" => {
                    crossed["certified_lsn"] =
                        serde_json::json!(record["sourceBoundary"].as_i64().unwrap() + 1)
                }
                "source_timeline" => crossed["source_lineage"]["timeline"] = serde_json::json!(999),
                _ => crossed["retained_lineage"]["system_identifier"] = serde_json::json!("1234"),
            }
            let crossed = serde_json::from_value(crossed).unwrap();
            assert!(
                source_driver
                    .public_request(Request::RepairSource(crossed))
                    .await
                    .is_err(),
                "crossed {field} obtained normal source authority"
            );
        }
        if fault == RepairFault::FalseCandidate {
            let candidate =
                std::path::PathBuf::from(record["repair"]["candidatePath"].as_str().unwrap());
            std::fs::rename(&candidate, candidate.with_extension("fault-retained-clone")).unwrap();
            std::fs::create_dir(&candidate).unwrap();
            assert!(
                source_driver
                    .public_request(Request::RepairSource(offer.clone()))
                    .await
                    .is_err()
            );
        }
        cloned_gate.release.notify_one();
        if let Some(gate) = fault_gate {
            gate.entered.notified().await;
            let (_, record) = offer_for(&target_driver).await;
            let candidate =
                std::path::PathBuf::from(record["repair"]["candidatePath"].as_str().unwrap());
            std::fs::rename(
                candidate.join("pg_wal"),
                candidate.join("fault-retained-wal"),
            )
            .unwrap();
            std::fs::create_dir(candidate.join("pg_wal")).unwrap();
            gate.release.notify_one();
        }
        if fault == RepairFault::CancelHelper {
            let helper = actual_rewind_child().await;
            let probe = ProcessProbe::process(helper);
            target_driver.pg_quarantine_session(offer.binding.session.as_deref().unwrap());
            tokio::time::timeout(Duration::from_secs(10), probe.assert_gone())
                .await
                .unwrap();
            eprintln!("actual selected repair helper reaped pid={helper}");
        }
        if let Some(gate) = move_gate {
            gate.entered.notified().await;
            let (_, record) = offer_for(&target_driver).await;
            let archive =
                std::path::PathBuf::from(record["repair"]["archivePath"].as_str().unwrap());
            assert_eq!(
                session::repair::retained_tree_digest(&archive).unwrap(),
                original_tree
            );
            assert!(target_driver.public_start().await.is_err());
            if fault == RepairFault::OriginalMoveGap {
                assert!(std::fs::symlink_metadata(target_driver.instance.data_dir()).is_err());
                let config = PgServiceConfig {
                    resource_uid: target_driver.durable.snapshot().await.identity.resource_uid,
                    application_root: target_driver
                        .instance
                        .data_dir()
                        .parent()
                        .unwrap()
                        .join("a"),
                    pg_data: target_driver.instance.data_dir().to_owned(),
                    pg_bin: find_pg_bin(),
                    pg_port: target_driver.instance.port(),
                    replication_address: target_driver.coordination.local_endpoint.clone(),
                };
                assert_eq!(
                    PgService::storage_state(&config).unwrap(),
                    kuberic_runtime::host::ApplicationStorageState::Established
                );
            }
            target_driver.pg_quarantine_session(offer.binding.session.as_deref().unwrap());
            gate.release.notify_one();
        }
        native
    };
    let (status, native) = tokio::join!(effect, witness);
    if fault != RepairFault::None {
        assert!(matches!(
            status,
            ReplicaGoalStatus::Failed(_) | ReplicaGoalStatus::Uncertain(_)
        ));
        let (_, record) = offer_for(&target_driver).await;
        assert!(record["repair"]["failure"].is_string());
        let moving = matches!(
            fault,
            RepairFault::OriginalMoveGap | RepairFault::InstallMoveGap
        );
        assert_eq!(
            record["repair"]["stage"],
            if fault == RepairFault::OriginalMoveGap {
                "installSelected"
            } else if fault == RepairFault::InstallMoveGap {
                "originalRetained"
            } else if fault == RepairFault::FalseCandidate {
                "cloned"
            } else {
                "rewinding"
            }
        );
        let candidate =
            std::path::PathBuf::from(record["repair"]["candidatePath"].as_str().unwrap());
        if !moving {
            assert!(candidate.is_dir());
            assert_eq!(
                session::repair::retained_tree_digest(old.service.instance().data_dir()).unwrap(),
                original_tree
            );
        }
        assert!(
            old.driver()
                .durable
                .revalidate()
                .await
                .unwrap()
                .quiescence
                .is_some()
        );
        assert!(!old.service.instance().is_running().await);
        assert!(new.service.instance().connect_application().await.is_err());
        let (offer, _) = offer_for(&target_driver).await;
        assert!(
            source_driver
                .public_request(Request::RepairSource(offer.clone()))
                .await
                .is_err()
        );
        for member in [&old, &new] {
            member.running.shutdown();
        }
        // Historical failed joins remain errors; actual process probes, not a
        // successful Close label, establish subsequent physical exclusion.
        let target_close = old.running.wait().await;
        let source_close = new.running.wait().await;
        eprintln!("retained negative target close={target_close:?}; source close={source_close:?}");
        native.assert_gone().await;
        if moving {
            let identity = target_driver.durable.revalidate().await.unwrap().identity;
            let root = old
                .service
                .instance()
                .data_dir()
                .parent()
                .unwrap()
                .to_owned();
            let inode = record["repair"]["candidate"]["inode"].as_u64().unwrap();
            drop(target_driver);
            drop(source_driver);
            drop(old);
            drop(new);
            reopen_closed_repair(root, identity, offer, original_tree, inode).await;
        }
        return;
    }
    assert_eq!(status, ReplicaGoalStatus::Achieved);
    let fresh = new
        .driver()
        .pg_session_ids_for_target(ReplicaId::new(1))
        .into_iter()
        .find(|id| id.mode == session::PgSessionMode::Reformation)
        .unwrap();
    assert_eq!(
        new.driver().pg_native_establishment(&fresh),
        (session::PgSessionMode::Reformation, false, true)
    );
    assert_eq!(
        old.driver().pg_native_establishment(&fresh),
        (session::PgSessionMode::Reformation, false, true)
    );
    let repaired = old.driver().durable.revalidate().await.unwrap();
    let record = repaired
        .native_reformations
        .iter()
        .find(|record| record.nonce() == fresh.nonce)
        .unwrap();
    let record = serde_json::to_value(record).unwrap();
    let archive = std::path::PathBuf::from(record["repair"]["archivePath"].as_str().unwrap());
    assert_eq!(
        session::repair::retained_tree_digest(&archive).unwrap(),
        original_tree
    );
    {
        use std::os::unix::fs::MetadataExt;
        assert_eq!(std::fs::metadata(&archive).unwrap().ino(), original_inode);
        assert_ne!(
            std::fs::metadata(old.service.instance().data_dir())
                .unwrap()
                .ino(),
            original_inode
        );
    }
    assert!(
        fresh
            .source
            .scope
            .configuration
            .previous
            .as_ref()
            .unwrap()
            .members
            .iter()
            .any(|member| member.identity == ids[0] && member.role == ReplicaRole::Primary)
    );
    let current = scope(
        selected.clone(),
        None,
        &[
            (&old, ReplicaRole::ActiveSecondary),
            (&new, ReplicaRole::Primary),
        ],
    )
    .await;
    old.run(
        current.clone(),
        ReplicaGoalKind::Reconfigure {
            role: ReplicaRole::ActiveSecondary,
            catch_up: None,
        },
    )
    .await;
    new.run(
        current,
        ReplicaGoalKind::Reconfigure {
            role: ReplicaRole::Primary,
            catch_up: Some(PublicCatchUpMode::WriteQuorum),
        },
    )
    .await;
    assert_eq!(
        new.snapshot()
            .await
            .publication
            .unwrap()
            .scope
            .configuration
            .current
            .write_quorum,
        pair.write_quorum
    );
    assert!(
        new.snapshot()
            .await
            .publication
            .unwrap()
            .scope
            .configuration
            .previous
            .is_none()
    );
    let (sql, connection) = new.application().await;
    assert_eq!(
        sql.query_one("SELECT count(*) FROM repair_prefix", &[])
            .await
            .unwrap()
            .get::<_, i64>(0),
        2
    );
    sql.batch_execute("INSERT INTO repair_prefix VALUES(3, 'acknowledged-after-native-repair')")
        .await
        .unwrap();
    drop(sql);
    connection.await.unwrap();
    PrimaryReplicator::wait_for_catch_up_quorum(new.driver().as_ref(), ReplicaSetQuorumMode::All)
        .await
        .unwrap();
    let (sql, connection) = old.service.instance().connect().await.unwrap();
    assert!(
        sql.query_one("SELECT pg_is_in_recovery()", &[])
            .await
            .unwrap()
            .get::<_, bool>(0)
    );
    drop(sql);
    connection.await.unwrap();
    let (sql, connection) = tokio_postgres::connect(
        &old.service
            .instance()
            .connection_string()
            .replace("dbname=postgres", "dbname=kuberic"),
        tokio_postgres::NoTls,
    )
    .await
    .unwrap();
    let connection = tokio::spawn(async move {
        connection.await.unwrap();
    });
    assert_eq!(
        sql.query_one("SELECT count(*) FROM repair_prefix", &[])
            .await
            .unwrap()
            .get::<_, i64>(0),
        3
    );
    drop(sql);
    connection.await.unwrap();
    for previous in previous_sessions {
        assert!(
            old.driver()
                .pg_session_request(session::SessionRequest {
                    session: previous,
                    action: session::SessionAction::Complete
                })
                .await
                .is_err()
        );
    }
    let before = old.driver().durable.revalidate().await.unwrap();
    assert!(
        old.driver()
            .durable
            .update(|state| {
                state.native_reformations.clear();
                Ok(())
            })
            .await
            .is_err(),
        "installed repair history was erasable after acknowledged use"
    );
    let mut rolled_back = serde_json::to_value(before.native_reformations.last().unwrap()).unwrap();
    rolled_back["stage"] = serde_json::json!("selected");
    rolled_back["evidence"] = serde_json::Value::Null;
    rolled_back["repair"]["stage"] = serde_json::json!("installed");
    assert!(
        old.driver()
            .durable
            .update(|state| {
                *state.native_reformations.last_mut().unwrap() =
                    serde_json::from_value(rolled_back).unwrap();
                Ok(())
            })
            .await
            .is_err(),
        "installed lineage rolled back after native acknowledgement"
    );
    assert_eq!(old.driver().durable.revalidate().await.unwrap(), before);
    let (offer, _) = offer_for(old.driver()).await;
    assert!(
        new.driver()
            .public_request(Request::RepairSource(offer))
            .await
            .is_err(),
        "normal read authority survived the completed candidate stage"
    );
    old.stop().await;
    new.stop().await;
}
