use tokio_util::sync::CancellationToken;
use tonic::transport::Server;
use tracing::{error, info};

mod greeter;
use greeter::{GreeterServer, MyGreeter};

mod storage;
use storage::{MyStorage, StorageServer};

pub mod leader;
use leader::LeaderElection;
pub mod leader_rpc;
use leader_rpc::{LeaderElectionServer, MyLeaderElection};
pub mod app;

pub mod leader2;

#[tokio::main]
async fn main() {
    // Initialize tracing subscriber
    tracing_subscriber::fmt()
        .with_target(false)
        .with_thread_ids(true)
        .with_level(true)
        .init();

    let addr = "[::]:8080".parse().unwrap();
    let greeter = MyGreeter::default();
    let storage = MyStorage::default();

    // Initialize leader election
    let leader_state = LeaderElection::new(
        "xedio".to_string(),
        "xdata-app-lease".to_string(),
        std::time::Duration::from_secs(15),
    );

    let (app, leader_state_arc) = app::App::new(leader_state);

    info!("Starting gRPC server on {}", addr);

    // Create a cancellation token
    let cancellation_token = CancellationToken::new();
    let token_sig = cancellation_token.clone();
    let token_app = cancellation_token.clone();

    // Spawn leader election loop
    let app_task = tokio::spawn(async move {
        app.run(token_app).await;
    });

    // Spawn a task to listen for shutdown signals
    tokio::spawn(async move {
        wait_for_shutdown_signal().await;
        info!("Shutdown signal received, cancelling server...");
        token_sig.cancel();
    });

    // Run the server with graceful shutdown using the cancellation token
    let result = Server::builder()
        .add_service(GreeterServer::new(greeter))
        .add_service(StorageServer::new(storage))
        .add_service(LeaderElectionServer::new(MyLeaderElection::new(
            leader_state_arc,
        )))
        .serve_with_shutdown(addr, cancellation_token.cancelled())
        .await;

    if let Err(e) = result {
        error!("Server error: {}", e);
    }
    app_task.await.unwrap();
    info!("Server shutdown complete");
}

/// k8s uses sigterm to signal shutdown
async fn wait_for_shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};
    let mut sigterm = signal(SignalKind::terminate()).expect("Failed to setup SIGTERM handler");
    let mut sigint = signal(SignalKind::interrupt()).expect("Failed to setup SIGINT handler");
    tokio::select! {
        _ = sigterm.recv() => {
            info!("Received SIGTERM, shutting down.");
        }
        _ = sigint.recv() => {
            info!("Received SIGINT, shutting down.");
        }
    }
}
