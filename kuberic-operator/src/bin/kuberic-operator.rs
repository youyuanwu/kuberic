use std::{net::Ipv4Addr, sync::Arc, time::Duration};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use kube::CustomResourceExt;
use kuberic_operator::{Config, Operator};
use tokio::signal::unix::{signal, SignalKind};

#[derive(Parser)]
struct Arguments {
    #[command(subcommand)]
    command: Operation,
}

#[derive(Subcommand)]
enum Operation {
    Run {
        #[arg(long, env = "POD_NAMESPACE")]
        namespace: String,
        #[arg(long, env = "POD_IP")]
        advertise_ip: Ipv4Addr,
        #[arg(long, default_value_t = 19001)]
        node_port: u16,
        #[arg(long, default_value_t = 180_000)]
        teardown_timeout_ms: u64,
        #[arg(long, default_value_t = 250)]
        requeue_ms: u64,
    },
    Crd,
    Manifests {
        #[arg(long)]
        namespace: String,
        #[arg(long)]
        image: String,
    },
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    match Arguments::parse().command {
        Operation::Crd => println!(
            "{}",
            serde_json::to_string_pretty(&kuberic_operator::KubericSet::crd())?
        ),
        Operation::Manifests { namespace, image } => println!(
            "{}",
            serde_json::to_string_pretty(&kuberic_operator::deploy::installation(
                &namespace, &image
            )?)?
        ),
        Operation::Run {
            namespace,
            advertise_ip,
            node_port,
            teardown_timeout_ms,
            requeue_ms,
        } => {
            rustls::crypto::ring::default_provider()
                .install_default()
                .map_err(|_| anyhow::anyhow!("operator TLS provider was already installed"))?;
            let mut term =
                signal(SignalKind::terminate()).context("installing operator SIGTERM handler")?;
            let mut interrupt =
                signal(SignalKind::interrupt()).context("installing operator SIGINT handler")?;
            let operator = Arc::new(Operator::new(
                kube::Client::try_default().await?,
                Config {
                    namespace,
                    advertise_ip,
                    node_port,
                    teardown_timeout_ms,
                    requeue: Duration::from_millis(requeue_ms),
                },
            )?);
            operator
                .run(async move {
                    tokio::select! {
                        _ = term.recv() => {}
                        _ = interrupt.recv() => {}
                    }
                })
                .await?;
        }
    }
    Ok(())
}
