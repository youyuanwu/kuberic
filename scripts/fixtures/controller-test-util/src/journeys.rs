use anyhow::{Result, ensure};
use kuberic_controller::test_util::TestCluster;

use super::CounterFactory;

async fn cluster() -> kuberic_controller::test_util::Result<TestCluster<CounterFactory>> {
    TestCluster::builder(CounterFactory)
        .image("in-process/downstream-counter")
        .start()
        .await
}

async fn finish(cluster: TestCluster<CounterFactory>, result: Result<()>) -> Result<()> {
    cluster.finish(result).await.map_err(|error| {
        if let Some(diagnostics) = error.diagnostics() {
            eprintln!("redacted harness diagnostics: {diagnostics}");
        }
        anyhow::Error::new(error)
    })
}

async fn assert_values(
    cluster: &mut TestCluster<CounterFactory>,
    members: usize,
    expected: i64,
) -> Result<()> {
    let topology = cluster.settle_members(members).await?;
    let mut services = Vec::new();
    for replica in topology.members() {
        services.push(cluster.service(replica).await?);
    }
    cluster
        .drive(async {
            loop {
                let mut matches = 0;
                for service in &services {
                    if service.read().await? == expected {
                        matches += 1;
                    }
                }
                if matches == services.len() {
                    return Ok::<_, anyhow::Error>(());
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn own_application_bootstraps_restarts_and_replaces_with_acknowledged_data() -> Result<()> {
    let mut cluster = cluster().await?;
    let result = async {
        let topology = cluster.settle().await?;
        let writer = cluster.service(topology.primary()).await?;
        cluster
            .drive(async { Ok::<_, anyhow::Error>(writer.set(17).await?) })
            .await?;
        let secondary = topology.secondaries().next().unwrap().clone();
        let reopened = cluster.restart(&secondary).await?;
        let service = cluster.service(&reopened).await?;
        ensure!(service.read().await? == 17);
        ensure!(
            cluster.service(&secondary).await.is_err(),
            "old session reference resolved"
        );

        let topology = cluster.settle().await?;
        let replaced = topology.secondaries().last().unwrap().clone();
        cluster.delete_replica(&replaced).await?;
        let topology = cluster.settle().await?;
        ensure!(
            cluster.service(&replaced).await.is_err(),
            "retired reference resolved"
        );
        assert_values(&mut cluster, 3, 17).await?;
        let writer = cluster.service(topology.primary()).await?;
        cluster
            .drive(async { Ok::<_, anyhow::Error>(writer.set(23).await?) })
            .await?;
        assert_values(&mut cluster, 3, 23).await?;
        Ok::<_, anyhow::Error>(())
    }
    .await;
    finish(cluster, result).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stopped_primary_failover_preserves_receipts_and_fences_the_old_service() -> Result<()> {
    let mut cluster = cluster().await?;
    let result = async {
        let topology = cluster.settle().await?;
        let old = topology.primary().clone();
        let old_service = cluster.service(&old).await?;
        cluster
            .drive(async { Ok::<_, anyhow::Error>(old_service.set(31).await?) })
            .await?;
        eprintln!("stopped-primary: write receipt observed");
        cluster.stop(&old).await?;
        eprintln!("stopped-primary: local stop drained");
        let topology = cluster.settle_members(2).await?;
        eprintln!("stopped-primary: survivor topology settled");
        let writer = cluster.service(topology.primary()).await?;
        ensure!(writer.read().await? == 31);
        ensure!(old_service.set(99).await.is_err());
        cluster
            .drive(async { Ok::<_, anyhow::Error>(writer.set(37).await?) })
            .await?;
        assert_values(&mut cluster, 2, 37).await?;
        Ok::<_, anyhow::Error>(())
    }
    .await;
    finish(cluster, result).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_live_isolation_and_group_quorum_loss_restore_the_application() -> Result<()> {
    let mut cluster = cluster().await?;
    let result = async {
        let topology = cluster.settle().await?;
        let old = topology.primary().clone();
        let old_service = cluster.service(&old).await?;
        cluster
            .drive(async { Ok::<_, anyhow::Error>(old_service.set(41).await?) })
            .await?;
        let isolation = cluster.isolate(&[old]).await?;
        let survivors = cluster.settle_survivors(&isolation, 2).await?;
        let writer = cluster.service(survivors.primary()).await?;
        cluster
            .drive(async { Ok::<_, anyhow::Error>(writer.set(47).await?) })
            .await?;
        ensure!(
            cluster
                .probe(async { Ok::<_, anyhow::Error>(old_service.set(99).await?) })
                .await
                .is_err()
        );
        cluster.heal(isolation).await?;
        let topology = cluster.settle().await?;
        ensure!(old_service.set(99).await.is_err());
        assert_values(&mut cluster, 3, 47).await?;

        let writer = cluster.service(topology.primary()).await?;
        let isolated = topology.secondaries().cloned().collect::<Vec<_>>();
        let isolation = cluster.isolate(&isolated).await?;
        ensure!(
            cluster
                .probe(async { Ok::<_, anyhow::Error>(writer.set(53).await?) })
                .await
                .is_err()
        );
        ensure!(
            cluster.isolate(&isolated).await.is_err(),
            "second fault was accepted"
        );
        cluster.heal(isolation).await?;
        let topology = cluster.settle().await?;
        let writer = cluster.service(topology.primary()).await?;
        let recovered = writer.read().await?;
        ensure!(
            recovered == 47 || recovered == 53,
            "unknown ambiguous outcome"
        );
        assert_values(&mut cluster, 3, recovered).await?;
        cluster
            .drive(async { Ok::<_, anyhow::Error>(writer.set(59).await?) })
            .await?;
        assert_values(&mut cluster, 3, 59).await?;
        Ok::<_, anyhow::Error>(())
    }
    .await;
    finish(cluster, result).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn planned_transfer_uses_the_real_spec_and_preserves_application_data() -> Result<()> {
    let mut cluster = cluster().await?;
    let result = async {
        let topology = cluster.settle().await?;
        let writer = cluster.service(topology.primary()).await?;
        cluster
            .drive(async { Ok::<_, anyhow::Error>(writer.set(61).await?) })
            .await?;
        let target = topology.secondaries().next().unwrap().clone();
        cluster.switchover(&target).await?;
        let topology = cluster.settle().await?;
        ensure!(topology.primary() == &target);
        let promoted = cluster.service(topology.primary()).await?;
        ensure!(promoted.read().await? == 61);
        ensure!(writer.set(99).await.is_err());
        cluster
            .drive(async { Ok::<_, anyhow::Error>(promoted.set(67).await?) })
            .await?;
        assert_values(&mut cluster, 3, 67).await?;
        Ok::<_, anyhow::Error>(())
    }
    .await;
    finish(cluster, result).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn application_error_is_returned_and_does_not_skip_owned_cleanup() -> Result<()> {
    let mut cluster = cluster().await?;
    cluster.settle().await?;
    let error = cluster
        .drive(async { Err::<(), _>(anyhow::anyhow!("intentional downstream application error")) })
        .await
        .expect_err("application error was hidden");
    ensure!(
        error.sources().any(|source| source
            .to_string()
            .contains("intentional downstream application error")),
        "original application error was not preserved"
    );
    finish(cluster, Ok(())).await
}
