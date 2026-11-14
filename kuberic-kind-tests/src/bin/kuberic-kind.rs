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
        #[arg(long, conflicts_with_all = ["staggered_shutdown", "primary_app_failover"])]
        early_delete: bool,
        #[arg(long, conflicts_with = "primary_app_failover")]
        staggered_shutdown: bool,
        #[arg(long)]
        primary_app_failover: bool,
    },
    CrashPrimaryApp {
        #[arg(long)]
        cohort: String,
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
            primary_app_failover,
        } => {
            let scenario = if early_delete {
                Scenario::DeleteDuringBootstrap
            } else if staggered_shutdown {
                Scenario::StaggeredShutdown
            } else if primary_app_failover {
                Scenario::PrimaryAppFailover
            } else {
                Scenario::Bootstrap
            };
            driver::run(scenario).await?;
        }
        Operation::CrashPrimaryApp { cohort } => {
            driver::crash_primary_app(&serde_json::from_str(&cohort)?)?;
        }
    }
    Ok(())
}
