pub mod configmapgen;
pub mod podset;
pub mod shared;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();

    let token = tokio_util::sync::CancellationToken::new();
    tokio::spawn({
        let token = token.clone();
        async move {
            kubelicate_shared::utils::wait_for_shutdown_signal().await;
            token.cancel();
        }
    });

    let (mut reload_tx, reload_rx) = futures::channel::mpsc::channel(0);
    // Using a regular background thread since tokio::io::stdin() doesn't allow aborting reads,
    // and its worker prevents the Tokio runtime from shutting down.
    std::thread::spawn(move || {
        use std::io::BufRead;
        for _ in std::io::BufReader::new(std::io::stdin()).lines() {
            let _ = reload_tx.try_send(());
        }
    });

    let client = kube::Client::try_default().await?;
    configmapgen::controller::run_controller(token, client, reload_rx).await?;

    Ok(())
}
