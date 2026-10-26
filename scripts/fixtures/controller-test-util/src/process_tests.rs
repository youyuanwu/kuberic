use std::sync::Arc;

use anyhow::{Result, ensure};
use kuberic_controller::test_util::{
    ApplicationFactory, CreatedApplication, ReplicaContext, TestCluster,
};
use kuberic_runtime::application::OpenMode;

use super::{CounterFactory, CounterService};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_counter_failed_owner_restores_without_copy_and_remains_consuming_secondary()
-> Result<()> {
    use super::retained_witness::{WitnessFactory, helper_path, qualify_retained_secondary};
    qualify_retained_secondary(WitnessFactory::new(
        CounterFactory,
        helper_path(
            "KUBERIC_RETAINED_COUNTER_MEMBER_BIN",
            "kuberic-retained-counter-member",
        ),
    ))
    .await
}

/// Opt-in fixture tier: a missing explicitly built helper is a failure, not a skip.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_member_bootstrap_stop_reap_restart_denies_old_data_proxy() -> Result<()> {
    let mut cluster = TestCluster::builder(CounterFactory)
        .initial_replicas(1)
        .start()
        .await?;
    let result = async {
        let topology = cluster.settle_members(1).await?;
        let old = topology.primary().clone();
        let writer = cluster.service(&old).await?;
        ensure!(writer.store.is_none(), "parent opened a counter provider");
        let proxy = writer
            .proxy
            .as_ref()
            .expect("process fixture did not select a child");
        let old_generation = proxy.member_generation().unwrap().to_owned();
        let observation = proxy.member_observation()?;
        ensure!(
            observation["pid"]
                .as_u64()
                .is_some_and(|pid| { pid > 0 && pid != u64::from(std::process::id()) }),
            "counter data adapter is not in an independent OS child"
        );
        ensure!(writer.try_open_modes()? == [OpenMode::New]);
        cluster
            .drive(async { Ok::<_, anyhow::Error>(writer.set(71).await?) })
            .await?;
        ensure!(writer.read().await? == 71);
        cluster.stop(&old).await?;
        ensure!(writer.set(72).await.is_err());
        ensure!(writer.read().await.is_err());
        ensure!(writer.committed_value().is_err());
        ensure!(writer.try_open_modes().is_err());
        cluster.step().await?;
        ensure!(
            cluster.service(&old).await.is_err(),
            "deliberately stopped child was restarted"
        );
        let fresh = cluster.restart(&old).await?;
        let service = cluster.service(&fresh).await?;
        ensure!(
            service.proxy.as_ref().unwrap().member_generation() != Some(old_generation.as_str())
        );
        ensure!(service.try_open_modes()? == [OpenMode::Existing]);
        ensure!(service.committed_value()? == 71);
        ensure!(writer.set(73).await.is_err(), "old proxy reached successor");
        ensure!(writer.read().await.is_err());
        cluster
            .drive(async { Ok::<_, anyhow::Error>(service.set(79).await?) })
            .await?;
        ensure!(service.read().await? == 79);
        Ok::<_, anyhow::Error>(())
    }
    .await;
    cluster.finish(result).await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_child_attachment_required_for_transport_cut_and_stale_refs_refuse() -> Result<()> {
    let mut cluster = TestCluster::builder(CounterFactory).start().await?;
    let result = async {
        let topology = cluster.settle().await?;
        let secondary = topology.secondaries().next().unwrap().clone();
        let mut cut = cluster.isolate(std::slice::from_ref(&secondary)).await?;
        cut.entered().await?;
        cluster.heal(cut).await?;
        cluster.stop(&secondary).await?;
        ensure!(
            cluster
                .isolate(std::slice::from_ref(&secondary))
                .await
                .is_err()
        );
        Ok::<_, anyhow::Error>(())
    }
    .await;
    cluster.finish(result).await?;
    Ok(())
}

struct MissingExecutable;

#[async_trait::async_trait]
impl ApplicationFactory for MissingExecutable {
    type Service = CounterService;

    async fn create(
        &self,
        _: ReplicaContext,
    ) -> kuberic_controller::test_util::Result<CreatedApplication<Self::Service>> {
        panic!("missing helper must fail before application creation")
    }
    async fn shutdown_and_drain(
        &self,
        _: Arc<Self::Service>,
    ) -> kuberic_controller::test_util::Result<()> {
        panic!("missing helper cannot have an application drain")
    }
    fn member_executable(&self) -> Option<std::path::PathBuf> {
        Some(
            std::env::current_dir()
                .unwrap()
                .join("absent-counter-member-executable"),
        )
    }
}

#[tokio::test]
async fn missing_member_executable_is_a_hard_failure_without_a_runtime_build() -> Result<()> {
    let mut cluster = TestCluster::builder(MissingExecutable)
        .initial_replicas(1)
        .start()
        .await?;
    let result = cluster.settle_members(1).await;
    ensure!(result.is_err(), "missing executable was treated as success");
    let finished = cluster.finish(Ok::<_, anyhow::Error>(())).await;
    ensure!(finished.is_err(), "failed startup was erased by cleanup");
    Ok(())
}
