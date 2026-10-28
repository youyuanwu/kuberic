use anyhow::{Context, Result, ensure};
use kuberic_controller::test_util::{Barrier, BarrierStage, InstructionPosition};
use kuberic_runtime::protocol::public_operations::{
    PublicV1CallKind, RemoteReplicaState, ReplicaGoalKind, ReplicaGoalStatus,
    ReplicaPublicationStage,
};
use kuberic_runtime::protocol::types::OperationId;
use kuberic_runtime::protocol::types::{AccessStatus, ReplicaRole};

use super::harness::Harness;

const CUTS: [InstructionPosition; 3] = [
    InstructionPosition::Selected,
    InstructionPosition::Effect,
    InstructionPosition::Receipt,
];

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn m3_target_reopen_at_build_return_settles_then_rebuilds() -> Result<()> {
    super::recovery_images::source_build_target_reopen_image(InstructionPosition::BuildReturned)
        .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn m3_target_reopen_at_build_acceptance_settles_then_rebuilds() -> Result<()> {
    super::recovery_images::source_build_target_reopen_image(InstructionPosition::Receipt).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn m3_target_reopen_at_new_voter_boundary_settles_then_rebuilds() -> Result<()> {
    let mut harness = Harness::with_members(1).await?;
    let result = async {
        let primary_id = harness.ready_members(1).await?;
        let source = harness.reference(primary_id).await?;
        let writer = harness.cluster.service(&source).await?;
        let prefix = harness.cluster.probe(writer.replicate_put(
            "new-voter-prefix".into(), "acknowledged".into()
        )).await?;
        harness.cluster.scale_to(2).await?;
        let admission = harness.cluster.pause_at(Barrier::Role {
            stage: BarrierStage::Intent,
            role: ReplicaRole::Primary,
            participant: Some(source.clone()),
        }).await?;
        let goal = admission.evidence().goal().context("new-voter goal missing")?.goal.clone();
        ensure!(goal.scope.configuration.previous.as_ref()
            .is_some_and(|pc| pc.members.len() == 1));
        ensure!(goal.scope.configuration.current.members.len() == 2);
        let observed = harness.cluster.observe().await?;
        let target = observed.replicas().find(|reference| reference.replica_id() != primary_id)
            .context("new voter target missing")?.clone();
        let old_build = observed.snapshot(&source).unwrap().goals.iter()
            .find(|entry| matches!(entry.goal.desired, ReplicaGoalKind::Build { .. })
                && entry.status == ReplicaGoalStatus::Achieved && entry.host_settled)
            .context("new-voter Ready was not actually accepted")?.goal.clone();
        let cut = harness.cluster.arm_instruction(
            &source, &goal, "Configuration", InstructionPosition::Selected
        ).await?;
        harness.cluster.resume(admission).await?;
        let evidence = harness.cluster.enter_instruction(&cut).await?;
        let image = harness.cluster.capture_instruction_image(&cut).await?;
        let terminal = harness.cluster.retain_ordinary_terminal(&target, PublicV1CallKind::Close).await?;
        loop {
            harness.cluster.step().await?;
            if harness.cluster.observe().await?.snapshot(&target).unwrap().goals.iter()
                .any(|entry| entry.goal == terminal
                    && entry.status == ReplicaGoalStatus::Achieved && entry.host_settled) {
                break;
            }
        }
        harness.cluster.stop(&target).await?;
        let fresh_target = harness.cluster.request_restart(&target).await?;
        ensure!(fresh_target.storage_incarnation() == target.storage_incarnation());
        ensure!(harness.cluster.submit_ordinary_goal(&source, &old_build).await.is_err());
        eprintln!("C12 actual accepted-Ready/new-voter boundary: {evidence:?}; joined receiver={terminal:?}; fresh_receiver={fresh_target:?}");
        cut.release();
        let completion = harness.cluster.pause_at(Barrier::Role {
            stage: BarrierStage::Completion,
            role: ReplicaRole::Primary,
            participant: Some(source.clone()),
        }).await?;
        ensure!(completion.evidence().goal().is_some_and(|entry| entry.goal == goal));
        let denied = harness.cluster.observe().await?;
        ensure!(denied.snapshot(&source).unwrap().goals.iter().any(|entry| {
            entry.goal == goal
                && matches!(entry.status, ReplicaGoalStatus::Failed(_) | ReplicaGoalStatus::Uncertain(_))
        }), "actual new-voter owner did not refuse the reopened target");
        harness.cluster.resume(completion).await?;
        let fresh_source = harness.cluster.reopen_instruction_image(image).await?;
        ensure!(writer.get("new-voter-prefix").await.is_err());
        let mut checked_crossed_copy = false;
        loop {
            harness.cluster.step().await?;
            let observed = harness.cluster.observe().await?;
            let snapshot = observed.snapshot(&fresh_source).unwrap();
            if let Some(failed) = snapshot.goals.iter().find(|entry| {
                entry.goal.scope.process_session_id == *fresh_source.process_session_id()
                    && matches!(entry.status,
                        ReplicaGoalStatus::Failed(_) | ReplicaGoalStatus::Uncertain(_))
            }) {
                anyhow::bail!("actual new-voter recovery failed with old Ready still denied: {failed:?}");
            }
            let status = observed.set().status.as_ref().unwrap();
            if let Some(selected) = status.replica_goals.iter().find(|entry| {
                entry.goal.scope.process_session_id == *fresh_source.process_session_id()
                    && matches!(entry.goal.desired, ReplicaGoalKind::RecoverContainedCreation { .. })
                    && entry.outcome.is_none()
                    && !snapshot.goals.iter().any(|local| local.goal == entry.goal)
            }) {
                if let ReplicaGoalKind::RecoverContainedCreation { recovery } = &selected.goal.desired
                    && recovery.addition.as_ref().is_some_and(|addition| addition.phase
                        == kuberic_runtime::protocol::public_operations::MembershipAdditionPhase::Bootstrap)
                {
                    for crossed in 0..5 {
                        let mut invalid = selected.goal.clone();
                        let ReplicaGoalKind::RecoverContainedCreation { recovery } = &mut invalid.desired else { unreachable!() };
                        let addition = recovery.addition.as_mut().unwrap();
                        match crossed {
                            0 => addition.admitted.operation_id = OperationId::new("unselected-original-A"),
                            1 => addition.interrupted.operation_id = OperationId::new("unselected-pending-C"),
                            2 => { recovery.predecessor_operations.pop(); },
                            3 => recovery.role = ReplicaRole::ActiveSecondary,
                            _ => addition.receiver.process_session_id = old_build.scope.process_session_id.clone(),
                        }
                        ensure!(harness.cluster.submit_ordinary_goal(&fresh_source, &invalid).await.is_err(),
                            "crossed addition selection {crossed} was admitted");
                    }
                    let unchanged = harness.cluster.observe().await?;
                    ensure!(!unchanged.snapshot(&fresh_source).unwrap().goals.iter().any(|entry|
                        entry.goal.scope.process_session_id == *fresh_source.process_session_id()),
                        "rejected addition input mutated the fresh source journal");
                }
                if !selected.goal.scope.configuration.replicas.is_empty() {
                    let mut scalar = selected.goal.clone();
                    scalar.scope.configuration.replicas[0].current_progress = prefix + 100;
                    ensure!(scalar.validate().is_err(), "addition phase accepted a caller progress scalar");
                }
                harness.cluster.submit_ordinary_goal(&fresh_source, &selected.goal).await
                    .context("exact frozen new-voter reconstruction selection rejected")?;
            }
            if snapshot.goals.iter().any(|entry| {
                matches!(&entry.goal.desired, ReplicaGoalKind::RecoverContainedCreation { recovery }
                    if recovery.addition.as_ref().is_some_and(|addition| addition.phase
                        == kuberic_runtime::protocol::public_operations::MembershipAdditionPhase::Bootstrap))
                    && entry.status == ReplicaGoalStatus::Achieved && entry.host_settled
            }) && !snapshot.goals.iter().any(|entry| {
                matches!(&entry.goal.desired, ReplicaGoalKind::RecoverContainedCreation { recovery }
                    if recovery.addition.as_ref().is_some_and(|addition| addition.phase
                        == kuberic_runtime::protocol::public_operations::MembershipAdditionPhase::Current))
                    && entry.status == ReplicaGoalStatus::Achieved && entry.host_settled
            }) {
                ensure!(snapshot.read_status != AccessStatus::Granted && snapshot.write_status != AccessStatus::Granted,
                    "intermediate addition achievement granted application access");
                if !checked_crossed_copy
                    && let Some(source) = snapshot.publication.as_ref().filter(|publication|
                        publication.stage == ReplicaPublicationStage::AchievedScope
                            && publication.scope.configuration.previous.is_none()
                            && publication.scope.configuration.current.members.len() == 1)
                    {
                        let ReplicaGoalKind::Build { remote: old } = &old_build.desired else { unreachable!() };
                        let mut remote = old.as_ref().clone();
                        remote.source = source.clone();
                        remote.target.scope.epoch = source.scope.epoch;
                        remote.target.scope.configuration = source.scope.configuration.clone();
                        let mut invalid = old_build.clone();
                        invalid.operation_id = OperationId::new("crossed-copy-from-intermediate-addition");
                        invalid.revision = snapshot.accepted_revision_high_watermark + 1;
                        invalid.scope = source.scope.clone();
                        invalid.desired = ReplicaGoalKind::Build { remote: Box::new(remote) };
                        invalid.validate().map_err(anyhow::Error::msg)?;
                        let refused = harness.cluster.submit_ordinary_goal(&fresh_source, &invalid).await.unwrap_err();
                        ensure!(format!("{:#}", anyhow::Error::from(refused))
                            .contains("ordered addition permits only its selected phase/copy continuation"),
                            "intermediate P0 did not enforce its exact selected copy receiver");
                        checked_crossed_copy = true;
                }
            }
            if status.public_v1_topology.as_ref().is_some_and(|topology| {
                topology.member_count == 2 && topology.sessions.iter().any(|session| {
                    session.process_session_id == *fresh_source.process_session_id()
                }) && topology.sessions.iter().any(|session| {
                    session.process_session_id == *fresh_target.process_session_id()
                })
            }) {
                break;
            }
        }
        harness.ready_members(2).await?;
        harness.assert_values_on_members("new-voter-prefix", "acknowledged", 2).await?;
        let writer = harness.cluster.service(&fresh_source).await?;
        let new = harness.cluster.probe(writer.replicate_put(
            "new-voter-new".into(), "fresh-scope".into()
        )).await?;
        ensure!(new > prefix);
        harness.assert_values_on_members("new-voter-new", "fresh-scope", 2).await?;
        let final_observation = harness.cluster.observe().await?;
        let recovered = final_observation.snapshot(&fresh_source).unwrap();
        let mut phases = Vec::new();
        for entry in &recovered.goals {
            if let ReplicaGoalKind::RecoverContainedCreation { recovery } = &entry.goal.desired
                && let Some(addition) = &recovery.addition
            {
                ensure!(addition.interrupted.as_ref() == &goal);
                ensure!(entry.status == ReplicaGoalStatus::Achieved && entry.host_settled);
                ensure!(entry.completed_handoff.is_none(), "membership addition invented a transfer certificate");
                phases.push(addition.phase);
                eprintln!("C12 achieved ordered phase {:?}: operation={}, epoch={:?}, current={}, previous={:?}, quorum={}, receipts={}, accepted-and-joined={}",
                    addition.phase, entry.goal.operation_id, entry.goal.scope.epoch,
                    entry.goal.scope.configuration.current.configuration_id,
                    entry.goal.scope.configuration.previous.as_ref().map(|previous| &previous.configuration_id),
                    entry.goal.scope.configuration.current.write_quorum, entry.receipts, entry.host_settled);
            }
        }
        use kuberic_runtime::protocol::public_operations::MembershipAdditionPhase as Phase;
        ensure!(phases == [Phase::Bootstrap, Phase::Joint, Phase::Current]);
        ensure!(checked_crossed_copy, "exact intermediate copy selection negative did not execute");
        let bootstrap = recovered.goals.iter().find(|entry| matches!(&entry.goal.desired,
            ReplicaGoalKind::RecoverContainedCreation { recovery } if recovery.addition.as_ref()
                .is_some_and(|addition| addition.phase == Phase::Bootstrap))).unwrap();
        let receiver = final_observation.snapshot(&fresh_target).unwrap().goals.iter()
            .find(|entry| matches!(&entry.goal.desired, ReplicaGoalKind::RecoverContainedCreation { recovery }
                if recovery.addition.as_ref().is_some_and(|addition| addition.phase == Phase::Receiver))).unwrap();
        ensure!(receiver.status == ReplicaGoalStatus::Achieved && receiver.host_settled
            && receiver.goal.scope.configuration == bootstrap.goal.scope.configuration);
        let copied = recovered.goals.iter().find(|entry| entry.goal.scope == bootstrap.goal.scope
            && matches!(&entry.goal.desired, ReplicaGoalKind::Build { remote }
                if remote.target.scope.process_session_id == *fresh_target.process_session_id())).unwrap();
        ensure!(copied.status == ReplicaGoalStatus::Achieved && copied.host_settled
            && copied.goal.operation_id != old_build.operation_id);
        eprintln!("C12 real Idle/P0={}, receipts={}, fresh Build/copy/Ready/ACK={}, prefix LSN={prefix}, new acknowledged write LSN={new}",
            receiver.goal.operation_id, receiver.receipts, copied.goal.operation_id);
        ensure!(recovered.goals.iter().any(|entry| entry.goal == goal
            && matches!(entry.status, ReplicaGoalStatus::Failed(_) | ReplicaGoalStatus::Uncertain(_))));
        let final_scope = &recovered.publication.as_ref().unwrap().scope.configuration;
        ensure!(final_scope.current.primary_id == goal.scope.configuration.current.primary_id
            && final_scope.current.members == goal.scope.configuration.current.members
            && final_scope.current.write_quorum == goal.scope.configuration.current.write_quorum
            && final_scope.previous.is_none());
        eprintln!("C12 final actual source native roots: {}", harness.cluster.native_transport_observation(&fresh_source).await?);
        eprintln!("C12 final actual receiver native roots: {}", harness.cluster.native_transport_observation(&fresh_target).await?);
        Ok::<_, anyhow::Error>(())
    }.await;
    harness.finish(result).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn m3_addition_current_callback_failure_negative_ack_preserves_recovery_obligations()
-> Result<()> {
    use kuberic_runtime::protocol::public_operations::MembershipAdditionPhase as Phase;

    let mut harness = Harness::with_members(1).await?;
    let result = async {
        let primary_id = harness.ready_members(1).await?;
        let source = harness.reference(primary_id).await?;
        let writer = harness.cluster.service(&source).await?;
        harness.cluster.probe(writer.replicate_put(
            "negative-ack-prefix".into(), "acknowledged".into()
        )).await?;
        harness.cluster.scale_to(2).await?;
        let admission = harness.cluster.pause_at(Barrier::Role {
            stage: BarrierStage::Intent,
            role: ReplicaRole::Primary,
            participant: Some(source.clone()),
        }).await?;
        let original = admission.evidence().goal().context("pending C missing")?.goal.clone();
        ensure!(original.scope.configuration.previous.as_ref()
            .is_some_and(|pc| pc.members.len() == 1));
        ensure!(original.scope.configuration.current.members.len() == 2);
        let observed = harness.cluster.observe().await?;
        let target = observed.replicas().find(|reference| reference.replica_id() != primary_id)
            .context("new voter target missing")?.clone();
        let old_build = observed.snapshot(&source).unwrap().goals.iter()
            .find(|entry| matches!(entry.goal.desired, ReplicaGoalKind::Build { .. })
                && entry.status == ReplicaGoalStatus::Achieved && entry.host_settled)
            .context("old Ready was not actually accepted")?.goal.clone();
        let cut = harness.cluster.arm_instruction(
            &source, &original, "Configuration", InstructionPosition::Selected
        ).await?;
        harness.cluster.resume(admission).await?;
        harness.cluster.enter_instruction(&cut).await?;
        let image = harness.cluster.capture_instruction_image(&cut).await?;
        let terminal = harness.cluster.retain_ordinary_terminal(&target, PublicV1CallKind::Close).await?;
        loop {
            harness.cluster.step().await?;
            if harness.cluster.observe().await?.snapshot(&target).unwrap().goals.iter()
                .any(|entry| entry.goal == terminal
                    && entry.status == ReplicaGoalStatus::Achieved && entry.host_settled) {
                break;
            }
        }
        harness.cluster.stop(&target).await?;
        let fresh_target = harness.cluster.request_restart(&target).await?;
        ensure!(fresh_target.storage_incarnation() == target.storage_incarnation());
        ensure!(harness.cluster.submit_ordinary_goal(&source, &old_build).await.is_err());
        cut.release();
        let completion = harness.cluster.pause_at(Barrier::Role {
            stage: BarrierStage::Completion,
            role: ReplicaRole::Primary,
            participant: Some(source.clone()),
        }).await?;
        ensure!(completion.evidence().goal().is_some_and(|entry| entry.goal == original));
        let denied = harness.cluster.observe().await?;
        ensure!(denied.snapshot(&source).unwrap().goals.iter().any(|entry| entry.goal == original
            && matches!(entry.status, ReplicaGoalStatus::Failed(_) | ReplicaGoalStatus::Uncertain(_))),
            "actual original Configuration did not fail after target reopen");
        harness.cluster.resume(completion).await?;
        let fresh_source = harness.cluster.reopen_instruction_image(image).await?;
        ensure!(writer.get("negative-ack-prefix").await.is_err());
        let current = loop {
            harness.cluster.step().await?;
            let observed = harness.cluster.observe().await?;
            let snapshot = observed.snapshot(&fresh_source).unwrap();
            ensure!(!snapshot.goals.iter().any(|entry|
                entry.goal.scope.process_session_id == *fresh_source.process_session_id()
                    && matches!(entry.status, ReplicaGoalStatus::Failed(_) | ReplicaGoalStatus::Uncertain(_))),
                "recovery failed before the selected Current callback");
            let selected = observed.set().status.as_ref().unwrap().replica_goals.iter()
                .find(|entry| entry.goal.scope.process_session_id == *fresh_source.process_session_id()
                    && matches!(entry.goal.desired, ReplicaGoalKind::RecoverContainedCreation { .. })
                    && entry.outcome.is_none()
                    && !snapshot.goals.iter().any(|local| local.goal == entry.goal));
            if let Some(selected) = selected {
                let goal = selected.goal.clone();
                if matches!(&goal.desired, ReplicaGoalKind::RecoverContainedCreation { recovery }
                    if recovery.addition.as_ref().is_some_and(|addition| addition.phase == Phase::Current))
                {
                    break goal;
                }
                harness.cluster.submit_ordinary_goal(&fresh_source, &goal).await?;
            }
        };
        let failure = harness.cluster.arm_addition_current_failure(&fresh_source, &current).await?;
        harness.cluster.submit_ordinary_goal(&fresh_source, &current).await?;
        let evidence = harness.cluster.enter_instruction(&failure).await?;
        ensure!(evidence.recipe == ["ProgramEpoch", "Configuration", "FirstCatchUp", "ApplicationRole"]);
        ensure!(evidence.index == 3 && evidence.receipts == 3
            && evidence.selected_call.as_deref() == Some("ApplicationRole"),
            "Current did not execute real native Configuration/All before application failure");
        failure.release();
        let before_snapshot = loop {
            let observed = harness.cluster.observe().await?;
            let snapshot = observed.snapshot(&fresh_source).unwrap();
            if snapshot.goals.iter().any(|entry| entry.goal == current
                && matches!(&entry.status, ReplicaGoalStatus::Failed(error)
                    if error.contains("injected owner-bound post-effect addition Current application error")))
            {
                break snapshot.clone();
            }
            tokio::task::yield_now().await;
        };
        let failed = before_snapshot.goals.iter().find(|entry| entry.goal == current).unwrap();
        ensure!(!failed.host_settled && failed.receipts == 3
            && failed.selected_call.as_deref() == Some("ApplicationRole"));
        ensure!(before_snapshot.goals.iter().any(|entry| entry.goal == original
            && matches!(entry.status, ReplicaGoalStatus::Failed(_) | ReplicaGoalStatus::Uncertain(_))));
        for phase in [Phase::Bootstrap, Phase::Joint] {
            ensure!(before_snapshot.goals.iter().any(|entry| matches!(&entry.goal.desired,
                ReplicaGoalKind::RecoverContainedCreation { recovery } if recovery.addition.as_ref()
                    .is_some_and(|addition| addition.phase == phase))
                && entry.status == ReplicaGoalStatus::Achieved && entry.host_settled));
        }
        ensure!(before_snapshot.goals.iter().any(|entry| matches!(&entry.goal.desired,
            ReplicaGoalKind::Build { remote }
                if remote.target.scope.process_session_id == *fresh_target.process_session_id())
            && entry.status == ReplicaGoalStatus::Achieved && entry.host_settled));
        let (before, pending) = harness.cluster.addition_ack_observation(&fresh_source).await?;
        ensure!(pending, "Current callback failure lost addition_pending");
        let ReplicaGoalKind::RecoverContainedCreation { recovery } = &current.desired else { unreachable!() };
        ensure!(recovery.addition.as_ref().unwrap().interrupted.as_ref() == &original);
        ensure!(recovery.predecessor_operations.contains(&original.operation_id));
        for predecessor in &recovery.predecessor_operations {
            let record = &before["operations"][predecessor.as_str()];
            ensure!(record.is_object() && record["supersededBy"].is_null(),
                "predecessor was already superseded before negative ACK: {predecessor}");
        }
        let current_id = current.operation_id.as_str();
        eprintln!("ADD19-R1 actual failed Current journal: {}", before["operations"][current_id]);
        ensure!(before["operations"][current_id]["stage"] == "completed"
            && before["operations"][current_id]["disposition"]["kind"] == "failed"
            && before["operations"][current_id]["lifecycle"]["in_flight"] == "ApplicationRole"
            && before["operations"][current_id]["ordinary"]["accepted"] == false
            && before["operations"][current_id]["ordinary"]["host_settled"] == false);
        let service = harness.cluster.service(&fresh_source).await?;
        ensure!(before_snapshot.read_status != AccessStatus::Granted
            && before_snapshot.write_status != AccessStatus::Granted);
        ensure!(service.get("negative-ack-prefix").await.is_err());
        ensure!(service.replicate_put("negative-ack-denied".into(), "before".into()).await.is_err());
        let mut expected = before.clone();
        expected["operations"][current_id]["ordinary"]["accepted"] = serde_json::json!(true);
        expected["operations"][current_id]["ordinary"]["host_settled"] = serde_json::json!(true);
        let mut expected_goals = before_snapshot.goals.clone();
        expected_goals.iter_mut().find(|entry| entry.goal == current).unwrap().host_settled = true;
        for attempt in 0..2 {
            let error = harness.cluster.acknowledge_ordinary_goal(&fresh_source, &current)
                .await.expect_err("failed Current ACK unexpectedly released final access");
            let error = format!("{:#}", anyhow::Error::from(error));
            ensure!(error.contains("addition final acceptance missing"),
                "ACK did not reach the post-transaction access guard: {error}");
            let (after, pending) = harness.cluster.addition_ack_observation(&fresh_source).await?;
            ensure!(after == expected,
                "negative ACK changed predecessor links, historical failures or outstanding obligations");
            ensure!(pending, "negative ACK cleared addition_pending");
            let observed = harness.cluster.observe().await?;
            let snapshot = observed.snapshot(&fresh_source).unwrap();
            ensure!(snapshot.goals == expected_goals
                && snapshot.read_status != AccessStatus::Granted
                && snapshot.write_status != AccessStatus::Granted,
                "negative ACK changed the failed outcome or opened access");
            ensure!(service.get("negative-ack-prefix").await.is_err());
            ensure!(service.replicate_put("negative-ack-denied".into(), "after".into()).await.is_err());
            eprintln!("ADD19-R1 actual Current negative ACK attempt={attempt}, operation={}, native receipts={:?}, committed accepted={}, host_settled={}, addition_pending={pending}, RPC={error}; complete journal except negative acceptance unchanged",
                current.operation_id, evidence.recipe,
                after["operations"][current_id]["ordinary"]["accepted"],
                after["operations"][current_id]["ordinary"]["host_settled"]);
        }
        Ok::<_, anyhow::Error>(())
    }.await;
    harness.finish(result).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn m3_failed_build_typed_before_failure_receipt_recovers_data() -> Result<()> {
    super::recovery_images::source_build_result_image(
        InstructionPosition::FailureBeforeReceipt,
        Some(kuberic_runtime::testing::PublicBuildFailure::TypedRemote),
    )
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn m3_failed_build_typed_after_failure_receipt_recovers_data() -> Result<()> {
    super::recovery_images::source_build_result_image(
        InstructionPosition::FailureRecorded,
        Some(kuberic_runtime::testing::PublicBuildFailure::TypedRemote),
    )
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn m3_failed_build_untyped_before_failure_receipt_recovers_data() -> Result<()> {
    super::recovery_images::source_build_result_image(
        InstructionPosition::FailureBeforeReceipt,
        Some(kuberic_runtime::testing::PublicBuildFailure::UntypedPostEffect),
    )
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn m3_failed_build_untyped_after_failure_receipt_recovers_data() -> Result<()> {
    super::recovery_images::source_build_result_image(
        InstructionPosition::FailureRecorded,
        Some(kuberic_runtime::testing::PublicBuildFailure::UntypedPostEffect),
    )
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn m3_tombstone_reopen_allows_only_fresh_settled_generation() -> Result<()> {
    let mut harness = Harness::with_members(1).await?;
    let result = async {
        let primary_id = harness.ready_members(1).await?;
        let source = harness.reference(primary_id).await?;
        let writer = harness.cluster.service(&source).await?;
        let prefix = harness.cluster.probe(writer.replicate_put(
            "tombstone-prefix".into(), "acknowledged".into()
        )).await?;
        harness.cluster.scale_to(2).await?;
        let prepared = harness.cluster.pause_at(Barrier::Role {
            stage: BarrierStage::Completion,
            role: ReplicaRole::IdleSecondary,
            participant: None,
        }).await?;
        let prepare = prepared.evidence().goal().context("idle preparation missing")?.goal.clone();
        let observation = harness.cluster.observe().await?;
        let target = observation.replicas().find(|reference| {
            reference.storage_incarnation() == &prepare.scope.storage
        }).context("idle target missing")?.clone();
        harness.cluster.resume(prepared).await?;
        let remove = harness.cluster.retain_remove_before_build(&source, &target).await?;
        let completion = harness.cluster.pause_at(Barrier::completion(PublicV1CallKind::Remove)).await?;
        ensure!(completion.evidence().goal().is_some_and(|entry| entry.goal == remove));
        let image = harness.cluster.capture_tombstone_image(&source, &remove).await?;
        harness.cluster.resume(completion).await?;
        let ReplicaGoalKind::Remove { remote } = &remove.desired else { unreachable!() };
        let mut late = remove.clone();
        late.operation_id = OperationId::new("late-build-after-tombstone");
        late.revision += 1;
        late.desired = ReplicaGoalKind::Build { remote: remote.clone() };
        let denied = harness.cluster.submit_ordinary_goal(&source, &late).await.unwrap_err();
        let denied = format!("{:#}", anyhow::Error::from(denied));
        ensure!(denied.contains("tombstone") || denied.contains("watermark"), "{denied}");
        harness.cluster.recreate_controller().await?;
        let fresh_source = harness.cluster.reopen_tombstone_image(image).await?;
        ensure!(fresh_source.storage_incarnation() == source.storage_incarnation());
        ensure!(harness.cluster.submit_ordinary_goal(&fresh_source, &late).await.is_err());
        let opened = harness.cluster.observe().await?;
        let tombstone = opened.snapshot(&fresh_source).unwrap().remote_operations.iter()
            .find(|entry| entry.selection == **remote)
            .context("retained remove watermark missing")?.clone();
        ensure!(tombstone.state == RemoteReplicaState::Dropped);
        let terminal = harness.cluster.retain_ordinary_terminal(&target, PublicV1CallKind::Close).await?;
        loop {
            harness.cluster.step().await?;
            if harness.cluster.observe().await?.snapshot(&target).unwrap().goals.iter()
                .any(|entry| entry.goal == terminal
                    && entry.status == ReplicaGoalStatus::Achieved && entry.host_settled) {
                break;
            }
        }
        harness.cluster.stop(&target).await?;
        let fresh_target = harness.cluster.request_restart(&target).await?;
        ensure!(fresh_target.storage_incarnation() == target.storage_incarnation());
        ensure!(fresh_target.process_session_id() != target.process_session_id());
        harness.cluster.recreate_controller().await?;
        harness.ready_members(2).await?;
        harness.assert_values_on_members("tombstone-prefix", "acknowledged", 2).await?;
        let view = harness.cluster.observe().await?;
        let snapshot = view.snapshot(&fresh_source).unwrap();
        ensure!(snapshot.remote_operations.iter().any(|entry| entry == &tombstone));
        let build = snapshot.goals.iter().find(|entry| {
            matches!(&entry.goal.desired, ReplicaGoalKind::Build { remote: selected }
                if selected.target.scope.process_session_id == *fresh_target.process_session_id()
                    && selected.target.scope.creation_id == *fresh_target.replicator_creation_id()
                    && selected.target.scope.storage == remote.target.scope.storage)
                && entry.status == ReplicaGoalStatus::Achieved && entry.host_settled
        }).context("fresh settled generation did not actually Build")?;
        ensure!(matches!(&build.goal.desired, ReplicaGoalKind::Build { remote: selected }
            if selected != remote));
        let writer = harness.cluster.service(&fresh_source).await?;
        let new = harness.cluster.probe(writer.replicate_put(
            "tombstone-new".into(), "fresh-generation".into()
        )).await?;
        ensure!(new > prefix);
        harness.assert_values_on_members("tombstone-new", "fresh-generation", 2).await?;
        eprintln!("C13 CLOSED actual no-callback Remove={remove:?}; retained tombstone={tombstone:?}; fresh Build={build:?}; prefix={prefix} new={new}");
        Ok::<_, anyhow::Error>(())
    }.await;
    harness.finish(result).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn m3_role_demoted_replicator_images_recover_service() -> Result<()> {
    handoff_images(false, "ReplicatorRole").await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn m3_role_demoted_application_images_recover_service() -> Result<()> {
    handoff_images(false, "ApplicationRole").await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn m3_role_promoted_replicator_images_recover_service() -> Result<()> {
    handoff_images(true, "ReplicatorRole").await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn m3_role_promoted_application_images_recover_service() -> Result<()> {
    handoff_images(true, "ApplicationRole").await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn m3_epoch_source_images_recover_service() -> Result<()> {
    handoff_images(false, "ProgramEpoch").await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn m3_epoch_target_images_recover_service() -> Result<()> {
    handoff_images(true, "ProgramEpoch").await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn m3_configuration_primary_joint_images_recover_service() -> Result<()> {
    handoff_images(true, "Configuration").await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn m3_configuration_primary_current_only_images_recover_service() -> Result<()> {
    handoff_recipe_images(true, "Configuration", false, true, &CUTS).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn m3_catchup_source_first_effect_receipt_images_recover_service() -> Result<()> {
    handoff_recipe_images(
        false,
        "FirstCatchUp",
        true,
        false,
        &[InstructionPosition::Effect, InstructionPosition::Receipt],
    )
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn m3_catchup_source_second_effect_receipt_images_recover_service() -> Result<()> {
    handoff_recipe_images(
        false,
        "SecondCatchUp",
        false,
        false,
        &[InstructionPosition::Effect, InstructionPosition::Receipt],
    )
    .await
}

async fn handoff_images(promoted: bool, instruction: &str) -> Result<()> {
    handoff_recipe_images(promoted, instruction, false, false, &CUTS).await
}

async fn handoff_recipe_images(
    promoted: bool,
    instruction: &str,
    source_all: bool,
    current_only: bool,
    positions: &[InstructionPosition],
) -> Result<()> {
    let mut failures = Vec::new();
    for &position in positions {
        let mut harness = Harness::new().await?;
        let result = async {
            let primary_id = harness.ready().await?;
            let source = harness.reference(primary_id).await?;
            let writer = harness.cluster.service(&source).await?;
            let baseline = harness
                .cluster
                .probe(writer.replicate_put("handoff-image".into(), "acknowledged".into()))
                .await?;
            harness.assert_values("handoff-image", "acknowledged").await?;
            let before = harness.cluster.observe().await?;
            let target = before
                .replicas()
                .find(|reference| reference.replica_id() != primary_id)
                .context("handoff target missing")?
                .clone();
            let original_configuration = before
                .snapshot(&source)
                .unwrap()
                .publication
                .as_ref()
                .unwrap()
                .scope
                .configuration
                .current
                .clone();
            let request = harness.cluster.submit_switchover(&target).await?;
            let actor = if promoted { &target } else { &source };
            if current_only {
                let promotion = harness.cluster.pause_at(Barrier::Role {
                    stage: BarrierStage::Completion,
                    role: ReplicaRole::Primary,
                    participant: Some(target.clone()),
                }).await?;
                ensure!(promotion.evidence().goal().is_some_and(|entry| {
                    entry.goal.scope.configuration.previous.is_some()
                }));
                harness.cluster.resume(promotion).await?;
            }
            let intent = harness
                .cluster
                .pause_at(if source_all {
                    Barrier::Role {
                        stage: BarrierStage::Intent,
                        role: ReplicaRole::Primary,
                        participant: Some(source.clone()),
                    }
                } else if promoted {
                    Barrier::Role {
                        stage: BarrierStage::Intent,
                        role: ReplicaRole::Primary,
                        participant: Some(target.clone()),
                    }
                } else {
                    Barrier::SwitchoverHandoff {
                        request: request.clone(),
                        source: source.clone(),
                        stage: BarrierStage::Intent,
                    }
                })
                .await?;
            let goal = intent.evidence().goal().context("role goal missing")?.goal.clone();
            ensure!(goal.scope.configuration.previous.is_some() == (!source_all && !current_only));
            ensure!(
                goal.scope.configuration.current.primary_id
                    == if source_all { source.identity().replica_id } else { target.identity().replica_id }
            );
            let old_service = harness.cluster.service(actor).await?;
            let root = harness.cluster.application_root(actor).await?;
            let cut = harness
                .cluster
                .arm_instruction(actor, &goal, instruction, position)
                .await?;
            harness.cluster.resume(intent).await?;
            let evidence = harness.cluster.enter_instruction(&cut).await?;
            if !promoted && !source_all {
                ensure!(
                    evidence.recipe
                        == [
                            "Revoke",
                            "ProgramEpoch",
                            "RefreshedConfiguration",
                            "SecondCatchUp",
                            "ReplicatorRole",
                            "ApplicationRole",
                            "NativeComplete",
                        ]
                );
            }
            let image = harness.cluster.capture_instruction_image(&cut).await?;
            eprintln!(
                "C07-C10 actual image actor={} promoted={promoted} source_all={source_all} current_only={current_only} baseline={baseline}: {evidence:?}",
                actor.replica_id()
            );
            cut.release();
            let completion = harness
                .cluster
                .pause_at(Barrier::Role {
                    stage: BarrierStage::Completion,
                    role: if promoted || source_all {
                        ReplicaRole::Primary
                    } else {
                        ReplicaRole::ActiveSecondary
                    },
                    participant: Some(actor.clone()),
                })
                .await?;
            ensure!(
                completion
                    .evidence()
                    .goal()
                    .is_some_and(|entry| entry.goal == goal)
            );
            harness.cluster.resume(completion).await?;
            let fresh = harness.cluster.reopen_instruction_image(image).await?;
            ensure!(fresh.storage_incarnation() == actor.storage_incarnation());
            ensure!(fresh.process_session_id() != actor.process_session_id());
            ensure!(harness.cluster.application_root(&fresh).await? == root);
            ensure!(old_service.get("handoff-image").await.is_err());
            ensure!(
                old_service
                    .replicate_put("old-handoff".into(), "denied".into())
                    .await
                    .is_err()
            );
            let opened = harness.cluster.observe().await?;
            let snapshot = opened.snapshot(&fresh).context("reopened role actor missing")?;
            ensure!(
                snapshot.publication.is_none()
                    && snapshot.write_status != AccessStatus::Granted
            );
            let uncertain = snapshot
                .goals
                .iter()
                .find(|entry| entry.goal == goal)
                .context("interrupted role result missing")?
                .clone();
            ensure!(matches!(uncertain.status, ReplicaGoalStatus::Uncertain(_)));
            loop {
                harness.cluster.step().await?;
                let observed = harness.cluster.observe().await?;
                for (reference, snapshot) in observed.snapshots() {
                    if let Some(failed) = snapshot.goals.iter().find(|entry| {
                        entry.goal.scope.process_session_id == *reference.process_session_id()
                            && matches!(entry.status,
                                ReplicaGoalStatus::Failed(_) | ReplicaGoalStatus::Uncertain(_))
                    }) {
                        anyhow::bail!("actual {instruction}/{position:?} recovery failed: {failed:?}");
                    }
                }
                let status = observed.set().status.as_ref().unwrap();
                if let Some(selected) = status.replica_goals.iter().find(|entry| {
                    entry.goal.scope.process_session_id == *fresh.process_session_id()
                        && matches!(entry.goal.desired, ReplicaGoalKind::RecoverContainedCreation { .. })
                        && entry.outcome.is_none()
                        && !observed.snapshot(&fresh).unwrap().goals.iter()
                            .any(|local| local.goal == entry.goal)
                }) {
                    harness.cluster.submit_ordinary_goal(&fresh, &selected.goal).await
                        .with_context(|| format!("exact frozen {instruction}/{position:?} reconstruction selection rejected"))?;
                }
                let recovered = observed.snapshot(&fresh).unwrap();
                if !promoted && !source_all
                    && recovered.publication.as_ref().is_some_and(|publication| {
                        publication.stage == ReplicaPublicationStage::AchievedScope
                            && publication.role == ReplicaRole::ActiveSecondary
                            && publication.scope.configuration.previous.is_some()
                    })
                    && observed.snapshot(&target).is_some_and(|snapshot| snapshot.withdrawn)
                    && recovered.goals.iter().any(|entry| matches!(entry.goal.desired,
                        ReplicaGoalKind::RecoverContainedCreation { .. })
                        && entry.status == ReplicaGoalStatus::Achieved && entry.host_settled)
                {
                    anyhow::bail!("actual recovered demoted joint member has no selected handoff continuation; original receiver was independently closed; frozen source={recovered:?}");
                }
                if status.public_v1_topology.as_ref().is_some_and(|topology| {
                    topology.primary_id == target.identity().replica_id
                        && topology.completed_switchover_request_id.as_deref() == Some(request.id())
                        && topology.sessions.iter().any(|session| {
                            session.process_session_id == *fresh.process_session_id()
                        })
                }) {
                    break;
                }
            }
            let primary = harness.ready().await?;
            ensure!(primary == target.replica_id());
            let final_view = harness.cluster.observe().await?;
            let recovered = final_view.snapshot(&fresh).unwrap();
            ensure!(recovered.goals.iter().any(|entry| entry == &uncertain));
            let topology = final_view.set().status.as_ref().unwrap()
                .public_v1_topology.as_ref().unwrap();
            ensure!(
                topology.completed_switchover_request_id.as_deref() == Some(request.id())
                    && topology.member_count == original_configuration.members.len() as u32
                    && topology.configuration().map_err(anyhow::Error::msg)?.write_quorum
                        == original_configuration.write_quorum
            );
            harness.assert_values("handoff-image", "acknowledged").await?;
            harness.write("handoff-image-new", "fresh-native-scope").await?;
            harness.assert_values("handoff-image-new", "fresh-native-scope").await?;
            Ok::<_, anyhow::Error>(())
        }
        .await;
        if let Err(error) = harness.finish(result).await {
            failures.push(format!("{instruction}/{position:?}: {error:#}"));
        }
    }
    ensure!(
        failures.is_empty(),
        "closed recovery matrix remains red:\n{}",
        failures.join("\n")
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn m3_terminal_source_close_cleanup_images_recover_service() -> Result<()> {
    terminal_images(false, PublicV1CallKind::Close).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn m3_terminal_source_abort_cleanup_images_recover_service() -> Result<()> {
    terminal_images(false, PublicV1CallKind::Abort).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn m3_terminal_target_close_cleanup_images_recover_service() -> Result<()> {
    terminal_images(true, PublicV1CallKind::Close).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn m3_terminal_target_abort_cleanup_images_recover_service() -> Result<()> {
    terminal_images(true, PublicV1CallKind::Abort).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn m3_terminal_source_abort_instruction_images_recover_service() -> Result<()> {
    terminal_images_at(false, PublicV1CallKind::Abort, "Abort").await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn m3_terminal_target_abort_instruction_images_recover_service() -> Result<()> {
    terminal_images_at(true, PublicV1CallKind::Abort, "Abort").await
}

async fn terminal_images(target: bool, kind: PublicV1CallKind) -> Result<()> {
    terminal_images_at(target, kind, "Cleanup").await
}

async fn terminal_images_at(target: bool, kind: PublicV1CallKind, instruction: &str) -> Result<()> {
    for position in CUTS {
        let members = if target { 3 } else { 1 };
        let mut harness = Harness::with_members(members).await?;
        let result = async {
            let primary_id = harness.ready_members(members as usize).await?;
            let primary = harness.reference(primary_id).await?;
            let writer = harness.cluster.service(&primary).await?;
            let key = "closed-recovery-prefix";
            let watermark = harness
                .cluster
                .probe(writer.replicate_put(key.into(), "acknowledged".into()))
                .await?;
            harness
                .assert_values_on_members(key, "acknowledged", members as usize)
                .await?;
            let original = harness.cluster.observe().await?;
            let actor = original
                .replicas()
                .find(|reference| (reference.replica_id() != primary_id) == target)
                .context("terminal actor missing")?
                .clone();
            let old_service = harness.cluster.service(&actor).await?;
            let root = harness.cluster.application_root(&actor).await?;
            let configuration = original
                .snapshot(&primary)
                .and_then(|snapshot| snapshot.publication.as_ref())
                .context("original configuration missing")?
                .scope
                .configuration
                .current
                .clone();
            let terminal = harness.cluster.retain_ordinary_terminal(&actor, kind).await?;
            let cut = harness
                .cluster
                .arm_instruction(&actor, &terminal, instruction, position)
                .await?;
            let evidence = harness.cluster.enter_instruction(&cut).await?;
            ensure!(evidence.position == position && evidence.instruction == instruction);
            ensure!(
                evidence.recipe
                    == if kind == PublicV1CallKind::Abort {
                        vec!["Revoke", "Abort", "Cleanup"]
                    } else {
                        vec!["Revoke", "Cleanup"]
                    },
                "ordinary terminal Cleanup is not a standalone child Close recipe"
            );
            let image = harness.cluster.capture_instruction_image(&cut).await?;
            eprintln!(
                "C11 metadata-image + actual joins actor={} target={target} kind={kind:?} call={instruction}: {evidence:?}",
                actor.replica_id()
            );
            cut.release();
            let completion = harness
                .cluster
                .pause_at(Barrier::Operation {
                    stage: BarrierStage::Completion,
                    kind: Some(kind),
                    participant: Some(actor.clone()),
                })
                .await?;
            ensure!(
                completion
                    .evidence()
                    .goal()
                    .is_some_and(|entry| entry.goal == terminal)
            );
            harness.cluster.resume(completion).await?;
            let fresh = harness.cluster.reopen_instruction_image(image).await?;
            ensure!(fresh.storage_incarnation() == actor.storage_incarnation());
            ensure!(fresh.process_session_id() != actor.process_session_id());
            ensure!(fresh.replicator_creation_id() != actor.replicator_creation_id());
            ensure!(harness.cluster.application_root(&fresh).await? == root);
            ensure!(old_service.get(key).await.is_err());
            ensure!(
                old_service
                    .replicate_put("old-owner".into(), "forbidden".into())
                    .await
                    .is_err()
            );
            let reopened = harness.cluster.observe().await?;
            let snapshot = reopened.snapshot(&fresh).context("fresh actor missing")?;
            ensure!(snapshot.publication.is_none());
            ensure!(
                snapshot.read_status != AccessStatus::Granted
                    && snapshot.write_status != AccessStatus::Granted
            );
            let old_result = snapshot
                .goals
                .iter()
                .find(|entry| entry.goal == terminal)
                .context("interrupted terminal missing")?
                .clone();
            ensure!(
                matches!(old_result.status, ReplicaGoalStatus::Uncertain(_)),
                "an interrupted terminal image cannot claim its later actual join"
            );
            let new_primary = harness.ready_members(members as usize).await?;
            ensure!(new_primary == primary_id);
            let final_view = harness.cluster.observe().await?;
            let snapshot = final_view.snapshot(&fresh).context("recovered actor missing")?;
            ensure!(snapshot.goals.iter().any(|entry| entry == &old_result));
            let recovery = snapshot
                .goals
                .iter()
                .find(|entry| {
                    entry.goal.scope.process_session_id == *fresh.process_session_id()
                        && matches!(
                            entry.goal.desired,
                            ReplicaGoalKind::RecoverContainedCreation { .. }
                        )
                })
                .context("actual reconstruction result missing")?;
            ensure!(
                recovery.status == ReplicaGoalStatus::Achieved && recovery.host_settled
            );
            let publication = snapshot.publication.as_ref().unwrap();
            ensure!(
                publication.stage == ReplicaPublicationStage::AchievedScope
                    && publication.role
                        == if target {
                            ReplicaRole::ActiveSecondary
                        } else {
                            ReplicaRole::Primary
                        }
                    && publication.scope.configuration.previous.is_none()
                    && publication.scope.configuration.current.members == configuration.members
                    && publication.scope.configuration.current.write_quorum
                        == configuration.write_quorum
            );
            let sampled = harness.cluster.sample_replica(&fresh).await?;
            ensure!(sampled.progress.value().is_some_and(|lsn| lsn >= watermark));
            harness
                .assert_values_on_members(key, "acknowledged", members as usize)
                .await?;
            let source = harness.reference(primary_id).await?;
            let writer = harness.cluster.service(&source).await?;
            let lsn = harness
                .cluster
                .probe(writer.replicate_put("closed-recovery-new".into(), "fresh-scope".into()))
                .await?;
            ensure!(lsn > watermark);
            harness
                .assert_values_on_members("closed-recovery-new", "fresh-scope", members as usize)
                .await?;
            eprintln!(
                "C11 CLOSED actor={} target={target} kind={kind:?} call={instruction} cut={position:?} prefix={watermark} new={lsn} fresh_process={} fresh_creation={}",
                actor.replica_id(),
                fresh.process_session_id(),
                fresh.replicator_creation_id()
            );
            Ok::<_, anyhow::Error>(())
        }
        .await;
        harness.finish(result).await?;
    }
    Ok(())
}
