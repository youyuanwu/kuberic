use anyhow::{Context, Result, ensure};
use kuberic_controller::test_util::{ReplicaRef, TestCluster};
use kuberic_runtime::application::OpenMode;
use kuberic_runtime::protocol::types::AccessStatus;

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
    eprintln!("counter values: requesting settled members={members}, value={expected}");
    let topology = cluster.settle_members(members).await?;
    eprintln!("counter values: topology settled for value={expected}");
    let mut services = Vec::new();
    for replica in topology.members() {
        services.push((
            cluster.service(replica).await?,
            replica == topology.primary(),
        ));
    }
    cluster
        .drive(async {
            let mut last = Vec::new();
            loop {
                let mut matches = 0;
                let mut observed = Vec::new();
                for (service, primary) in &services {
                    if *primary {
                        ensure!(service.read().await? == service.committed_value()?);
                    } else {
                        ensure!(matches!(
                            service.read().await,
                            Err(kuberic_runtime::RuntimeError::ReadClosed(
                                AccessStatus::NotPrimary
                            ))
                        ));
                    }
                    let value = service.committed_value()?;
                    observed.push(value);
                    if value == expected {
                        matches += 1;
                    }
                }
                if matches == services.len() {
                    eprintln!("counter values: matched value={expected}");
                    return Ok::<_, anyhow::Error>(());
                }
                if observed != last {
                    eprintln!("counter values: waiting for {expected}, actual={observed:?}");
                    last = observed;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await?;
    eprintln!("counter values: drive completed value={expected}");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn own_application_bootstraps_restarts_and_replaces_with_acknowledged_data() -> Result<()> {
    let mut cluster = cluster().await?;
    let result = async {
        let topology = cluster.settle().await?;
        for replica in topology.members() {
            let service = cluster.service(replica).await?;
            ensure!(service.open_modes() == [OpenMode::New]);
        }
        let writer = cluster.service(topology.primary()).await?;
        cluster
            .drive(async { Ok::<_, anyhow::Error>(writer.set(17).await?) })
            .await?;
        let secondary = topology.secondaries().next().unwrap().clone();
        let reopened = cluster
            .restart(&secondary)
            .await
            .context("restart secondary after acknowledged counter write")?;
        let service = cluster.service(&reopened).await?;
        ensure!(service.open_modes() == [OpenMode::Existing]);
        ensure!(service.committed_value()? == 17);
        ensure!(matches!(
            service.read().await,
            Err(kuberic_runtime::RuntimeError::ReadClosed(
                AccessStatus::NotPrimary
            ))
        ));
        ensure!(
            cluster.service(&secondary).await.is_err(),
            "old session reference resolved"
        );

        let topology = cluster.settle().await?;
        let replaced = topology.secondaries().last().unwrap().clone();
        cluster.delete_replica(&replaced).await?;
        let topology = cluster.settle().await?;
        let replacement = topology
            .members()
            .find(|replica| replica.identity().replica_id == replaced.identity().replica_id)
            .unwrap();
        ensure!(cluster.service(replacement).await?.open_modes() == [OpenMode::New]);
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
async fn m2_secondary_gap_preserves_ordinary_membership_and_healthy_quorum() -> Result<()> {
    let mut cluster = cluster().await?;
    let result = secondary_gap_prefix(&mut cluster).await.map(|_| ());
    finish(cluster, result).await
}

async fn secondary_gap_prefix(
    cluster: &mut TestCluster<CounterFactory>,
) -> Result<(ReplicaRef, ReplicaRef)> {
    let topology = cluster.settle().await?;
    let writer = cluster.service(topology.primary()).await?;
    cluster
        .probe(async { Ok::<_, anyhow::Error>(writer.set(83).await?) })
        .await?;
    cluster.settle().await?;
    let before = cluster.observe().await?;
    let goals = before
        .set()
        .status
        .as_ref()
        .context("settled ordinary status missing")?
        .replica_goals
        .clone();
    let configuration = before
        .set()
        .status
        .as_ref()
        .and_then(|status| status.public_v1_topology.clone())
        .context("settled topology missing")?;
    let secondary = topology.secondaries().next().unwrap().clone();
    let isolation = cluster.isolate(std::slice::from_ref(&secondary)).await?;
    for _ in 0..6 {
        cluster.step().await?;
    }
    let during = cluster.observe().await?;
    ensure!(
        during
            .set()
            .status
            .as_ref()
            .and_then(|status| status.public_v1_topology.as_ref())
            == Some(&configuration),
        "a single-secondary transport outage changed admitted membership"
    );
    let current = &during
        .set()
        .status
        .as_ref()
        .context("current ordinary status missing")?
        .replica_goals;
    ensure!(
        *current == goals,
        "secondary outage changed ordinary authority"
    );
    ensure!(during.snapshot(&secondary).is_none());
    ensure!(
        during
            .snapshot(topology.primary())
            .context("healthy primary snapshot missing")?
            .write_status
            == AccessStatus::Granted,
        "a single-secondary transport outage fenced the healthy primary"
    );
    cluster
        .probe(async {
            Ok::<_, anyhow::Error>(
                tokio::time::timeout(std::time::Duration::from_secs(10), writer.set(89))
                    .await
                    .context("healthy quorum write during one-secondary gap")??,
            )
        })
        .await?;
    ensure!(writer.read().await? == 89);
    for member in topology.members().filter(|member| *member != &secondary) {
        ensure!(cluster.service(member).await?.committed_value()? == 89);
    }
    cluster.heal(isolation).await?;
    eprintln!(
        "M2 downstream secondary gap: actual bootstrap/copy, unchanged ordinary membership, unavailable secondary observation and healthy quorum write/read passed"
    );
    Ok((topology.primary().clone(), secondary))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn secondary_restart_gap_preserves_admitted_topology_and_primary_writes() -> Result<()> {
    let mut cluster = cluster().await?;
    let result = async {
        let (primary, secondary) = secondary_gap_prefix(&mut cluster).await?;
        eprintln!("downstream gap: requesting automatic same-storage secondary restart");
        let reopened = cluster
            .restart(&secondary)
            .await
            .context("restart after the RPC gap")?;
        ensure!(reopened != secondary);
        ensure!(cluster.service(&reopened).await?.open_modes() == [OpenMode::Existing]);
        let restored = cluster
            .settle()
            .await
            .context("settle the renewed secondary")?;
        ensure!(restored.primary() == &primary);
        assert_values(&mut cluster, 3, 89)
            .await
            .context("acknowledged data after secondary renewal")?;
        Ok::<_, anyhow::Error>(())
    }
    .await;
    finish(cluster, result).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repeated_secondary_replacement_preserves_acknowledged_data() -> Result<()> {
    let mut cluster = cluster().await?;
    let result = async {
        let topology = cluster.settle().await?;
        let writer = cluster.service(topology.primary()).await?;
        cluster
            .drive(async { Ok::<_, anyhow::Error>(writer.set(71).await?) })
            .await?;
        let topology = cluster.settle().await?;
        let replica_id = topology.secondaries().next().unwrap().identity().replica_id;
        for _ in 0..2 {
            let topology = cluster.settle().await?;
            let replaced = topology
                .secondaries()
                .find(|replica| replica.identity().replica_id == replica_id)
                .unwrap()
                .clone();
            cluster.delete_replica(&replaced).await?;
            cluster.settle().await?;
            let observation = cluster.observe().await?;
            let replacement = observation
                .replicas()
                .find(|replica| replica.identity().replica_id == replica_id)
                .unwrap();
            ensure!(cluster.service(replacement).await?.open_modes() == [OpenMode::New]);
            ensure!(
                cluster.service(&replaced).await.is_err(),
                "retired reference resolved"
            );
            assert_values(&mut cluster, 3, 71).await?;
        }
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
    let started = std::time::Instant::now();
    let stage = |name| eprintln!("counter combined {:?}: {name}", started.elapsed());
    let result = async {
        let topology = cluster.settle().await?;
        stage("bootstrap settled");
        let old = topology.primary().clone();
        let old_service = cluster.service(&old).await?;
        cluster
            .drive(async { Ok::<_, anyhow::Error>(old_service.set(41).await?) })
            .await?;
        let isolation = cluster.isolate(&[old]).await?;
        stage("Primary isolation acknowledged");
        let survivors = cluster.settle_survivors(&isolation, 2).await?;
        stage("survivor election settled");
        let writer = cluster.service(survivors.primary()).await?;
        cluster
            .drive(async { Ok::<_, anyhow::Error>(writer.set(47).await?) })
            .await?;
        ensure!(
            cluster
                .probe(async {
                    // Native quorum work may remain pending after the client observer times out.
                    Ok::<_, anyhow::Error>(
                        tokio::time::timeout(
                            std::time::Duration::from_millis(250),
                            old_service.set(99),
                        )
                        .await??,
                    )
                })
                .await
                .is_err()
        );
        cluster.heal(isolation).await?;
        stage("Primary isolation healed");
        let topology = cluster.settle().await?;
        stage("old Primary retired/rejoined");
        ensure!(old_service.set(99).await.is_err());
        assert_values(&mut cluster, 3, 47).await?;

        let writer = cluster.service(topology.primary()).await?;
        let isolated = topology.secondaries().cloned().collect::<Vec<_>>();
        let isolation = cluster.isolate(&isolated).await?;
        stage("group isolation acknowledged");
        ensure!(
            cluster
                .probe(async {
                    Ok::<_, anyhow::Error>(
                        tokio::time::timeout(std::time::Duration::from_millis(250), writer.set(53))
                            .await??,
                    )
                })
                .await
                .is_err()
        );
        ensure!(
            cluster.isolate(&isolated).await.is_err(),
            "second fault was accepted"
        );
        cluster.heal(isolation).await?;
        stage("group isolation healed");
        let topology = cluster.settle().await?;
        stage("group recovery settled");
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
        stage("post-recovery write acknowledged");
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
