use anyhow::{Context, Result, ensure};
use kuberic_controller::test_util::{Barrier, InstructionPosition};
use kuberic_runtime::protocol::public_operations::{PublicV1CallKind, ReplicaGoalKind};
use kuberic_runtime::protocol::types::ProcessSessionId;

use super::harness::Harness;

/// Mechanism qualification only: no actor recreation is claimed by this test.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn m3_local_instruction_bridge_observes_real_build_selection_effect_and_receipt() -> Result<()>
{
    for position in [
        InstructionPosition::Selected,
        InstructionPosition::Effect,
        InstructionPosition::Receipt,
    ] {
        let mut harness = Harness::with_members(1).await?;
        let result = async {
            let source_id = harness.ready_members(1).await?;
            let reference = harness.reference(source_id).await?;
            let source = harness.cluster.service(&reference).await?;
            harness
                .cluster
                .probe(
                    source
                        .replicate_put("instruction-cut".into(), "acknowledged-before-copy".into()),
                )
                .await?;
            harness.cluster.scale_to(2).await?;
            let pause = harness
                .cluster
                .pause_at(Barrier::intent(PublicV1CallKind::Build))
                .await?;
            let goal = pause
                .evidence()
                .goal()
                .context("actual selected Build missing")?
                .goal
                .clone();
            ensure!(matches!(goal.desired, ReplicaGoalKind::Build { .. }));
            let mut crossed = goal.clone();
            crossed.scope.process_session_id = ProcessSessionId::new("not-the-owned-process");
            ensure!(
                harness
                    .cluster
                    .arm_instruction(&reference, &crossed, "Build", position)
                    .await
                    .is_err()
            );
            ensure!(
                harness
                    .cluster
                    .arm_instruction(&reference, &goal, "NotAnInstruction", position)
                    .await
                    .is_err()
            );
            let first = harness
                .cluster
                .arm_instruction(&reference, &goal, "Build", position)
                .await?;
            ensure!(
                harness
                    .cluster
                    .arm_instruction(&reference, &goal, "Build", position)
                    .await
                    .is_err()
            );
            // Dropping an unentered scheduling hold disarms it; it does not
            // certify a native/root join or cancel the selected canonical work.
            drop(first);
            let cut = harness
                .cluster
                .arm_instruction(&reference, &goal, "Build", position)
                .await?;
            harness.cluster.resume(pause).await?;
            let observed = harness.cluster.enter_instruction(&cut).await?;
            ensure!(observed.goal == goal);
            ensure!(observed.position == position);
            ensure!(observed.recipe.get(observed.index).map(String::as_str) == Some("Build"));
            ensure!(observed.instruction == "Build");
            ensure!(
                observed.receipts
                    == observed.index + usize::from(position == InstructionPosition::Receipt)
            );
            eprintln!("actual actor-local instruction boundary: {observed:?}");
            cut.release();
            harness.ready_members(2).await?;
            harness
                .assert_values_on_members("instruction-cut", "acknowledged-before-copy", 2)
                .await?;
            let writer = harness.cluster.service(&reference).await?;
            harness
                .cluster
                .probe(writer.replicate_put(
                    "instruction-cut".into(),
                    "acknowledged-after-release".into(),
                ))
                .await?;
            harness
                .assert_values_on_members("instruction-cut", "acknowledged-after-release", 2)
                .await?;
            Ok::<_, anyhow::Error>(())
        }
        .await;
        harness.finish(result).await?;
    }
    Ok(())
}
