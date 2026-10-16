use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use kuberic_runtime::control::proto;
use kuberic_runtime::host::{
    ApplicationStorageState, ReplicaEndpointResolver, ReplicaHost, ReplicaProcessConfig,
};
use kuberic_runtime::protocol::types::{
    AgentGeneration, ConfigurationDescriptor, ConfigurationMember, Epoch, PodUid, PvcUid,
    ReplicaId, ReplicaIdentity, ReplicaInstanceId, ReplicaRole, ResourceUid,
    derive_agent_generation, derive_initialization_id,
};
use sqlite_replicated::SqlitePersistence;
use sqlite_replicated::service::SqliteService;
use tokio::sync::watch;

const TOKEN: &str = "provider-rejection-token";

struct Resolver;

impl ReplicaEndpointResolver for Resolver {
    fn control_endpoint(&self, _identity: &ReplicaIdentity) -> String {
        "http://127.0.0.1:1".into()
    }

    fn replication_endpoint(&self, _identity: &ReplicaIdentity) -> String {
        "http://127.0.0.1:1".into()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum TreeEntry {
    Directory,
    File(Vec<u8>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TreeSnapshot {
    exists: bool,
    entries: BTreeMap<PathBuf, TreeEntry>,
}

fn snapshot(root: &Path) -> TreeSnapshot {
    fn visit(root: &Path, path: &Path, entries: &mut BTreeMap<PathBuf, TreeEntry>) {
        let mut children = std::fs::read_dir(path)
            .unwrap()
            .collect::<std::io::Result<Vec<_>>>()
            .unwrap();
        children.sort_by_key(std::fs::DirEntry::file_name);
        for child in children {
            let path = child.path();
            let relative = path.strip_prefix(root).unwrap().to_path_buf();
            let metadata = std::fs::symlink_metadata(&path).unwrap();
            if metadata.is_dir() {
                entries.insert(relative, TreeEntry::Directory);
                visit(root, &path, entries);
            } else {
                assert!(
                    metadata.is_file(),
                    "unexpected provider tree entry: {path:?}"
                );
                entries.insert(relative, TreeEntry::File(std::fs::read(path).unwrap()));
            }
        }
    }

    if !root.exists() {
        return TreeSnapshot {
            exists: false,
            entries: BTreeMap::new(),
        };
    }
    assert!(root.is_dir());
    let mut entries = BTreeMap::new();
    visit(root, root, &mut entries);
    TreeSnapshot {
        exists: true,
        entries,
    }
}

fn assert_no_transient_files(tree: &TreeSnapshot) {
    for path in tree.entries.keys() {
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default();
        assert!(
            !name.ends_with("-wal")
                && !name.ends_with("-shm")
                && !name.ends_with(".tmp")
                && !name.ends_with(".staging"),
            "rejected admission left transient storage: {path:?}"
        );
    }
}

fn root(name: &str) -> PathBuf {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/provider-state-rejection/sqlite")
        .join(format!("{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    root
}

fn address() -> std::net::SocketAddr {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    address
}

macro_rules! connect {
    ($address:expr) => {{
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match tonic::transport::Endpoint::from_shared(format!("http://{}", $address))
                    .unwrap()
                    .connect()
                    .await
                {
                    Ok(channel) => {
                        break proto::agent_control_client::AgentControlClient::with_interceptor(
                            channel,
                            |mut request: tonic::Request<()>| {
                                let value = format!("Bearer {TOKEN}").parse().unwrap();
                                request.metadata_mut().insert("authorization", value);
                                Ok(request)
                            },
                        );
                    }
                    Err(_) => tokio::time::sleep(Duration::from_millis(10)).await,
                }
            }
        })
        .await
        .expect("initialization service did not become reachable")
    }};
}

fn replacement_request(
    resource_uid: &ResourceUid,
    process_session_id: String,
) -> proto::ExecuteCommandRequest {
    let replica_id = ReplicaId::new(1);
    let pod_uid = PodUid::new("replacement-pod");
    let pvc_uid = PvcUid::new("replacement-pvc");
    let initialization_id = derive_initialization_id(resource_uid, replica_id, &pod_uid, &pvc_uid);
    let identity = ReplicaIdentity {
        replica_id,
        instance_id: ReplicaInstanceId::new(pod_uid.as_str()),
        agent_generation: derive_agent_generation(&initialization_id),
    };
    let configuration = ConfigurationDescriptor::new(
        Epoch::new(0, 1),
        replica_id,
        vec![ConfigurationMember {
            identity: identity.clone(),
            role: ReplicaRole::Primary,
        }],
        1,
    );
    proto::ExecuteCommandRequest {
        protocol_version: kuberic_runtime::protocol::PROTOCOL_VERSION,
        resource_uid: resource_uid.to_string(),
        target: Some(identity.clone().into()),
        expected_process_session_id: process_session_id,
        command: Some(
            proto::execute_command_request::Command::InitializeAgentStore(
                proto::InitializeAgentStoreCommand {
                    initialization_id: initialization_id.to_string(),
                    resource_uid: resource_uid.to_string(),
                    local_replica_id: replica_id.value(),
                    expected_instance_id: pod_uid.to_string(),
                    expected_pod_uid: pod_uid.to_string(),
                    expected_pvc_uid: pvc_uid.to_string(),
                    assigned_agent_generation: identity.agent_generation.to_string(),
                    effective_policy: Some(proto::EffectivePolicy {
                        replica_set_size: 1,
                        write_quorum: 1,
                        read_quorum: 1,
                        failover_delay_seconds: 30,
                    }),
                    bootstrap_configuration: Some(configuration.into()),
                    provisioning: Some(proto::ProvisioningIntent {
                        purpose: Some(proto::provisioning_intent::Purpose::Replaces(
                            ReplicaIdentity {
                                replica_id,
                                instance_id: ReplicaInstanceId::new("phase6-pod"),
                                agent_generation: AgentGeneration::new("phase6-agent"),
                            }
                            .into(),
                        )),
                        pod_uid: pod_uid.to_string(),
                        pvc_uid: pvc_uid.to_string(),
                        operation_id: "replacement-admission".into(),
                    }),
                },
            ),
        ),
    }
}

fn config(
    root: &Path,
    resource_uid: &ResourceUid,
    replication_address: std::net::SocketAddr,
) -> ReplicaProcessConfig {
    ReplicaProcessConfig {
        resource_uid: resource_uid.clone(),
        replica_id: ReplicaId::new(1),
        pod_uid: PodUid::new("replacement-pod"),
        pvc_uid: PvcUid::new("replacement-pvc"),
        data_root: root.to_path_buf(),
        replication_address,
        bearer_token: TOKEN.into(),
        rpc_deadline: Duration::from_secs(2),
        transport_window_capacity: 8,
    }
}

#[tokio::test]
async fn phase6_sqlite_provider_state_rejects_replacement_admission_without_mutation() {
    tokio::time::timeout(Duration::from_secs(120), async {
        let root = root("reject");
        let application_root = root.join("application");
        let provider = SqlitePersistence::open(application_root.clone()).unwrap();
        assert_eq!(provider.progress().unwrap().applied_lsn, 0);
        drop(provider);
        assert_eq!(
            SqlitePersistence::open(application_root.clone())
                .unwrap()
                .progress()
                .unwrap()
                .committed_lsn,
            0
        );

        let agent_root = root.join(".kuberic");
        let provider_before = snapshot(&application_root);
        let agent_before = snapshot(&agent_root);
        assert!(provider_before.exists);
        assert!(!agent_before.exists);
        assert_no_transient_files(&provider_before);

        let resource_uid = ResourceUid::new("sqlite-phase6-provider-rejection");
        let control_address = address();
        let replication_address = address();
        let application = Arc::new(
            SqliteService::deferred(
                application_root.clone(),
                format!("http://{replication_address}"),
            )
            .unwrap(),
        );
        let (shutdown, receiver) = watch::channel(false);
        let startup = tokio::spawn(
            ReplicaHost::new(
                config(&root, &resource_uid, replication_address),
                application,
                ApplicationStorageState::Established,
                Arc::new(Resolver),
            )
            .start_with_shutdown(
                tokio::net::TcpListener::bind(control_address)
                    .await
                    .unwrap(),
                receiver,
            ),
        );
        let mut client = connect!(control_address);
        let report = client
            .get_status(proto::GetAgentStatusRequest {
                protocol_version: kuberic_runtime::protocol::PROTOCOL_VERSION,
                resource_uid: resource_uid.to_string(),
                replica_id: 1,
                expected_instance_id: "replacement-pod".into(),
            })
            .await
            .unwrap()
            .into_inner();
        assert_eq!(
            report.storage_state,
            proto::AgentStorageState::Unsafe as i32
        );
        assert!(!report.healthy);
        assert_eq!(
            report.storage_error,
            "application state exists without matching Kuberic agent metadata"
        );
        let error = client
            .execute(replacement_request(
                &resource_uid,
                report.process_session_id,
            ))
            .await
            .unwrap_err();
        assert_eq!(error.code() as i32, 9);
        assert_eq!(error.message(), "application state is not fresh and empty");
        drop(client);
        shutdown.send_replace(true);
        assert!(startup.await.unwrap().unwrap().is_none());

        let provider_after = snapshot(&application_root);
        let agent_after = snapshot(&agent_root);
        assert_eq!(provider_after, provider_before);
        assert_eq!(agent_after, agent_before);
        assert_no_transient_files(&provider_after);
        assert_no_transient_files(&agent_after);
        std::fs::remove_dir_all(root).unwrap();
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn fresh_and_replacement_created_sqlite_state_open_and_reopen() {
    tokio::time::timeout(Duration::from_secs(120), async {
        let root = root("reopen");
        let application_root = root.join("application");
        assert!(SqlitePersistence::is_fresh_empty(&application_root).unwrap());

        let resource_uid = ResourceUid::new("sqlite-replacement-created-reopen");
        let control_address = address();
        let replication_address = address();
        let application = Arc::new(
            SqliteService::deferred(
                application_root.clone(),
                format!("http://{replication_address}"),
            )
            .unwrap(),
        );
        let startup = tokio::spawn(
            ReplicaHost::new(
                config(&root, &resource_uid, replication_address),
                application,
                ApplicationStorageState::FreshEmpty,
                Arc::new(Resolver),
            )
            .start(
                tokio::net::TcpListener::bind(control_address)
                    .await
                    .unwrap(),
            ),
        );
        let mut client = connect!(control_address);
        let report = client
            .get_status(proto::GetAgentStatusRequest {
                protocol_version: kuberic_runtime::protocol::PROTOCOL_VERSION,
                resource_uid: resource_uid.to_string(),
                replica_id: 1,
                expected_instance_id: "replacement-pod".into(),
            })
            .await
            .unwrap()
            .into_inner();
        assert_eq!(
            report.storage_state,
            proto::AgentStorageState::Uninitialized as i32
        );
        client
            .execute(replacement_request(
                &resource_uid,
                report.process_session_id,
            ))
            .await
            .unwrap();
        drop(client);
        let mut running = startup.await.unwrap().unwrap();
        running.shutdown();
        running.wait().await.unwrap();
        drop(running);

        let persistence = SqlitePersistence::open(application_root.clone()).unwrap();
        assert_eq!(persistence.progress().unwrap().applied_lsn, 0);
        drop(persistence);
        assert!(!SqlitePersistence::is_fresh_empty(&application_root).unwrap());
        assert_eq!(
            SqlitePersistence::open(application_root.clone())
                .unwrap()
                .progress()
                .unwrap()
                .committed_lsn,
            0
        );
        let provider_before_restart = snapshot(&application_root);
        let agent_before_restart = snapshot(&root.join(".kuberic"));

        let control_address = address();
        let replication_address = address();
        let application = Arc::new(
            SqliteService::deferred(
                application_root.clone(),
                format!("http://{replication_address}"),
            )
            .unwrap(),
        );
        let mut restarted = ReplicaHost::new(
            config(&root, &resource_uid, replication_address),
            application,
            ApplicationStorageState::Established,
            Arc::new(Resolver),
        )
        .start(
            tokio::net::TcpListener::bind(control_address)
                .await
                .unwrap(),
        )
        .await
        .unwrap();
        restarted.shutdown();
        restarted.wait().await.unwrap();
        drop(restarted);
        assert_eq!(snapshot(&application_root), provider_before_restart);
        assert_eq!(snapshot(&root.join(".kuberic")), agent_before_restart);
        std::fs::remove_dir_all(root).unwrap();
    })
    .await
    .unwrap();
}
