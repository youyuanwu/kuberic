use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use kubelicate_core::pod::PodRuntime;
use kubelicate_core::types::CancellationToken;
use tokio::sync::RwLock;
use tracing::info;

use kv_stateful::state::{KvState, SharedState};

#[derive(Parser)]
#[command(name = "kv-stateful", about = "Replicated key-value store example")]
struct Args {
    /// Replica ID for this instance.
    #[arg(long, default_value = "1")]
    replica_id: i64,

    /// Bind address for the gRPC control server (operator → pod).
    #[arg(long, default_value = "127.0.0.1:0")]
    control_bind: String,

    /// Bind address for the gRPC data server (primary → secondary replication).
    #[arg(long, default_value = "127.0.0.1:0")]
    data_bind: String,

    /// Bind address for the client-facing KV gRPC server.
    #[arg(long, default_value = "127.0.0.1:0")]
    client_bind: String,

    /// Data directory for persistent state.
    #[arg(long, default_value = "/var/lib/kv-stateful/data")]
    data_dir: PathBuf,

    /// Run in demo mode: simulates operator + client for quick testing.
    #[arg(long)]
    demo: bool,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();
    let args = Args::parse();

    info!("=== KV-Stateful: Replicated Key-Value Store ===");

    let bundle = PodRuntime::builder(args.replica_id)
        .reply_timeout(Duration::from_secs(10))
        .control_bind(args.control_bind)
        .data_bind(args.data_bind)
        .build()
        .await?;

    let control_address = bundle.control_address.clone();
    let shutdown = bundle.runtime.shutdown_token();
    let state: SharedState = Arc::new(RwLock::new(KvState::open(args.data_dir).await?));

    info!(
        control = %control_address,
        data = %bundle.data_address,
        client_bind = %args.client_bind,
        "ready"
    );

    let runtime_handle = tokio::spawn(bundle.runtime.serve());
    let service_handle = tokio::spawn(kv_stateful::service::run_service(
        bundle.lifecycle_rx,
        bundle.state_provider_rx,
        state.clone(),
        args.client_bind.clone(),
    ));

    if args.demo {
        info!("Demo mode: simulating operator + client");
        kv_stateful::demo::simulate_operator(control_address.clone()).await;
        kv_stateful::demo::run_demo_client(args.client_bind).await;
        kv_stateful::demo::demo_close(control_address).await;
    } else {
        info!("Waiting for operator commands on {}", control_address);
        info!("Press Ctrl+C to shut down");
        wait_for_signal(shutdown).await;
    }

    let _ = service_handle.await;
    let _ = runtime_handle.await;

    let st = state.read().await;
    info!(
        entries = st.data.len(),
        last_lsn = st.last_applied_lsn,
        "final state"
    );

    info!("=== Shutdown ===");
    Ok(())
}

async fn wait_for_signal(shutdown: CancellationToken) {
    use tokio::signal::unix::{SignalKind, signal};
    let mut sigterm = signal(SignalKind::terminate()).expect("SIGTERM handler");
    let mut sigint = signal(SignalKind::interrupt()).expect("SIGINT handler");
    tokio::select! {
        _ = sigterm.recv() => info!("received SIGTERM"),
        _ = sigint.recv() => info!("received SIGINT"),
    }
    info!("initiating graceful shutdown");
    shutdown.cancel();
}
