use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, put};
use clap::Parser;
use kuberic_runtime::host::KubernetesDnsResolver;
use kuberic_runtime::host::{
    ApplicationStorageState, ReplicaDiagnostics, ReplicaHandle, ReplicaHost, ReplicaProcessConfig,
};
use kuberic_runtime::protocol::types::{PodUid, PvcUid, ReplicaId, ResourceUid};
use tokio::sync::watch;

use kvstore2::persistence::KvPersistence;
use kvstore2::service::KvService;
use kvstore2::state::CopyGate;

#[derive(Debug, Parser)]
struct Config {
    #[arg(long, env = "KUBERIC_RESOURCE_UID")]
    resource_uid: String,
    #[arg(long, env = "KUBERIC_REPLICA_ID")]
    replica_id: i64,
    #[arg(long, env = "KUBERIC_POD_UID")]
    pod_uid: String,
    #[arg(long, env = "KUBERIC_PVC_UID")]
    pvc_uid: String,
    #[arg(long, env = "KUBERIC_POD_IP")]
    pod_ip: String,
    #[arg(long, env = "KUBERIC_SET_NAME")]
    set_name: String,
    #[arg(long, env = "KUBERIC_NAMESPACE")]
    namespace: String,
    #[arg(long, env = "KUBERIC_AGENT_BEARER_TOKEN")]
    bearer_token: String,
    #[arg(long, env = "KUBERIC_DATA_ROOT", default_value = "/var/lib/kuberic")]
    data_root: PathBuf,
    #[arg(long, env = "KUBERIC_CONTROL_ADDRESS", default_value = "0.0.0.0:50051")]
    control_address: SocketAddr,
    #[arg(
        long,
        env = "KUBERIC_REPLICATION_ADDRESS",
        default_value = "0.0.0.0:50052"
    )]
    replication_address: SocketAddr,
    #[arg(
        long,
        env = "KUBERIC_APPLICATION_ADDRESS",
        default_value = "0.0.0.0:8080"
    )]
    application_address: SocketAddr,
    #[arg(long, env = "KUBERIC_LIVE_TEST_COPY_GATE_ADDRESS")]
    live_test_copy_gate_address: Option<SocketAddr>,
}

#[derive(Clone)]
struct HttpState {
    application: Arc<KvService>,
    replica: ReplicaHandle,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let config = Config::parse();
    if config.replica_id <= 0 {
        anyhow::bail!("KUBERIC_REPLICA_ID must be positive");
    }
    let application_path = config.data_root.join("application");
    let application_storage = if KvPersistence::is_fresh_empty(&application_path)? {
        ApplicationStorageState::FreshEmpty
    } else {
        ApplicationStorageState::Established
    };
    let persistence = Arc::new(KvPersistence::open(application_path)?);
    let copy_gate = match config.live_test_copy_gate_address {
        Some(_) => CopyGate::enabled(config.data_root.join(".live-test-copy-gate"))?,
        None => CopyGate::disabled(),
    };
    let application = Arc::new(KvService::new(
        persistence,
        format!("http://{}:50052", config.pod_ip),
        copy_gate.clone(),
    ));
    let mut replica = replica_host(&config, application.clone(), application_storage)
        .start(tokio::net::TcpListener::bind(config.control_address).await?)
        .await?;
    let shutdown_rx = replica.shutdown_signal();
    let http_state = HttpState {
        application: application.clone(),
        replica: replica.handle(),
    };
    let router = application_router(http_state);
    let listener = tokio::net::TcpListener::bind(config.application_address).await?;
    let mut http_task = tokio::spawn(
        axum::serve(listener, router)
            .with_graceful_shutdown(wait_shutdown(shutdown_rx.clone()))
            .into_future(),
    );
    let live_test_address = config.live_test_copy_gate_address;
    let live_test_router = copy_gate_router(live_test_address.map(|_| copy_gate));
    let mut live_test_task = tokio::spawn(async move {
        if let Some(address) = live_test_address {
            let listener = tokio::net::TcpListener::bind(address).await?;
            axum::serve(listener, live_test_router)
                .with_graceful_shutdown(wait_shutdown(shutdown_rx))
                .await
        } else {
            futures::future::pending::<std::io::Result<()>>().await
        }
    });

    let mut host_finished = false;
    let result: Result<()> = tokio::select! {
        result = replica.wait() => {
            host_finished = true;
            result.map_err(Into::into)
        },
        result = &mut http_task => result.map_err(anyhow::Error::from)
            .and_then(|result| result.map_err(Into::into)),
        result = &mut live_test_task => result.map_err(anyhow::Error::from)
            .and_then(|result| result.map_err(Into::into)),
        result = tokio::signal::ctrl_c() => result.map_err(Into::into),
    };

