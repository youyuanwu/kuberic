use super::*;
use kuberic_runtime::application::OpenMode;
use kuberic_runtime::control::{convert, proto};
use kuberic_runtime::protocol::public_operations::*;
use kuberic_runtime::protocol::types::{
    ConfigurationDescriptor, ConfigurationMember, Epoch, OperationId, ReplicaIdentity,
    ReplicaInstanceId, ReplicaRole, derive_agent_generation, derive_initialization_id,
};

fn authorized<T>(message: T) -> tonic::Request<T> {
    let mut request = tonic::Request::new(message);
    request
        .metadata_mut()
        .insert("authorization", "Bearer startup-token".parse().unwrap());
    request
}

fn tree(root: &std::path::Path) -> std::collections::BTreeMap<PathBuf, Option<Vec<u8>>> {
    fn walk(
        base: &std::path::Path,
        path: &std::path::Path,
        out: &mut std::collections::BTreeMap<PathBuf, Option<Vec<u8>>>,
    ) {
        for entry in std::fs::read_dir(path).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                out.insert(path.strip_prefix(base).unwrap().to_path_buf(), None);
                walk(base, &path, out);
            } else {
                out.insert(
                    path.strip_prefix(base).unwrap().to_path_buf(),
                    Some(std::fs::read(&path).unwrap()),
                );
            }
        }
    }
    let mut result = std::collections::BTreeMap::new();
    walk(root, root, &mut result);
    result
}

fn restore_fixture(
    root: &std::path::Path,
    snapshot: &std::collections::BTreeMap<PathBuf, Option<Vec<u8>>>,
) {
    std::fs::remove_dir_all(root).unwrap();
    std::fs::create_dir_all(root).unwrap();
    for (relative, bytes) in snapshot {
        let path = root.join(relative);
        if let Some(bytes) = bytes {
            std::fs::write(path, bytes).unwrap();
        } else {
            std::fs::create_dir_all(path).unwrap();
        }
    }
}

