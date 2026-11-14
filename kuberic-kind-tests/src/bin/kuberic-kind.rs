use anyhow::Result;
use clap::{Parser, Subcommand};
use kube::CustomResourceExt;
use kuberic_kind_tests::{driver, host::Cluster};

#[derive(Parser)]
struct Arguments {
    #[command(subcommand)]
    command: Operation,
}

#[derive(Subcommand)]
enum Operation {
    Create,
    Verify,
    Delete,
    Images,
    Install,
    Diagnostics,
    Crd,
    Run {
        #[arg(long)]
        early_delete: bool,
    },
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    match Arguments::parse().command {
        Operation::Create => Cluster::requested()?.create()?,
        Operation::Verify => Cluster::from_env()?.verify()?,
        Operation::Delete => Cluster::from_env()?.delete()?,
        Operation::Images => {
            println!(
                "{}",
                kuberic_kind_tests::host::prepare_image(&Cluster::from_env()?)?
            );
        }
        Operation::Install => Cluster::from_env()?.install()?,
        Operation::Diagnostics => Cluster::from_env()?.diagnostics()?,
        Operation::Crd => println!(
            "{}",
            serde_json::to_string_pretty(&kuberic_controller::KubericSet::crd())?
        ),
        Operation::Run { early_delete } => driver::run(early_delete).await?,
    }
    Ok(())
}