    replica.shutdown();
    let host_cleanup = if host_finished {
        Ok(())
    } else {
        replica.wait().await
    };
    let application_cleanup = application.shutdown_and_drain().await;
    let errors = [
        result.err().map(|error| error.to_string()),
        host_cleanup.err().map(|error| error.to_string()),
        application_cleanup.err().map(|error| error.to_string()),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>();
    anyhow::ensure!(errors.is_empty(), "{}", errors.join("; "));
    Ok(())
}

fn replica_host(
    config: &Config,
    application: Arc<KvService>,
    application_storage: ApplicationStorageState,
) -> ReplicaHost<KvService, KubernetesDnsResolver> {
    ReplicaHost::new(
        ReplicaProcessConfig {
            resource_uid: ResourceUid::new(&config.resource_uid),
            replica_id: ReplicaId::new(config.replica_id),
            pod_uid: PodUid::new(&config.pod_uid),
            pvc_uid: PvcUid::new(&config.pvc_uid),
            data_root: config.data_root.clone(),
            replication_address: config.replication_address,
            bearer_token: config.bearer_token.clone(),
            rpc_deadline: Duration::from_secs(5),
            transport_window_capacity: 256,
        },
        application,
        application_storage,
        Arc::new(KubernetesDnsResolver::new(
            ResourceUid::new(&config.resource_uid),
            config.namespace.clone(),
        )),
    )
    .with_application_storage_paths(std::collections::BTreeMap::from([(
        "application".into(),
        config.data_root.join("application"),
    )]))
}

fn application_router(state: HttpState) -> Router {
    Router::new()
        .route("/kv/{key}", put(put_value).get(get_value))
        .route("/status", get(get_status))
        .with_state(state)
}

fn copy_gate_router(copy_gate: Option<CopyGate>) -> Router {
    match copy_gate {
        Some(copy_gate) => Router::new()
            .route("/live-test/copy-gate/{action}", put(set_copy_gate))
            .with_state(copy_gate),
        None => Router::new(),
    }
}

async fn set_copy_gate(
    State(copy_gate): State<CopyGate>,
    Path(action): Path<String>,
) -> std::result::Result<&'static str, (StatusCode, String)> {
    match action.as_str() {
        "hold" => copy_gate.hold().map(|()| "held"),
        "release" => copy_gate.release().map(|()| "released"),
        _ => return Err((StatusCode::BAD_REQUEST, "unknown copy-gate action".into())),
    }
    .map_err(|error| (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()))
}

async fn put_value(
    State(state): State<HttpState>,
    Path(key): Path<String>,
    body: Bytes,
) -> std::result::Result<String, (StatusCode, String)> {
    let value = String::from_utf8(body.to_vec())
        .map_err(|error| (StatusCode::BAD_REQUEST, error.to_string()))?;
    state
        .application
        .replicate_put(key, value)
        .await
        .map(|lsn| lsn.to_string())
        .map_err(runtime_http_error)
}

async fn get_value(
    State(state): State<HttpState>,
    Path(key): Path<String>,
) -> std::result::Result<String, StatusCode> {
    state
        .application
        .get(&key)
        .await
        .map_err(|error| runtime_http_error(error).0)?
        .ok_or(StatusCode::NOT_FOUND)
}

async fn get_status(
    State(state): State<HttpState>,
) -> std::result::Result<axum::Json<ReplicaDiagnostics>, (StatusCode, String)> {
    state
        .replica
        .diagnostics()
        .await
        .map(axum::Json)
        .map_err(|error| (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()))
}

fn runtime_http_error(error: kuberic_runtime::RuntimeError) -> (StatusCode, String) {
    let status = match error {
        kuberic_runtime::RuntimeError::NotPrimary
        | kuberic_runtime::RuntimeError::NotOpen
        | kuberic_runtime::RuntimeError::WriteClosed(_)
        | kuberic_runtime::RuntimeError::ReadClosed(_) => StatusCode::SERVICE_UNAVAILABLE,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    };
    (status, error.to_string())
}

