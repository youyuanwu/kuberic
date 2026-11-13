use std::{io::Write, path::PathBuf, process::ExitCode};

use anyhow::{Context, Result};
use clap::Parser;
use kuberic_fm::{Config, Status};
use tokio::signal::unix::{signal, SignalKind};

#[derive(Parser)]
#[command(
    version,
    about = "Process-only external FM for fube's fresh volatile 3/2 profile"
)]
struct Arguments {
    #[arg(long)]
    config: PathBuf,
}

async fn shutdown() -> Result<()> {
    let mut term = signal(SignalKind::terminate())?;
    tokio::select! {
        result = tokio::signal::ctrl_c() => result.context("waiting for SIGINT"),
        result = term.recv() => result.context("SIGTERM stream closed"),
    }
}

fn report(status: &Status) -> Result<()> {
    let mut output = std::io::stdout().lock();
    serde_json::to_writer(
        &mut output,
        &serde_json::json!({
            "event": "status",
            "status": status,
        }),
    )?;
    output.write_all(b"\n")?;
    output.flush()?;
    Ok(())
}

async fn execute() -> Result<()> {
    let config = Config::read(&Arguments::parse().config)?;
    let listener = tokio::net::TcpListener::bind(config.fm.address)
        .await
        .context("binding FM listener")?;
    kuberic_fm::run(config, listener, shutdown(), report).await
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    match execute().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!(
                "{}",
                serde_json::json!({"event": "error", "error": format!("{error:#}")})
            );
            ExitCode::FAILURE
        }
    }
}
