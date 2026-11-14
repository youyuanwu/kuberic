use std::{path::PathBuf, process::ExitCode};

use clap::Parser;

#[derive(Parser)]
struct Arguments {
    #[arg(long)]
    config: PathBuf,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    match kuberic_operator::node::run(&Arguments::parse().config).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            let message = format!("node cleanup observer failed: {error:#}");
            eprintln!("{message}");
            if let Err(error) = std::fs::write("/dev/termination-log", message) {
                eprintln!("writing failure termination message: {error}");
            }
            ExitCode::FAILURE
        }
    }
}
