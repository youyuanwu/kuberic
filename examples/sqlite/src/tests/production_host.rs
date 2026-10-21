use super::*;
use kuberic_runtime::application::OpenMode;
use kuberic_runtime::control::{convert, proto};
use kuberic_runtime::protocol::command::PublicV1OperationCommand;
use kuberic_runtime::protocol::public_operations::{
    PublicOperationClass, PublicOperationIntent, PublicOperationProgram,
};
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

fn tree(root: &std::path::Path) -> std::collections::BTreeMap<PathBuf, Vec<u8>> {
    fn walk(
        base: &std::path::Path,
        path: &std::path::Path,
        out: &mut std::collections::BTreeMap<PathBuf, Vec<u8>>,
    ) {
        for entry in std::fs::read_dir(path).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                out.insert(path.strip_prefix(base).unwrap().to_path_buf(), Vec::new());
                walk(base, &path, out);
            } else {
                out.insert(
                    path.strip_prefix(base).unwrap().to_path_buf(),
                    std::fs::read(&path).unwrap(),
                );
            }
        }
    }
    let mut result = std::collections::BTreeMap::new();
    walk(root, root, &mut result);
    result
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
    let mut valid_manifest = Vec::new();

    // Every round uses main's constructor, deferred application, authenticated
    // control service and actual application/native Open. No metadata fixture.
    for round in 0..5 {
        if round == 2 {
            valid_manifest = std::fs::read(&manifest)?;
            std::fs::write(&manifest, b"not valid metadata")?;
        } else if round == 3 {
            std::fs::remove_file(&manifest)?;
        } else if round == 4 {
            let mut value: serde_json::Value = serde_json::from_slice(&valid_manifest)?;
            value["version"] = serde_json::json!(u32::MAX);
            std::fs::write(&manifest, serde_json::to_vec(&value)?)?;
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
            let program = PublicOperationProgram::Open {
                replica: identity.clone(),
                existing: round != 0,
            };
            let command = PublicV1OperationCommand {
                target: identity.clone(),
                intent: PublicOperationIntent {
                    identity: report.identity,
                    operation_id: OperationId::new(format!("open-{round}")),
                    revision: 10 + round * 10,
                    process_session_id: report.process_session_id.clone(),
                    class: PublicOperationClass::Authority,
                    input_digest: program.digest(),
                    replay_disposition: program.replay_disposition(),
                    lifecycle: None,
                    program: Some(program),
                },
            };
            let request = |command: &PublicV1OperationCommand| {
                authorized(proto::ExecuteCommandRequest {
                    protocol_version: kuberic_runtime::protocol::PROTOCOL_VERSION,
                    resource_uid: resource.to_string(),
                    target: Some(identity.clone().into()),
                    expected_process_session_id: report.process_session_id.to_string(),
                    command: Some(proto::execute_command_request::Command::PublicV1Operation(
                        convert::public_v1_operation_command_to_proto(command).unwrap(),
                    )),
                })
            };
            let opened = client.execute(request(&command)).await;
            if round < 2 {
                opened?;
                let report = convert::normalize_public_v1_agent_status_report(
                    client
                        .get_status(authorized(status_request))
                        .await?
                        .into_inner(),
                )?;
                assert_eq!(report.open_mode, OpenMode::Existing);
                assert!(manifest.is_file());
            } else {
                assert!(opened.is_err(), "invalid established storage must reject");
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
    }
    Ok(())
}
