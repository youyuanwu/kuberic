mod persistence;
mod service;
mod state;
mod status;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, put};
use clap::Parser;
use kuberic_agent::hosting::PodRuntime;
use kuberic_agent::provisioning::{ObservedStorageIdentity, validate_established_identity};
use kuberic_agent::service::{AgentService, InitializationService};
use kuberic_agent::sqlite_store::SqliteStore;
use kuberic_agent::store::AgentStore;
use kuberic_agent::transport::{
    GrpcOutboundDispatcher, KubernetesDnsResolver, ReliableTransport, run_outbound,
    run_peer_discovery,
};
use kuberic_protocol::types::{PodUid, PvcUid, ReplicaId, ReplicaInstanceId, ResourceUid};
use tokio::sync::{Mutex, watch};

use crate::persistence::KvPersistence;
use crate::service::KvService;

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
}

#[derive(Clone)]
struct HttpState {
    application: Arc<KvService>,
    runtime: Arc<PodRuntime>,
    store: Arc<SqliteStore>,
    process_session: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let config = Config::parse();
    if config.replica_id <= 0 {
        anyhow::bail!("KUBERIC_REPLICA_ID must be positive");
    }
    let database_path = SqliteStore::metadata_database_path(&config.data_root);
    let application_path = config.data_root.join("application");
    if !database_path.is_file() {
        serve_initialization(
            &config,
            database_path.clone(),
            KvPersistence::is_fresh_empty(&application_path)?,
        )
        .await?;
    }

    let store = Arc::new(
        SqliteStore::open_existing(&database_path, None)
            .context("opening durable Kuberic metadata")?,
    );
    let identity = store.identity().await?;
    validate_established_identity(
        &identity,
        &observed_storage_identity(&config),
        ReplicaId::new(config.replica_id),
    )?;
    let persistence = Arc::new(KvPersistence::open(application_path)?);
    let application = Arc::new(KvService::new(
        persistence,
        format!("http://{}:50052", config.pod_ip),
    ));
    let runtime = Arc::new(PodRuntime::new(
        identity.local_identity.clone(),
        application.clone(),
        store.clone(),
    ));
    let agent = AgentService::new(
        store.clone(),
        runtime.clone(),
        runtime.clone(),
        config.bearer_token.clone(),
    )?;
    let process_session = agent.sessions().local_session().to_string();
    let sessions = agent.sessions().clone();
    let transport = Arc::new(Mutex::new(ReliableTransport::new(
        agent.sessions().local_session().clone(),
        256,
    )?));
    let dispatcher = Arc::new(GrpcOutboundDispatcher::new(
        runtime.clone(),
        transport.clone(),
        Arc::new(KubernetesDnsResolver::new(
            config.set_name.clone(),
            config.namespace.clone(),
        )),
        config.resource_uid.clone(),
        config.bearer_token.clone(),
        Duration::from_secs(5),
    )?);
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let (ready_tx, mut ready_rx) = watch::channel(false);

    let mut agent_task = tokio::spawn(agent.serve(
        config.control_address,
        config.replication_address,
        ready_tx,
        shutdown_rx.clone(),
    ));
    tokio::select! {
        result = &mut agent_task => {
            result??;
            anyhow::bail!("agent service stopped before becoming ready");
        }
        result = ready_rx.wait_for(|ready| *ready) => {
            result.context("agent readiness channel closed")?;
        }
    }
    let outbound_task = tokio::spawn(run_outbound(
        runtime.clone(),
        transport.clone(),
        dispatcher.clone(),
        shutdown_rx.clone(),
    ));
    let peer_task = tokio::spawn(run_peer_discovery(
        identity.local_identity,
        store.clone(),
        transport.clone(),
        dispatcher.clone(),
        sessions,
        shutdown_rx.clone(),
    ));
    let http_state = HttpState {
        application,
        runtime,
        store,
        process_session,
    };
    let router = Router::new()
        .route("/kv/{key}", put(put_value).get(get_value))
        .route("/status", get(get_status))
        .with_state(http_state);
    let listener = tokio::net::TcpListener::bind(config.application_address).await?;
    let mut http_task = tokio::spawn(
        axum::serve(listener, router)
            .with_graceful_shutdown(wait_shutdown(shutdown_rx.clone()))
            .into_future(),
    );

    tokio::select! {
        result = &mut agent_task => result??,
        result = outbound_task => result??,
        result = peer_task => result??,
        result = &mut http_task => result??,
        result = tokio::signal::ctrl_c() => result?,
    }

    shutdown_tx.send_replace(true);
    Ok(())
}

fn observed_storage_identity(config: &Config) -> ObservedStorageIdentity {
    ObservedStorageIdentity {
        resource_uid: ResourceUid::new(&config.resource_uid),
        pod_uid: PodUid::new(&config.pod_uid),
        pvc_uid: PvcUid::new(&config.pvc_uid),
        instance_id: ReplicaInstanceId::new(&config.pod_uid),
    }
}

async fn serve_initialization(
    config: &Config,
    database_path: PathBuf,
    fresh_application_state: bool,
) -> Result<()> {
    let observed = observed_storage_identity(config);
    let (initialized_tx, mut initialized_rx) = watch::channel(false);
    let service = InitializationService::new(
        observed,
        ReplicaId::new(config.replica_id),
        database_path,
        config.bearer_token.clone(),
        initialized_tx,
        fresh_application_state,
    )?;
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let (ready_tx, _) = watch::channel(false);
    let stop = tokio::spawn(async move {
        let _ = initialized_rx.wait_for(|initialized| *initialized).await;
        shutdown_tx.send_replace(true);
    });
    service
        .serve(config.control_address, ready_tx, shutdown_rx)
        .await?;
    stop.await?;
    Ok(())
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
        .persistence()
        .get(&key)
        .ok_or(StatusCode::NOT_FOUND)
}

async fn get_status(
    State(state): State<HttpState>,
) -> std::result::Result<axum::Json<status::ReplicaDiagnostics>, (StatusCode, String)> {
    status::diagnostics(&state.runtime, &state.store, &state.process_session)
        .await
        .map(axum::Json)
        .map_err(|error| (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()))
}

fn runtime_http_error(error: kuberic_runtime::RuntimeError) -> (StatusCode, String) {
    let status = match error {
        kuberic_runtime::RuntimeError::NotPrimary
        | kuberic_runtime::RuntimeError::WriteClosed(_) => StatusCode::SERVICE_UNAVAILABLE,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    };
    (status, error.to_string())
}

async fn wait_shutdown(mut shutdown: watch::Receiver<bool>) {
    let _ = shutdown.wait_for(|stopped| *stopped).await;
}
