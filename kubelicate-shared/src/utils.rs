/// k8s uses sigterm to signal shutdown
pub async fn wait_for_shutdown_signal() {
    use tokio::signal::unix::{SignalKind, signal};
    let mut sigterm = signal(SignalKind::terminate()).expect("Failed to setup SIGTERM handler");
    let mut sigint = signal(SignalKind::interrupt()).expect("Failed to setup SIGINT handler");
    tokio::select! {
        _ = sigterm.recv() => {
            tracing::info!("Received SIGTERM, shutting down.");
        }
        _ = sigint.recv() => {
            tracing::info!("Received SIGINT, shutting down.");
        }
    }
}
