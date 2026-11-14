use anyhow::Result;
use clap::{Parser, Subcommand};
use kube::CustomResourceExt;
use kuberic_kind_tests::{driver, host::Cluster, Scenario};

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
        #[arg(long, conflicts_with = "staggered_shutdown")]
        early_delete: bool,
        #[arg(long)]
        staggered_shutdown: bool,
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
            serde_json::to_string_pretty(&kuberic_operator::KubericSet::crd())?
        ),
        Operation::Run {
            early_delete,
            staggered_shutdown,
        } => {
            let scenario = if early_delete {
                Scenario::DeleteDuringBootstrap
            } else if staggered_shutdown {
                Scenario::StaggeredShutdown
            } else {
                Scenario::Bootstrap
            };
            driver::run(scenario).await?;
        }
    }
    Ok(())
}
