use anyhow::{Context, Result, ensure};
use kuberic_controller::test_util::{Barrier, InstructionPosition};
use kuberic_runtime::protocol::public_operations::{
    PublicV1CallKind, ReplicaGoalKind, ReplicaGoalStatus, ReplicaPublicationStage,
};
use kuberic_runtime::protocol::types::AccessStatus;

use super::harness::Harness;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn m3_target_application_open_selected_image_recovers_retained_data() -> Result<()> {
    target_open_image("ApplicationOpen", InstructionPosition::Selected).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn m3_target_application_open_effect_image_recovers_retained_data() -> Result<()> {
    target_open_image("ApplicationOpen", InstructionPosition::Effect).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn m3_target_application_open_receipt_image_recovers_retained_data() -> Result<()> {
    target_open_image("ApplicationOpen", InstructionPosition::Receipt).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn m3_target_replicator_open_selected_image_recovers_retained_data() -> Result<()> {
    target_open_image("ReplicatorOpen", InstructionPosition::Selected).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn m3_target_replicator_open_effect_image_recovers_retained_data() -> Result<()> {
    target_open_image("ReplicatorOpen", InstructionPosition::Effect).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn m3_target_replicator_open_receipt_image_recovers_retained_data() -> Result<()> {
    target_open_image("ReplicatorOpen", InstructionPosition::Receipt).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn m3_repeated_target_open_recovery_keeps_the_same_live_source() -> Result<()> {
    target_open_image_rounds("ApplicationOpen", InstructionPosition::Selected, 2).await
}

async fn target_open_image(instruction: &str, position: InstructionPosition) -> Result<()> {
    target_open_image_rounds(instruction, position, 1).await
}

async fn target_open_image_rounds(
    instruction: &str,
    position: InstructionPosition,
    rounds: usize,
) -> Result<()> {
    let mut harness = Harness::with_members(3).await?;
    let result = async {
        let primary_id = harness.ready_members(3).await?;
        let primary = harness.reference(primary_id).await?;
        let source = harness.cluster.service(&primary).await?;
        let mut target_id = None;
        let mut original_results = Vec::new();
        for round in 0..rounds {
            let key = format!("target-open-image-{round}");
            harness
                .cluster
                .probe(source.replicate_put(key.clone(), "acknowledged-before-open-image".into()))
                .await?;
            harness
                .assert_values_on_members(&key, "acknowledged-before-open-image", 3)
                .await?;
            let original = harness.cluster.observe().await?;
            let old_configuration = original
                .snapshot(&primary)
                .unwrap()
                .publication
                .as_ref()
                .unwrap()
                .scope
                .configuration
                .current
                .clone();
            let target = original
                .replicas()
                .find(|reference| {
                    reference.replica_id() != primary_id
                        && target_id.is_none_or(|id| reference.replica_id() == id)
                })
                .context("retained target missing")?
                .clone();
            target_id = Some(target.replica_id());
            let app_root = harness.cluster.application_root(&target).await?;
            harness.cluster.stop(&target).await?;
            let reopening = harness.cluster.request_restart(&target).await?;
            let old_service = harness.cluster.service(&reopening).await?;
            let intent = harness
                .cluster
                .pause_at(Barrier::intent(PublicV1CallKind::Open))
                .await?;
            let discover = intent
                .evidence()
                .goal()
                .context("actual target Discover missing")?
                .goal
                .clone();
            ensure!(matches!(discover.desired, ReplicaGoalKind::Discover));
            ensure!(discover.scope.process_session_id == *reopening.process_session_id());
            let cut = harness
                .cluster
                .arm_instruction(&reopening, &discover, instruction, position)
                .await?;
            harness.cluster.resume(intent).await?;
            let entered = harness.cluster.enter_instruction(&cut).await?;
            ensure!(entered.position == position);
            ensure!(
                entered.receipts
                    == entered.index + usize::from(position == InstructionPosition::Receipt)
            );
            ensure!(if position == InstructionPosition::Receipt {
                entered.selected_call.is_none()
            } else {
                entered.selected_call.as_deref() == Some(instruction)
            });
            let image = harness.cluster.capture_instruction_image(&cut).await?;
            ensure!(image.evidence().goal == discover);
            eprintln!(
                "C06 actual retained target Open boundary: {:?}",
                image.evidence()
            );
            cut.release();
            let completion = harness
                .cluster
                .pause_at(Barrier::completion(PublicV1CallKind::Open))
                .await?;
            ensure!(
                completion
                    .evidence()
                    .goal()
                    .is_some_and(|entry| entry.goal == discover)
            );
            harness.cluster.resume(completion).await?;
            let fresh = harness.cluster.reopen_instruction_image(image).await?;
            ensure!(fresh.storage_incarnation() == target.storage_incarnation());
            ensure!(fresh.process_session_id() != reopening.process_session_id());
            ensure!(fresh.replicator_creation_id() != reopening.replicator_creation_id());
            ensure!(harness.cluster.application_root(&fresh).await? == app_root);
            ensure!(old_service.get(&key).await.is_err());
            let initial = harness.cluster.observe().await?;
            let snapshot = initial
                .snapshot(&fresh)
                .context("reopened target snapshot missing")?;
            ensure!(snapshot.publication.is_none());
            ensure!(
                snapshot.read_status != AccessStatus::Granted
                    && snapshot.write_status != AccessStatus::Granted
            );
            let old_result = snapshot
                .goals
                .iter()
                .find(|entry| entry.goal == discover)
                .context("captured target Discover result missing")?
                .clone();
            ensure!(matches!(old_result.status, ReplicaGoalStatus::Uncertain(_)));
            loop {
                harness.cluster.step().await?;
                let observation = harness.cluster.observe().await?;
                let current = observation
                    .snapshot(&fresh)
                    .context("target recovery disappeared")?;
                if let Some(failed) = current.goals.iter().find(|entry| {
                    entry.goal.scope.process_session_id == *fresh.process_session_id()
                        && matches!(
                            entry.status,
                            ReplicaGoalStatus::Failed(_) | ReplicaGoalStatus::Uncertain(_)
                        )
                }) {
                    anyhow::bail!("C06 actual retained target recovery failed: {failed:?}");
                }
                if current.publication.as_ref().is_some_and(|publication| {
                    publication.stage == ReplicaPublicationStage::AchievedScope
                        && publication.role
                            == kuberic_runtime::protocol::types::ReplicaRole::ActiveSecondary
                }) {
                    break;
                }
            }
            loop {
                harness.cluster.step().await?;
                let observation = harness.cluster.observe().await?;
                for (reference, snapshot) in observation.snapshots() {
                    if let Some(failed) = snapshot.goals.iter().find(|entry| {
                        entry.goal.scope.process_session_id == *reference.process_session_id()
                            && matches!(
                                entry.status,
                                ReplicaGoalStatus::Failed(_) | ReplicaGoalStatus::Uncertain(_)
                            )
                    }) {
                        anyhow::bail!(
                            "C06 actual native convergence failed at member {}: {failed:?}",
                            reference.replica_id()
                        );
                    }
                }
                if observation
                    .set()
                    .status
                    .as_ref()
                    .and_then(|status| status.public_v1_topology.as_ref())
                    .is_some_and(|topology| {
                        topology.epoch > old_configuration.epoch
                            && topology.primary_id == old_configuration.primary_id
                            && topology.sessions.iter().any(|session| {
                                session.process_session_id == *fresh.process_session_id()
                            })
                    })
                {
                    break;
                }
            }
            harness.ready_members(3).await?;
            harness
                .assert_values_on_members(&key, "acknowledged-before-open-image", 3)
                .await?;
            let ready = harness.cluster.observe().await?;
            ensure!(
                ready.snapshot(&primary).is_some_and(|snapshot| {
                    snapshot.process_session_id == *primary.process_session_id()
                        && snapshot.creation_id == *primary.replicator_creation_id()
                        && snapshot.publication.as_ref().is_some_and(|publication| {
                            publication.role
                                == kuberic_runtime::protocol::types::ReplicaRole::Primary
                        })
                }),
                "retained target recovery restarted or replaced the live source"
            );
            let source = harness
                .cluster
                .service(
                    ready
                        .replicas()
                        .find(|reference| {
                            ready
                                .snapshot(reference)
                                .and_then(|snapshot| snapshot.publication.as_ref())
                                .is_some_and(|publication| {
                                    publication.role
                                        == kuberic_runtime::protocol::types::ReplicaRole::Primary
                                })
                        })
                        .context("qualified primary missing after target Open recovery")?,
                )
                .await?;
            harness
                .cluster
                .probe(source.replicate_put(key.clone(), "acknowledged-after-open-image".into()))
                .await?;
            harness
                .assert_values_on_members(&key, "acknowledged-after-open-image", 3)
                .await?;
            let final_view = harness.cluster.observe().await?;
            let final_target = final_view
                .snapshot(&fresh)
                .context("fresh target missing at service tail")?;
            ensure!(final_target.goals.iter().any(|entry| entry == &old_result));
            original_results.push(old_result);
            for original in &original_results {
                ensure!(final_target.goals.iter().any(|entry| entry == original));
            }
            ensure!(
                final_target
                    .publication
                    .as_ref()
                    .unwrap()
                    .scope
                    .configuration
                    .current
                    .write_quorum
                    == old_configuration.write_quorum
            );
            for preserved in 0..=round {
                harness
                    .assert_values_on_members(
                        &format!("target-open-image-{preserved}"),
                        "acknowledged-after-open-image",
                        3,
                    )
                    .await?;
            }
        }
        Ok::<_, anyhow::Error>(())
    }
    .await;
    harness.finish(result).await
}

/// Source metadata image at actual Build selection, followed by separate real
/// old-owner joins. This is not an abrupt OS crash or power-loss claim.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn m3_source_build_selected_image_recovers_retained_data() -> Result<()> {
    source_build_image(InstructionPosition::Selected).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn m3_source_build_effect_gap_image_recovers_retained_data() -> Result<()> {
    source_build_image(InstructionPosition::Effect).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn m3_source_build_receipt_gap_image_recovers_retained_data() -> Result<()> {
    source_build_image(InstructionPosition::Receipt).await
}

async fn source_build_image(position: InstructionPosition) -> Result<()> {
    source_build_result_image(position, None).await
}

pub(super) async fn source_build_result_image(
    position: InstructionPosition,
    failure: Option<kuberic_runtime::testing::PublicBuildFailure>,
) -> Result<()> {
    source_build_reopen_image(position, failure, false).await
}

pub(super) async fn source_build_target_reopen_image(position: InstructionPosition) -> Result<()> {
    source_build_reopen_image(position, None, true).await
}

async fn source_build_reopen_image(
    position: InstructionPosition,
    failure: Option<kuberic_runtime::testing::PublicBuildFailure>,
    reopen_target: bool,
) -> Result<()> {
    let mut harness = Harness::with_members(1).await?;
    let result = async {
        let source_id = harness.ready_members(1).await?;
        let source_ref = harness.reference(source_id).await?;
        let old_service = harness.cluster.service(&source_ref).await?;
        let baseline = harness
            .cluster
            .probe(
                old_service
                    .replicate_put("source-image".into(), "acknowledged-before-image".into()),
            )
            .await?;
        let app_root = harness.cluster.application_root(&source_ref).await?;
        harness.cluster.scale_to(2).await?;
        let intent = harness
            .cluster
            .pause_at(Barrier::intent(PublicV1CallKind::Build))
            .await?;
        let build = intent
            .evidence()
            .goal()
            .context("actual source Build missing")?
            .goal
            .clone();
        let remote = match &build.desired {
            ReplicaGoalKind::Build { remote } => remote.clone(),
            _ => anyhow::bail!("wrong canonical goal at source image"),
        };
        let target = harness
            .cluster
            .observe()
            .await?
            .replicas()
            .find(|reference| {
                reference.identity() == &remote.target.scope.storage.replica
                    && reference.process_session_id() == &remote.target.scope.process_session_id
                    && reference.replicator_creation_id() == &remote.target.scope.creation_id
            })
            .context("actual image receiver missing")?
            .clone();
        let cut = if let Some(failure) = failure {
            harness.cluster.arm_build_failure(&source_ref, &build, failure, position).await?
        } else {
            harness.cluster.arm_instruction(&source_ref, &build, "Build", position).await?
        };
        harness.cluster.resume(intent).await?;
        let entered = harness.cluster.enter_instruction(&cut).await?;
        ensure!(entered.position == position);
        let receipted = position == InstructionPosition::Receipt
            || (position == InstructionPosition::FailureRecorded
                && failure == Some(kuberic_runtime::testing::PublicBuildFailure::TypedRemote));
        ensure!(
            entered.receipts == entered.index + usize::from(receipted)
        );
        ensure!(if receipted {
            entered.selected_call.is_none()
        } else {
            entered.selected_call.as_deref() == Some("Build")
        });
        let image = harness.cluster.capture_instruction_image(&cut).await?;
        ensure!(image.evidence().goal == build);
        eprintln!("actual C03/C04/C05/C14 source boundary failure={failure:?}: {:?}", image.evidence());
        let mut cut = Some(cut);
        let fresh_receiver = if reopen_target {
            let acceptance = position == InstructionPosition::Receipt;
            if acceptance {
                cut.take().unwrap().release();
                loop {
                    let observed = harness.cluster.observe().await?;
                    if let Some(ready) = observed.snapshot(&source_ref).unwrap()
                        .remote_operations.iter().find(|entry| entry.operation_id == build.operation_id
                            && entry.state == kuberic_runtime::protocol::public_operations::RemoteReplicaState::Ready
                            && !entry.accepted)
                    {
                        eprintln!("C12 actual completed/unaccepted Ready before target reopen: {ready:?}");
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            }
            let terminal = harness.cluster.retain_ordinary_terminal(
                &target, PublicV1CallKind::Close
            ).await?;
            if acceptance {
                harness.cluster.submit_ordinary_goal(&target, &terminal).await?;
            }
            loop {
                if !acceptance {
                    harness.cluster.step().await?;
                }
                let observed = harness.cluster.observe().await?;
                if observed.snapshot(&target).unwrap().goals.iter().any(|entry| {
                    entry.goal == terminal && entry.status == ReplicaGoalStatus::Achieved
                        && (acceptance || entry.host_settled)
                }) {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            if acceptance {
                harness.cluster.acknowledge_retained_goal(&target, &terminal).await?;
            }
            harness.cluster.stop(&target).await?;
            let fresh_target = harness.cluster.request_restart(&target).await?;
            let observed = harness.cluster.observe().await?;
            let snapshot = observed.snapshot(&fresh_target).context("fresh receiver missing")?;
            ensure!(snapshot.publication.is_none()
                && snapshot.write_status != AccessStatus::Granted
                && snapshot.read_status != AccessStatus::Granted);
            ensure!(harness.cluster.submit_ordinary_goal(&source_ref, &build).await.is_err(),
                "reopened receiver admitted the old Ready/Build selection");
            if acceptance {
                ensure!(harness.cluster.acknowledge_retained_goal(&source_ref, &build).await.is_err(),
                    "actual acknowledgement accepted old Ready after target reopen");
            }
            eprintln!("C12 actual target reopen at {position:?}: old_target={} fresh_target={} joined_terminal={terminal:?}",
                target.process_session_id(), fresh_target.process_session_id());
            Some(fresh_target)
        } else {
            None
        };
        if let Some(cut) = cut {
            cut.release();
        }
        // The metadata image keeps its earlier gap. The real old process must
        // still finish/settle its actual work before another provider opens.
        let completion = harness
            .cluster
            .pause_at(Barrier::completion(PublicV1CallKind::Build))
            .await?;
        ensure!(
            completion
                .evidence()
                .goal()
                .is_some_and(|entry| entry.goal == build)
        );
        harness.cluster.resume(completion).await?;
        if failure.is_some() {
            let failed = harness.cluster.observe().await?;
            let failed = failed.snapshot(&source_ref).context("failed Build source missing")?;
            ensure!(failed.goals.iter().any(|entry| entry.goal == build
                && matches!(entry.status, ReplicaGoalStatus::Failed(_))));
            ensure!(failed.remote_operations.iter().all(|entry| {
                entry.operation_id != build.operation_id || entry.state
                    != kuberic_runtime::protocol::public_operations::RemoteReplicaState::Ready
            }));
        }
        let fresh = harness.cluster.reopen_instruction_image(image).await?;
        ensure!(fresh.identity() == source_ref.identity());
        ensure!(fresh.storage_incarnation() == source_ref.storage_incarnation());
        ensure!(fresh.process_session_id() != source_ref.process_session_id());
        ensure!(fresh.replicator_creation_id() != source_ref.replicator_creation_id());
        ensure!(harness.cluster.application_root(&fresh).await? == app_root);
        ensure!(
            old_service
                .replicate_put("late-old-source".into(), "denied".into())
                .await
                .is_err()
        );
        let opened = harness.cluster.observe().await?;
        let snapshot = opened
            .snapshot(&fresh)
            .context("fresh source image snapshot missing")?;
        ensure!(
            snapshot.goals.iter().any(|entry| {
                entry.goal == build && matches!(entry.status, ReplicaGoalStatus::Uncertain(_))
            }),
            "source image replayed or invented a successful old Build"
        );
        let old_result = snapshot
            .goals
            .iter()
            .find(|entry| entry.goal == build)
            .unwrap()
            .clone();
        let reconstruction = loop {
            harness.cluster.step().await?;
            let observation = harness.cluster.observe().await?;
            for (_, snapshot) in observation.snapshots() {
                if let Some(failed) = snapshot.goals.iter().find(|entry| {
                    matches!(
                        entry.goal.desired,
                        ReplicaGoalKind::RecoverContainedCreation { .. } | ReplicaGoalKind::Close
                    ) && matches!(
                        entry.status,
                        ReplicaGoalStatus::Failed(_) | ReplicaGoalStatus::Uncertain(_)
                    )
                }) {
                    anyhow::bail!("C03 actual recovery/receiver failure: {failed:?}");
                }
            }
            let status = observation.set().status.as_ref().unwrap();
            if let Some(entry) = status.replica_goals.iter().find(|entry| {
                entry.goal.scope.process_session_id == *fresh.process_session_id()
                    && matches!(
                        entry.goal.desired,
                        ReplicaGoalKind::RecoverContainedCreation { .. }
                    )
                    && entry.outcome.as_ref().is_some_and(|outcome| {
                        outcome.status == ReplicaGoalStatus::Achieved && outcome.host_settled
                    })
            }) && status
                .public_v1_topology
                .as_ref()
                .unwrap()
                .sessions
                .iter()
                .any(|session| session.process_session_id == *fresh.process_session_id())
            {
                break entry.goal.clone();
            }
        };
        let sampled = harness.cluster.sample_replica(&fresh).await?;
        ensure!(sampled.publication.as_ref().is_some_and(|publication| {
            publication.stage == ReplicaPublicationStage::AchievedScope
                && publication.scope == reconstruction.scope
        }));
        ensure!(
            sampled
                .progress
                .value()
                .is_some_and(|value| value >= baseline)
        );
        ensure!(
            sampled.read_status == AccessStatus::Granted
                && sampled.write_status == AccessStatus::Granted
        );
        ensure!(
            sampled.goals.iter().any(|entry| entry == &old_result),
            "original uncertain result changed"
        );
        let source = harness.cluster.service(&fresh).await?;
        ensure!(source.get("source-image").await?.as_deref() == Some("acknowledged-before-image"));
        // Receiver settlement is a separate actual terminal/root join. Only
        // then reopen its same data root and obtain a fresh generation/Ready.
        let fresh_target = if let Some(fresh) = fresh_receiver {
            fresh
        } else {
            harness.cluster.stop(&target).await?;
            harness.cluster.request_restart(&target).await?
        };
        ensure!(fresh_target.storage_incarnation() == target.storage_incarnation());
        ensure!(fresh_target.process_session_id() != target.process_session_id());
        harness.ready_members(2).await?;
        harness
            .assert_values_on_members("source-image", "acknowledged-before-image", 2)
            .await?;
        harness
            .cluster
            .probe(source.replicate_put("source-image".into(), "acknowledged-after-image".into()))
            .await?;
        harness
            .assert_values_on_members("source-image", "acknowledged-after-image", 2)
            .await?;
        let final_view = harness.cluster.observe().await?;
        ensure!(
            final_view
                .snapshot(&fresh)
                .unwrap()
                .goals
                .iter()
                .any(|entry| entry == &old_result)
        );
        ensure!(old_service.get("source-image").await.is_err());
        eprintln!("C14/C03-C05/C12 CLOSED target_reopen={reopen_target} failure={failure:?} cut={position:?} baseline={baseline} fresh_source={} fresh_target={} old_result={old_result:?}",
            fresh.process_session_id(), fresh_target.process_session_id());
        Ok::<_, anyhow::Error>(())
    }
    .await;
    harness.finish(result).await
}