#[tokio::test]
async fn production_host_new_existing_and_invalid_storage() -> Result<(), Box<dyn std::error::Error>>
{
    let root = tempfile::tempdir()?;
    let config = Config {
        resource_uid: "sqlite-production-startup".into(),
        replica_id: 1,
        pod_uid: "startup-pod".into(),
        pvc_uid: "startup-pvc".into(),
        pod_ip: "127.0.0.1".parse()?,
        namespace: "default".into(),
        bearer_token: "startup-token".into(),
        data_root: root.path().to_path_buf(),
        control_address: "127.0.0.1:0".parse()?,
        replication_address: "127.0.0.1:0".parse()?,
        application_address: "127.0.0.1:0".parse()?,
    };
    let resource = ResourceUid::new(&config.resource_uid);
    let id = ReplicaId::new(config.replica_id);
    let initialization = derive_initialization_id(
        &resource,
        id,
        &PodUid::new(&config.pod_uid),
        &PvcUid::new(&config.pvc_uid),
    );
    let identity = ReplicaIdentity {
        replica_id: id,
        instance_id: ReplicaInstanceId::new(&config.pod_uid),
        agent_generation: derive_agent_generation(&initialization),
    };
    let path = config.data_root.join("application");
    let manifest = path.join("state-v2.json");
    let mut established = std::collections::BTreeMap::new();

    // Every round uses main's constructor, deferred application, authenticated
    // control service and actual application/native Open. No metadata fixture.
    for round in 0..5 {
        if round >= 2 {
            // Each invalid case starts from the same successfully opened and
            // stopped journal, not a preceding Open failure/fault fence.
            restore_fixture(root.path(), &established);
        }
        if round == 2 {
            std::fs::write(&manifest, b"not valid metadata")?;
        } else if round == 3 {
            std::fs::remove_file(&manifest)?;
        } else if round == 4 {
            let checked = std::fs::read_to_string(&manifest)?;
            let state = checked
                .strip_prefix("{\"state\":")
                .unwrap()
                .rsplit_once(",\"checksum\":")
                .unwrap()
                .0;
            let value: serde_json::Value = serde_json::from_str(state)?;
            let state = state.replacen(
                &format!("\"version\":{}", value["version"]),
                &format!("\"version\":{}", u32::MAX),
                1,
            );
            // Preserve field order and a valid checksum so this specifically
            // exercises the incompatible version, not incidental corruption.
            let checksum = crc32fast::hash(state.as_bytes());
            std::fs::write(
                &manifest,
                format!("{{\"state\":{state},\"checksum\":{checksum}}}"),
            )?;
        }
        let before = (round >= 2).then(|| tree(&path));
        let fresh = SqlitePersistence::is_fresh_empty(&path)?;
        assert_eq!(fresh, round == 0);
        let application = Arc::new(SqliteService::deferred(
            path.clone(),
            "http://127.0.0.1:0".into(),
        )?);
        let host = replica_host(
            &config,
            application.clone(),
            if fresh {
                ApplicationStorageState::FreshEmpty
            } else {
                ApplicationStorageState::Established
            },
        );
        let listener = tokio::net::TcpListener::bind(config.control_address).await?;
        let address = format!("http://{}", listener.local_addr()?);
        let (shutdown, receiver) = tokio::sync::watch::channel(false);
        let (ready, mut ready_receiver) = tokio::sync::watch::channel(false);
        let startup = tokio::spawn(async move {
            if let Some(mut running) = host.start_with_shutdown(listener, receiver.clone()).await? {
                ready.send_replace(true);
                let mut receiver = receiver;
                let _ = receiver.wait_for(|stopped| *stopped).await;
                running.shutdown();
                running.wait().await?;
            }
            Ok::<_, kuberic_runtime::host::HostError>(())
        });
        let result = tokio::time::timeout(Duration::from_secs(20), async {
            let mut client =
                proto::agent_control_client::AgentControlClient::connect(address.clone()).await?;
            let status_request = proto::GetAgentStatusRequest {
                protocol_version: kuberic_runtime::protocol::PROTOCOL_VERSION,
                resource_uid: resource.to_string(),
                replica_id: id.value(),
                expected_instance_id: config.pod_uid.clone(),
            };
            if round == 0 {
                let status = client
                    .get_status(authorized(status_request.clone()))
                    .await?
                    .into_inner();
                assert_eq!(
                    status.storage_state,
                    proto::AgentStorageState::Uninitialized as i32
                );
                let configuration = ConfigurationDescriptor::new(
                    Epoch::new(0, 1),
                    id,
                    vec![ConfigurationMember {
                        identity: identity.clone(),
                        role: ReplicaRole::Primary,
                    }],
                    1,
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
                                    local_replica_id: id.value(),
                                    expected_instance_id: config.pod_uid.clone(),
                                    expected_pod_uid: config.pod_uid.clone(),
                                    expected_pvc_uid: config.pvc_uid.clone(),
                                    assigned_agent_generation: identity
                                        .agent_generation
                                        .to_string(),
                                    effective_policy: Some(proto::EffectivePolicy {
                                        replica_set_size: 1,
                                        write_quorum: 1,
                                        read_quorum: 1,
                                        failover_delay_seconds: 1,
                                    }),
                                    bootstrap_configuration: Some(configuration.into()),
                                    provisioning: None,
                                },
                            ),
                        ),
                    }))
                    .await?;
            }
            drop(client);
            ready_receiver.wait_for(|ready| *ready).await?;
            let mut client =
                proto::agent_control_client::AgentControlClient::connect(address).await?;
            let report = convert::normalize_public_v1_agent_status_report(
                client
                    .get_status(authorized(status_request.clone()))
                    .await?
                    .into_inner(),
            )?;
            assert_eq!(
                report.open_mode,
                if round == 0 {
                    OpenMode::New
                } else {
                    OpenMode::Existing
                },
                "production storage binding selects the authorized mode"
            );
            let query = || {
                authorized(proto::ReplicaSnapshotRequest {
                    protocol_version: ORDINARY_ACTION_FORMAT,
                    resource_uid: resource.to_string(),
                    target: Some(identity.clone().into()),
                    expected_process_session_id: report.process_session_id.to_string(),
                    sample_native_progress: false,
                })
            };
            let response = client.get_snapshot(query()).await?.into_inner();
            let ReplicaAgentQueryResult::Initialized(snapshot) =
                serde_json::from_slice(&response.canonical_json)?
            else {
                panic!("initialized ordinary snapshot required")
            };
            let configuration = ConfigurationDescriptor::new(
                Epoch::new(0, 1),
                id,
                vec![ConfigurationMember {
                    identity: identity.clone(),
                    role: ReplicaRole::Primary,
                }],
                1,
            );
            let goal = ReplicaGoal {
                format: ORDINARY_ACTION_FORMAT,
                identity: report.identity,
                operation_id: OperationId::new(format!("open-{round}")),
                revision: 10 + round * 10,
                scope: ReplicaScope {
                    history: report.history,
                    storage: report.storage_incarnation,
                    process_session_id: report.process_session_id.clone(),
                    creation_id: snapshot.creation_id,
                    epoch: configuration.epoch,
                    configuration: ReplicaConfigurationScope {
                        current: configuration,
                        previous: None,
                        replicas: Vec::new(),
                    },
                },
                desired: ReplicaGoalKind::Prepare {
                    role: ReplicaRole::Primary,
                    rebuild: None,
                },
            };
            client
                .submit_goal(authorized(proto::ReplicaGoalRequest {
                    protocol_version: ORDINARY_ACTION_FORMAT,
                    canonical_json: serde_json::to_vec(&goal)?,
                }))
                .await?;
            let opened = loop {
                let response = client.get_snapshot(query()).await?.into_inner();
                let ReplicaAgentQueryResult::Initialized(snapshot) =
                    serde_json::from_slice(&response.canonical_json)?
                else {
                    panic!("snapshot required")
                };
                if let Some(observation) = snapshot.goals.iter().find(|entry| entry.goal == goal)
                    && observation.status != ReplicaGoalStatus::Pending
                {
                    break observation.status.clone();
                }
                tokio::task::yield_now().await;
            };
            if round < 2 {
                assert_eq!(opened, ReplicaGoalStatus::Achieved);
                let report = convert::normalize_public_v1_agent_status_report(
                    client
                        .get_status(authorized(status_request))
                        .await?
                        .into_inner(),
                )?;
                assert_eq!(report.open_mode, OpenMode::Existing);
                assert!(manifest.is_file());
            } else {
                let ReplicaGoalStatus::Failed(detail) = opened else {
                    panic!("invalid storage must fail: {opened:?}")
                };
                assert!(
                    detail.contains("application error:"),
                    "actual application Open must reject: {detail}"
                );
                if round == 3 {
                    assert!(detail.contains("lacks classic V1 metadata"), "{detail}");
                } else if round == 4 {
                    assert!(
                        detail.contains("invalid v2 storage version or progress"),
                        "{detail}"
                    );
                }
                assert_eq!(tree(&path), *before.as_ref().unwrap());
            }
            Ok::<_, Box<dyn std::error::Error>>(())
        })
        .await;
        shutdown.send_replace(true);
        let host_result = startup.await?;
        let application_result = application.shutdown_and_drain().await;
        result??;
        host_result?;
        match application_result {
            Ok(()) | Err(kuberic_runtime::RuntimeError::OperationCancelled) => {}
            Err(error) => return Err(error.into()),
        }
        if round == 1 {
            established = tree(root.path());
        }
    }
    Ok(())
}
