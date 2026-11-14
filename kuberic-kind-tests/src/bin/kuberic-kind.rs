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
        #[arg(long, conflicts_with_all = ["staggered_shutdown", "primary_app_failover", "secondary_app_repair"])]
        early_delete: bool,
        #[arg(long, conflicts_with_all = ["primary_app_failover", "secondary_app_repair"])]
        staggered_shutdown: bool,
        #[arg(long, conflicts_with = "secondary_app_repair")]
        primary_app_failover: bool,
        #[arg(long)]
        secondary_app_repair: bool,
        #[arg(long, requires = "secondary_app_repair")]
        hold_copy: bool,
    },
    CrashPrimaryApp {
        #[arg(long)]
        cohort: String,
        #[arg(long)]
        node_index: usize,
    },
    CrashSecondaryApp {
        #[arg(long)]
        cohort: String,
        #[arg(long)]
        node_index: usize,
    },
    InspectNode {
        #[arg(long)]
        cohort: String,
        #[arg(long)]
        node_index: usize,
    },
    CopyBarrier {
        #[arg(long)]
        cohort: String,
        #[arg(long)]
        node_index: usize,
        #[arg(long, conflicts_with = "release")]
        hold: bool,
        #[arg(long)]
        release: bool,
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
            secondary_app_repair,
            hold_copy,
        } => {
            let scenario = if early_delete {
                Scenario::DeleteDuringBootstrap
            } else if staggered_shutdown {
                Scenario::StaggeredShutdown
            } else if primary_app_failover {
                Scenario::PrimaryAppFailover
            } else if hold_copy {
                Scenario::CopyHeldSecondaryRepair
            } else if secondary_app_repair {
                Scenario::SecondaryAppRepair
            } else {
                Scenario::Bootstrap
            };
            driver::run(scenario).await?;
        }
        Operation::CrashPrimaryApp { cohort, node_index } => {
            driver::crash_primary_app(&serde_json::from_str(&cohort)?, node_index)?;
        }
        Operation::CrashSecondaryApp { cohort, node_index } => {
            driver::crash_secondary_app(&serde_json::from_str(&cohort)?, node_index)?;
        }
        Operation::InspectNode { cohort, node_index } => {
            println!(
                "{}",
                driver::inspect_node(&serde_json::from_str(&cohort)?, node_index)?
            );
        }
        Operation::CopyBarrier {
            cohort,
            node_index,
            hold,
            release,
        } => {
            println!(
                "{}",
                driver::copy_barrier(&serde_json::from_str(&cohort)?, node_index, hold, release)?
            );
        }
    }
    Ok(())
}
