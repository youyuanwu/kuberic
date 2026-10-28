use anyhow::{Context, Result, ensure};
use kuberic_controller::test_util::{
    ApplicationFactory, Barrier, CreatedApplication, InstructionPosition, ReplicaContext,
    TestCluster,
};
use kuberic_runtime::protocol::public_operations::{
    PublicV1CallKind, ReplicaGoalKind, ReplicaGoalStatus,
};

#[path = "../../../scripts/fixtures/controller-test-util/src/lib.rs"]
mod application;

struct InlineCounterFactory;

#[async_trait::async_trait]
impl ApplicationFactory for InlineCounterFactory {
    type Service = application::CounterService;

    async fn create(
        &self,
        context: ReplicaContext,
    ) -> kuberic_controller::test_util::Result<CreatedApplication<Self::Service>> {
        ApplicationFactory::create(&application::CounterFactory, context).await
    }

    async fn shutdown_and_drain(
        &self,
        service: std::sync::Arc<Self::Service>,
    ) -> kuberic_controller::test_util::Result<()> {
        ApplicationFactory::shutdown_and_drain(&application::CounterFactory, service).await
    }
}

async fn counter_values(
    cluster: &mut TestCluster<InlineCounterFactory>,
    services: &[std::sync::Arc<application::CounterService>],
    expected: i64,
) -> Result<()> {
    cluster
        .probe(async {
            loop {
                let mut all = true;
                for service in services {
                    if service.committed_value()? != expected {
                        all = false;
                    }
                }
                if all {
                    return Ok::<(), kuberic_runtime::RuntimeError>(());
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn m3_counter_contained_reconstruction_consumes_native_stream_without_copy() -> Result<()> {
    let mut cluster = TestCluster::builder(InlineCounterFactory)
        .image("in-process/counter")
        .initial_replicas(3)
        .start()
        .await?;
    let result: Result<()> =
        async {
            let original = cluster.settle_members(3).await?;
            let source_ref = original.primary().clone();
            let source = cluster.service(&source_ref).await?;
            cluster.probe(source.set(17)).await?;
            ensure!(source.read().await? == 17);
            let mut services = Vec::new();
            for member in original.members() {
                services.push(cluster.service(member).await?);
            }
            counter_values(&mut cluster, &services, 17).await?;
            let before = cluster.observe().await?;
            let configuration = before
                .snapshot(&source_ref)
                .unwrap()
                .publication
                .as_ref()
                .unwrap()
                .scope
                .configuration
                .current
                .clone();
            let target = original
                .secondaries()
                .next()
                .context("counter retained target missing")?
                .clone();
            let root = cluster.application_root(&target).await?;
            cluster.stop(&target).await?;
            let reopening = cluster.request_restart(&target).await?;
            let intent = cluster
                .pause_at(Barrier::intent(PublicV1CallKind::Open))
                .await?;
            let goal = intent
                .evidence()
                .goal()
                .context("counter Discover missing")?
                .goal
                .clone();
            ensure!(matches!(goal.desired, ReplicaGoalKind::Discover));
            let cut = cluster
                .arm_instruction(
                    &reopening,
                    &goal,
                    "ApplicationOpen",
                    InstructionPosition::Selected,
                )
                .await?;
            cluster.resume(intent).await?;
            let entered = cluster.enter_instruction(&cut).await?;
            ensure!(
                entered.index == 0
                    && entered.receipts == 0
                    && entered.selected_call.as_deref() == Some("ApplicationOpen")
            );
            let image = cluster.capture_instruction_image(&cut).await?;
            cut.release();
            let completion = cluster
                .pause_at(Barrier::completion(PublicV1CallKind::Open))
                .await?;
            ensure!(
                completion
                    .evidence()
                    .goal()
                    .is_some_and(|entry| entry.goal == goal)
            );
            cluster.resume(completion).await?;
            let fresh = cluster.reopen_instruction_image(image).await?;
            ensure!(fresh.storage_incarnation() == target.storage_incarnation());
            ensure!(cluster.application_root(&fresh).await? == root);
            loop {
                cluster.step().await?;
                let observation = cluster.observe().await?;
                for (reference, snapshot) in observation.snapshots() {
                    if let Some(failed) = snapshot.goals.iter().find(|entry| {
                        entry.goal.scope.process_session_id == *reference.process_session_id()
                            && matches!(
                                entry.status,
                                ReplicaGoalStatus::Failed(_) | ReplicaGoalStatus::Uncertain(_)
                            )
                    }) {
                        anyhow::bail!("counter reconstruction failed: {failed:?}");
                    }
                }
                if observation
                    .set()
                    .status
                    .as_ref()
                    .and_then(|status| status.public_v1_topology.as_ref())
                    .is_some_and(|topology| {
                        topology.epoch > configuration.epoch
                            && topology.primary_id == configuration.primary_id
                            && topology.sessions.iter().any(|session| {
                                session.process_session_id == *fresh.process_session_id()
                            })
                    })
                {
                    break;
                }
            }
            let established = cluster.settle_members(3).await?;
            ensure!(
                established.primary().process_session_id() == source_ref.process_session_id()
                    && established.primary().replicator_creation_id()
                        == source_ref.replicator_creation_id()
            );
            services.clear();
            for member in established.members() {
                services.push(cluster.service(member).await?);
            }
            counter_values(&mut cluster, &services, 17).await?;
            cluster.probe(source.set(29)).await?;
            ensure!(source.read().await? == 29);
            counter_values(&mut cluster, &services, 29).await?;
            let final_view = cluster.observe().await?;
            let recovered = final_view
                .snapshot(&fresh)
                .context("counter recovered owner missing")?;
            ensure!(recovered.goals.iter().any(|entry| entry.goal == goal
                && matches!(entry.status, ReplicaGoalStatus::Uncertain(_))));
            ensure!(recovered.goals.iter().any(|entry| {
                matches!(
                    entry.goal.desired,
                    ReplicaGoalKind::RecoverContainedCreation { .. }
                ) && entry.status == ReplicaGoalStatus::Achieved
                    && entry.host_settled
            }));
            ensure!(
                recovered
                    .publication
                    .as_ref()
                    .unwrap()
                    .scope
                    .configuration
                    .current
                    .write_quorum
                    == configuration.write_quorum
            );
            ensure!(
                !final_view
                    .snapshots()
                    .flat_map(|(_, snapshot)| &snapshot.goals)
                    .any(
                        |entry| matches!(&entry.goal.desired, ReplicaGoalKind::Build { remote }
                if remote.target.scope.process_session_id == *fresh.process_session_id())
                    ),
                "contained counter initialization invented a fresh copy completion"
            );
            Ok(())
        }
        .await;
    cluster.finish(result).await.map_err(Into::into)
}
