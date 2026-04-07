//! Echo Stateful Service
//!
//! A minimal stateful service using kubelicate-core's PodRuntime.
//! The app starts the runtime and a user event loop. Operator commands
//! arrive via gRPC — the app never calls lifecycle methods directly.
//!
//! Usage:
//!   # Normal mode — waits for a real operator to connect via gRPC:
//!   cargo run -p echo-stateful
//!
//!   # Demo mode — simulates operator calls for quick testing:
//!   cargo run -p echo-stateful -- --demo
//!
//!   # Custom replica ID and ports:
//!   cargo run -p echo-stateful -- --replica-id 2 --control-port 9090 --data-port 9091

use std::time::Duration;

use bytes::Bytes;
use clap::Parser;
use kubelicate_core::events::ServiceEvent;
use kubelicate_core::handles::StateReplicatorHandle;
use kubelicate_core::pod::PodRuntime;
use kubelicate_core::types::{AccessStatus, CancellationToken, Role};
use tokio::sync::mpsc;
use tracing::{info, warn};

#[derive(Parser)]
#[command(name = "echo-stateful", about = "Example kubelicate stateful service")]
struct Args {
    /// Replica ID for this instance.
    #[arg(long, default_value = "1")]
    replica_id: i64,

    /// Bind address for the gRPC control server.
    #[arg(long, default_value = "127.0.0.1:0")]
    control_bind: String,

    /// Bind address for the gRPC data server.
    #[arg(long, default_value = "127.0.0.1:0")]
    data_bind: String,

    /// Run in demo mode: simulate operator calls for quick testing.
    #[arg(long)]
    demo: bool,

    /// Duration in seconds to run as primary in demo mode.
    #[arg(long, default_value = "3")]
    demo_duration: u64,
}

async fn run_service(mut events: mpsc::Receiver<ServiceEvent>) {
    let mut partition = None;
    let mut replicator: Option<StateReplicatorHandle> = None;
    let mut bg_handle: Option<tokio::task::JoinHandle<()>> = None;
    let mut bg_token: Option<CancellationToken> = None;

    info!("echo service started, waiting for events");

    while let Some(event) = events.recv().await {
        match event {
            ServiceEvent::Open { ctx, reply } => {
                info!("service opened");
                partition = Some(ctx.partition);
                replicator = Some(ctx.replicator);
                let _ = reply.send(Ok(()));
            }
            ServiceEvent::ChangeRole { new_role, reply } => {
                info!(?new_role, "role changed");
                if let Some(token) = bg_token.take() {
                    token.cancel();
                }
                if let Some(handle) = bg_handle.take() {
                    let _ = handle.await;
                }

                if new_role == Role::Primary {
                    let p = partition.clone().unwrap();
                    let r = replicator.clone().unwrap();
                    let token = CancellationToken::new();
                    let child = token.child_token();
                    bg_token = Some(token);
                    bg_handle = Some(tokio::spawn(echo_loop(p, r, child)));
                }
                let _ = reply.send(Ok(String::new()));
            }
            ServiceEvent::Close { reply } => {
                info!("service closing");
                if let Some(token) = bg_token.take() {
                    token.cancel();
                }
                if let Some(handle) = bg_handle.take() {
                    let _ = handle.await;
                }
                let _ = reply.send(Ok(()));
                break;
            }
            ServiceEvent::Abort => {
                if let Some(token) = bg_token.take() {
                    token.cancel();
                }
                break;
            }
        }
    }
    info!("echo service exited");
}

async fn echo_loop(
    partition: std::sync::Arc<kubelicate_core::handles::PartitionHandle>,
    replicator: StateReplicatorHandle,
    token: CancellationToken,
) {
    let mut counter = 0u64;
    info!("echo loop started (primary)");

    while !token.is_cancelled() {
        match partition.write_status() {
            AccessStatus::Granted => {
                counter += 1;
                let msg = format!("echo-{}", counter);
                match replicator
                    .replicate(Bytes::from(msg.clone()), token.clone())
                    .await
                {
                    Ok(lsn) => info!(lsn, msg, "replicated"),
                    Err(e) => {
                        warn!(error = %e, "replicate failed");
                        break;
                    }
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            AccessStatus::NoWriteQuorum => {
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            _ => break,
        }
    }
    info!("echo loop stopped");
}

async fn simulate_operator(control_address: String, demo_duration: u64) {
    use kubelicate_core::proto::replicator_control_client::ReplicatorControlClient;
    use kubelicate_core::proto::*;

    tokio::time::sleep(Duration::from_millis(100)).await;

    let mut client = ReplicatorControlClient::connect(control_address)
        .await
        .expect("connect to control server");

    info!("--- Operator: Open ---");
    client.open(OpenRequest { mode: 0 }).await.unwrap();

    info!("--- Operator: Idle → Active → Primary ---");
    let epoch = Some(EpochProto {
        data_loss_number: 0,
        configuration_number: 1,
    });
    client
        .change_role(ChangeRoleRequest {
            epoch,
            role: RoleProto::RoleIdleSecondary as i32,
        })
        .await
        .unwrap();
    client
        .change_role(ChangeRoleRequest {
            epoch,
            role: RoleProto::RoleActiveSecondary as i32,
        })
        .await
        .unwrap();
    client
        .change_role(ChangeRoleRequest {
            epoch,
            role: RoleProto::RolePrimary as i32,
        })
        .await
        .unwrap();

    info!(
        "--- Operator: Running as primary for {} seconds ---",
        demo_duration
    );
    tokio::time::sleep(Duration::from_secs(demo_duration)).await;

    info!("--- Operator: Demote ---");
    client
        .change_role(ChangeRoleRequest {
            epoch: Some(EpochProto {
                data_loss_number: 0,
                configuration_number: 2,
            }),
            role: RoleProto::RoleActiveSecondary as i32,
        })
        .await
        .unwrap();

    info!("--- Operator: Close ---");
    client.close(CloseRequest {}).await.unwrap();
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();
    let args = Args::parse();

    info!("=== Echo Stateful Service ===");

    let bundle = PodRuntime::builder(args.replica_id)
        .reply_timeout(Duration::from_secs(10))
        .control_bind(args.control_bind)
        .data_bind(args.data_bind)
        .build()
        .await?;

    let control_address = bundle.control_address.clone();
    let shutdown = bundle.runtime.shutdown_token();
    info!(control = %control_address, data = %bundle.data_address, "ready");

    let runtime_handle = tokio::spawn(bundle.runtime.serve());
    let service_handle = tokio::spawn(run_service(bundle.service_rx));

    if args.demo {
        info!("Demo mode: simulating operator calls");
        simulate_operator(control_address, args.demo_duration).await;
    } else {
        info!("Waiting for operator commands on {}", control_address);
        info!("Press Ctrl+C to shut down");
        wait_for_signal(shutdown).await;
    }

    // Wait for both tasks to finish
    let _ = service_handle.await;
    let _ = runtime_handle.await;

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