async fn wait_shutdown(mut shutdown: watch::Receiver<bool>) {
    let _ = shutdown.wait_for(|stopped| *stopped).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    use kvstore2::state::CopyGate;

    mod production_host {
        use super::CopyGate;
        #[tokio::test]
        async fn production_host_opens_fresh_application_storage() -> anyhow::Result<()> {
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

            let root = tempfile::tempdir()?;
            let config = super::Config {
                resource_uid: "binary-startup-resource".into(),
                replica_id: 1,
                pod_uid: "binary-startup-pod".into(),
                pvc_uid: "binary-startup-pvc".into(),
                pod_ip: "127.0.0.1".into(),
                set_name: "binary-startup".into(),
                namespace: "default".into(),
                bearer_token: "startup-token".into(),
                data_root: root.path().to_path_buf(),
                control_address: "127.0.0.1:0".parse()?,
                replication_address: "127.0.0.1:0".parse()?,
                application_address: "127.0.0.1:0".parse()?,
                live_test_copy_gate_address: None,
            };
            let application_path = root.path().join("application");
            assert!(super::KvPersistence::is_fresh_empty(&application_path)?);
            let application = super::Arc::new(super::KvService::new(
                super::Arc::new(super::KvPersistence::open(&application_path)?),
                "http://127.0.0.1:0".into(),
                CopyGate::disabled(),
            ));
            let host = super::replica_host(
                &config,
                application.clone(),
                super::ApplicationStorageState::FreshEmpty,
            );
            let listener = tokio::net::TcpListener::bind(config.control_address).await?;
            let address = format!("http://{}", listener.local_addr()?);
            let (shutdown, receiver) = tokio::sync::watch::channel(false);
            let (ready, mut ready_receiver) = tokio::sync::watch::channel(false);
            let startup = tokio::spawn(async move {
                if let Some(mut running) =
                    host.start_with_shutdown(listener, receiver.clone()).await?
                {
                    ready.send_replace(true);
                    super::wait_shutdown(receiver).await;
                    running.shutdown();
                    running.wait().await?;
                }
                Ok::<_, kuberic_runtime::host::HostError>(())
            });
            let result = tokio::time::timeout(super::Duration::from_secs(20), async {
                let mut client =
                    proto::agent_control_client::AgentControlClient::connect(address.clone())
                        .await?;
                let status_request = proto::GetAgentStatusRequest {
                    protocol_version: kuberic_runtime::protocol::PROTOCOL_VERSION,
                    resource_uid: config.resource_uid.clone(),
                    replica_id: config.replica_id,
                    expected_instance_id: config.pod_uid.clone(),
                };
                let status = client
                    .get_status(authorized(status_request.clone()))
                    .await?
                    .into_inner();
                anyhow::ensure!(
                    status.storage_state == proto::AgentStorageState::Uninitialized as i32
                );
                let resource = super::ResourceUid::new(&config.resource_uid);
                let replica_id = super::ReplicaId::new(config.replica_id);
                let initialization = derive_initialization_id(
                    &resource,
                    replica_id,
                    &super::PodUid::new(&config.pod_uid),
                    &super::PvcUid::new(&config.pvc_uid),
                );
                let identity = ReplicaIdentity {
                    replica_id,
                    instance_id: ReplicaInstanceId::new(&config.pod_uid),
                    agent_generation: derive_agent_generation(&initialization),
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
                                    local_replica_id: replica_id.value(),
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
                                    bootstrap_configuration: Some(configuration.clone().into()),
                                    provisioning: None,
                                },
                            ),
                        ),
                    }))
                    .await?;
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
                anyhow::ensure!(
                    report.open_mode == OpenMode::New,
                    "fresh production storage must authorize New, not Existing"
                );
                let query = || {
                    authorized(proto::ReplicaSnapshotRequest {
                        protocol_version: ORDINARY_ACTION_FORMAT,
                        resource_uid: resource.to_string(),
                        target: Some(identity.clone().into()),
                        expected_process_session_id: report.process_session_id.to_string(),
                    })
                };
                let response = client.get_snapshot(query()).await?.into_inner();
                let ReplicaAgentQueryResult::Initialized(snapshot) =
                    serde_json::from_slice(&response.canonical_json)?
                else {
                    anyhow::bail!("ordinary snapshot required")
                };
                let mut goal = ReplicaGoal {
                    format: ORDINARY_ACTION_FORMAT,
                    identity: report.identity,
                    operation_id: OperationId::new("binary-first-open"),
                    revision: 1,
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
                    },
                };
                for desired in [
                    ReplicaGoalKind::Prepare {
                        role: ReplicaRole::Primary,
                    },
                    ReplicaGoalKind::Reconfigure {
                        role: ReplicaRole::Primary,
                        catch_up: Some(PublicCatchUpMode::WriteQuorum),
                    },
                    ReplicaGoalKind::Close,
                ] {
                    goal.desired = desired;
                    goal.revision += 1;
                    goal.operation_id = OperationId::new(format!("binary-goal-{}", goal.revision));
                    client
                        .submit_goal(authorized(proto::ReplicaGoalRequest {
                            protocol_version: ORDINARY_ACTION_FORMAT,
                            canonical_json: serde_json::to_vec(&goal)?,
                        }))
                        .await?;
                    loop {
                        let response = client.get_snapshot(query()).await?.into_inner();
                        let ReplicaAgentQueryResult::Initialized(snapshot) =
                            serde_json::from_slice(&response.canonical_json)?
                        else {
                            anyhow::bail!("ordinary snapshot required")
                        };
                        if let Some(observation) =
                            snapshot.goals.iter().find(|entry| entry.goal == goal)
                            && observation.status != ReplicaGoalStatus::Pending
                        {
                            anyhow::ensure!(
                                observation.status == ReplicaGoalStatus::Achieved,
                                "{observation:?}"
                            );
                            break;
                        }
                        tokio::task::yield_now().await;
                    }
                    if matches!(goal.desired, ReplicaGoalKind::Reconfigure { .. }) {
                        application
                            .replicate_put("ordinary".into(), "acknowledged".into())
                            .await?;
                        anyhow::ensure!(
                            application.get("ordinary").await?.as_deref() == Some("acknowledged")
                        );
                    }
                }
                let opened = convert::normalize_public_v1_agent_status_report(
                    client
                        .get_status(authorized(status_request))
                        .await?
                        .into_inner(),
                )?;
                anyhow::ensure!(opened.open_mode == OpenMode::Existing);
                anyhow::ensure!(application_path.join("state.json").is_file());
                Ok::<_, anyhow::Error>(())
            })
            .await;
            shutdown.send_replace(true);
            let host_result = startup.await?;
            let application_result = application.shutdown_and_drain().await;
            result??;
            host_result?;
            application_result?;
            Ok(())
        }
    }

    #[test]
    fn diagnostics_distinguish_terminal_retirement_from_closed_access() {
        for retired in [false, true] {
            let diagnostics = super::ReplicaDiagnostics {
                replica_id: 3,
                instance_id: "pod-3".into(),
                agent_generation: "generation-3".into(),
                process_session: "restarted".into(),
                role: "None".into(),
                epoch: "0.2".into(),
                previous_configuration: None,
                current_configuration: None,
                current_progress:
                    kuberic_runtime::protocol::public_operations::ProgressQueryResult::Value(5),
                verified_replication_lsn: None,
                catch_up_capability:
                    kuberic_runtime::protocol::public_operations::ProgressQueryResult::Value(5),
                read_status: "NotPrimary".into(),
                write_status: "NotPrimary".into(),
                catch_up_boundary_lsn: None,
                scale_up_operation: None,
                retired,
                pending_operation: None,
                blocking: None,
                builds: Vec::new(),
            };
            let json = serde_json::to_value(diagnostics).unwrap();
            assert_eq!(json["retired"], retired);
            assert_eq!(json["role"], "None");
            assert_eq!(json["writeStatus"], "NotPrimary");
            assert!(json.get("retiredAuthority").is_none());
            assert!(json.get("acceptedSecondaryRemoval").is_none());
        }
    }

    #[test]
    fn retired_application_requests_are_explicitly_unavailable() {
        let (status, message) = super::runtime_http_error(kuberic_runtime::RuntimeError::NotOpen);
        assert_eq!(status, axum::http::StatusCode::SERVICE_UNAVAILABLE);
        assert!(!message.is_empty());
    }

    #[test]
    fn application_main_uses_the_agent_host_instead_of_protocol_internals() {
        let source = include_str!("main.rs");
        let production = source.split("#[cfg(test)]").next().unwrap();
        for internal in [
            "AgentService",
            "GrpcOutboundDispatcher",
            "InitializationService",
            "PodRuntime",
            "ReliableTransport",
            "SqliteStore",
            "run_outbound",
            "run_peer_discovery",
        ] {
            assert!(
                !production.contains(internal),
                "kvstore2 main must not assemble {internal}"
            );
        }
        assert!(production.contains("ReplicaHost::new"));
    }

    #[tokio::test]
    async fn default_router_does_not_register_live_test_copy_gate() {
        let response = super::copy_gate_router(None)
            .oneshot(
                Request::put("/live-test/copy-gate/hold")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), axum::http::StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn explicitly_enabled_live_test_router_arms_gate() {
        let directory = tempfile::tempdir().unwrap();
        let gate = CopyGate::enabled(directory.path().join("copy-gate")).unwrap();
        let response = super::copy_gate_router(Some(gate.clone()))
            .oneshot(
                Request::put("/live-test/copy-gate/hold")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), axum::http::StatusCode::OK);
        assert!(directory.path().join("copy-gate").is_file());
        gate.release().unwrap();
    }

    #[test]
    fn default_manifest_neither_enables_nor_routes_live_test_gate() {
        let sample = include_str!("../deploy/sample.yaml");
        let service = include_str!("../deploy/service.yaml");
        assert!(!sample.contains("testing.kuberic.io/live-copy-gate"));
        assert!(!sample.contains("KUBERIC_LIVE_TEST_COPY_GATE_ADDRESS"));
        assert!(service.contains("targetPort: 8080"));
        assert!(!service.contains("18080"));
    }
}
