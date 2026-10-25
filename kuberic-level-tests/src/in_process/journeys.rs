use anyhow::{Context, Result, bail, ensure};
use k8s_openapi::api::core::v1::{Pod, Secret, Service};
use kube::{Resource, ResourceExt};
use kuberic_controller::crd::PublicV1OperationStatus;
use kuberic_runtime::protocol::public_operations::{
    PublicOperationProgram, PublicV1AcceptedOperationCompletion, PublicV1CallKind,
    PublicV1OperationResult,
};

use super::harness::Harness;
use kuberic_controller::test_util::{Barrier, BarrierStage, ResourceKind as Kind, SupportResource};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn actual_v1_nonempty_copy_separates_remote_ready_idle_and_voting() -> Result<()> {
    use kuberic_runtime::engine::DurableState;
    use kuberic_runtime::protocol::public_operations::{
        RemoteReplicaState, ReplicaGoalKind, ReplicaGoalStatus, ReplicaPublicationStage,
    };
    use kuberic_runtime::protocol::types::{AccessStatus, ReplicaRole};
    use std::time::Duration;

    let mut harness = Harness::with_members(1).await?;
    let mut held = None;
    let result = async {
        let source_id = harness.ready_members(1).await?;
        eprintln!("actual-v1: singleton achieved");
        let primary = harness.reference(source_id).await?;
        let source = harness.cluster.service(&primary).await?;
        harness
            .cluster
            .probe(source.replicate_put("copied-before-build".into(), "nonempty".into()))
            .await?;
        let predecessor = harness
            .cluster
            .observe()
            .await?
            .snapshot(&primary)
            .and_then(|snapshot| snapshot.publication.as_ref())
            .context("source achieved scope missing")?
            .scope
            .configuration
            .clone();
        let gate = harness.copy_gate(source_id).await?;
        gate.hold()?;
        held = Some(gate.clone());
        harness.cluster.scale_to(2).await?;
        let started = tokio::time::Instant::now();
        let (build, target_ref, running) = loop {
            harness
                .cluster
                .drive(async {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                    Ok::<_, anyhow::Error>(())
                })
                .await?;
            let observed = harness.cluster.observe().await?;
            let snapshot = observed
                .snapshot(&primary)
                .context("source snapshot missing")?;
            ensure!(
                started.elapsed() < Duration::from_secs(20),
                "actual Build did not enter: goals {:?}, remotes {:?}",
                snapshot
                    .goals
                    .iter()
                    .map(|entry| (&entry.goal.desired, &entry.status))
                    .collect::<Vec<_>>(),
                snapshot
                    .remote_operations
                    .iter()
                    .map(|entry| &entry.state)
                    .collect::<Vec<_>>()
            );
            if let Some(remote) = snapshot
                .remote_operations
                .iter()
                .find(|remote| remote.state == RemoteReplicaState::InBuild)
            {
                let goal = snapshot
                    .goals
                    .iter()
                    .find(|entry| entry.goal.operation_id == remote.operation_id)
                    .context("retained Build missing")?;
                ensure!(goal.status == ReplicaGoalStatus::Pending);
                let target_ref = observed
                    .replicas()
                    .find(|reference| {
                        reference.identity() == &remote.selection.target.scope.storage.replica
                    })
                    .cloned()
                    .context("target missing")?;
                break (goal.goal.clone(), target_ref, snapshot.clone());
            }
        };
        let target = harness.cluster.service(&target_ref).await?;
        eprintln!("actual-v1: real Build held in provider");
        let ReplicaGoalKind::Build { remote } = &build.desired else {
            bail!("actual ordinary Build required")
        };
        ensure!(
            remote.source.scope.configuration == predecessor
                && remote.target.stage == ReplicaPublicationStage::AchievedScope
                && remote.target.role == ReplicaRole::IdleSecondary
                && remote.source.scope.storage == *primary.storage_incarnation()
                && remote.target.scope.storage == *target_ref.storage_incarnation()
        );
        ensure!(
            running
                .remote_operations
                .iter()
                .all(|entry| entry.state != RemoteReplicaState::Ready),
            "provider acquisition fabricated Ready"
        );

        gate.release()?;
        eprintln!("actual-v1: provider released; awaiting exact unaccepted Ready");
        let started = tokio::time::Instant::now();
        let observed = loop {
            let observed = harness.cluster.observe().await?;
            let snapshot = observed
                .snapshot(&primary)
                .context("source snapshot missing")?;
            if started.elapsed() >= Duration::from_secs(15) {
                eprintln!(
                    "optional native source: {:?}",
                    harness.cluster.native_transport_observation(&primary).await
                );
                eprintln!(
                    "optional native target: {:?}",
                    harness
                        .cluster
                        .native_transport_observation(&target_ref)
                        .await
                );
                bail!(
                    "actual copied Build did not reach Ready: {:?}, {:?}, copied {:?}",
                    snapshot
                        .goals
                        .iter()
                        .map(|entry| &entry.status)
                        .collect::<Vec<_>>(),
                    snapshot
                        .remote_operations
                        .iter()
                        .map(|entry| &entry.state)
                        .collect::<Vec<_>>(),
                    target.persistence().get("copied-before-build")
                );
            }
            if snapshot.remote_operations.iter().any(|entry| {
                entry.operation_id == build.operation_id
                    && entry.selection == **remote
                    && entry.state == RemoteReplicaState::Ready
                    && !entry.accepted
            }) {
                ensure!(snapshot.goals.iter().any(
                    |entry| entry.goal == build && entry.status == ReplicaGoalStatus::Achieved
                ));
                break observed;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        };
        let report = observed
            .snapshot(&target_ref)
            .context("target snapshot missing")?;
        eprintln!("actual-v1: real application bytes copied before admission");
        ensure!(
            report
                .publication
                .as_ref()
                .is_some_and(|publication| publication.role == ReplicaRole::IdleSecondary)
                && report.read_status != AccessStatus::Granted
                && report.write_status != AccessStatus::Granted
        );
        ensure!(
            !predecessor
                .current
                .members
                .iter()
                .any(|member| member.identity == report.storage.replica)
        );
        ensure!(
            target.get("copied-before-build").await.is_err()
                && target
                    .replicate_put("forbidden-idle".into(), "write".into())
                    .await
                    .is_err()
        );
        let committed = target.persistence().durable_progress().await?.committed_lsn;
        ensure!(
            target
                .persistence()
                .snapshot_at(committed)?
                .get("copied-before-build")
                .map(String::as_str)
                == Some("nonempty"),
            "real V1 copy omitted preexisting KV contents"
        );
        loop {
            harness.cluster.step().await?;
            let observed = harness.cluster.observe().await?;
            if let Some(goal) = observed.set().status.as_ref().and_then(|status| {
                status.replica_goals.iter().find(|entry| {
                    matches!(
                        entry.goal.desired,
                        ReplicaGoalKind::Reconfigure {
                            role: ReplicaRole::Primary,
                            ..
                        }
                    ) && entry.goal.scope.configuration.previous.as_ref()
                        == Some(&predecessor.current)
                })
            }) {
                ensure!(
                    goal.goal
                        .scope
                        .configuration
                        .current
                        .members
                        .iter()
                        .any(|member| member.identity == *target_ref.identity())
                );
                break;
            }
        }
        eprintln!("actual-v1: separate source joint admission retained");
        let topology = harness.cluster.settle_members(2).await?;
        let observed = harness.cluster.observe().await?;
        let installed = observed
            .snapshot(topology.primary())
            .and_then(|snapshot| snapshot.publication.as_ref())
            .map(|publication| &publication.scope.configuration)
            .context("admitted configuration missing")?;
        ensure!(
            installed.previous.is_none()
                && installed
                    .current
                    .members
                    .iter()
                    .any(|member| member.identity == *target_ref.identity())
        );
        harness
            .cluster
            .probe(source.replicate_put("after-admission".into(), "replicated".into()))
            .await?;
        harness
            .assert_values_on_members("copied-before-build", "nonempty", 2)
            .await?;
        harness
            .assert_values_on_members("after-admission", "replicated", 2)
            .await?;
        ensure!(
            target.get("after-admission").await.is_err()
                && target
                    .replicate_put("forbidden-secondary".into(), "write".into())
                    .await
                    .is_err(),
            "voter admission bypassed primary-only client fences"
        );
        Ok(())
    }
    .await;
    if let Some(gate) = held {
        gate.release()?;
    }
    harness.finish(result).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn actual_v1_controller_terminal_success_joins_before_public_success() -> Result<()> {
    actual_v1_controller_terminal(0).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn actual_v1_controller_terminal_child_failure_retains_failed_completion() -> Result<()> {
    actual_v1_controller_terminal(1).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn actual_v1_controller_terminal_lost_native_join_never_certifies_containment() -> Result<()>
{
    actual_v1_controller_terminal(2).await
}

async fn actual_v1_controller_terminal(fault: u8) -> Result<()> {
    use kuberic_runtime::protocol::public_operations::ReplicaGoalStatus;
    use kuberic_runtime::protocol::types::AccessStatus;
    let mut harness = Harness::with_members(1).await?;
    let result = async {
        let primary_id = harness.ready_members(1).await?;
        let primary = harness.reference(primary_id).await?;
        let service = harness.cluster.service(&primary).await?;
        harness.cluster.probe(service.replicate_put("terminal-tail".into(), "acknowledged".into())).await?;
        if fault == 1 {
            // Negative setup affects the real application's child Close callback.
            service.fail_next_close();
        } else if fault == 2 {
            // Negative setup loses the actual native driver's retained JoinHandle task.
            let observed = harness.cluster.observe().await?;
            let report = observed.snapshot(&primary).context("ordinary snapshot missing")?;
            kuberic_runtime::replicator::TestingTcpRoute::lose_native_driver(
                &report.process_session_id, &report.creation_id,
            ).await?;
        }
        let terminal = harness.cluster.retain_ordinary_terminal(&primary, PublicV1CallKind::Close).await?;
        let (completion, writer) = loop {
            harness.cluster.step().await?;
            let observed = harness.cluster.observe().await?;
            let snapshot = observed.snapshot(&primary).context("terminal snapshot missing")?;
            if let Some(completion) = snapshot.goals.iter().find(|entry| entry.goal == terminal)
                && completion.status != ReplicaGoalStatus::Pending {
                ensure!(snapshot.publication.is_none() && snapshot.withdrawn);
                ensure!(snapshot.read_status != AccessStatus::Granted && snapshot.write_status != AccessStatus::Granted);
                break (completion.clone(), snapshot.provider_writer_held);
            }
        };
        if fault == 0 {
            ensure!(completion.status == ReplicaGoalStatus::Achieved && !writer);
        } else {
            ensure!(writer);
            ensure!(matches!(&completion.status, ReplicaGoalStatus::Failed(detail)
                if detail.contains(if fault == 1 { "injected KV application Close failure" } else { "facade worker join" })), "{completion:?}");
        }
        ensure!(service.get("terminal-tail").await.is_err()
            && service.replicate_put("late-terminal".into(), "forbidden".into()).await.is_err());
        harness.cluster.recreate_controller().await?;
        for _ in 0..3 { harness.cluster.step().await?; }
        let observed = harness.cluster.observe().await?;
        let retry = observed.snapshot(&primary).context("retained terminal snapshot missing")?;
        ensure!(retry.goals.iter().any(|entry| entry == &completion)
            && retry.read_status != AccessStatus::Granted && retry.write_status != AccessStatus::Granted
            && retry.provider_writer_held == writer && retry.publication.is_none(),
            "recreated controller changed terminal result or restored authority");
        Ok(())
    }.await;
    if fault != 0 {
        let passed = result.is_ok();
        let finished = harness.finish(result).await;
        ensure!(passed, "terminal failure assertions failed: {finished:?}");
        ensure!(
            finished.is_err(),
            "failed actual join was hidden by teardown"
        );
        Ok(())
    } else {
        harness.finish(result).await
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn actual_v1_controller_undeclared_role_failure_is_not_retried() -> Result<()> {
    use kuberic_runtime::protocol::public_operations::{ReplicaGoalKind, ReplicaGoalStatus};
    use kuberic_runtime::protocol::types::{AccessStatus, ReplicaRole};
    let mut harness = Harness::with_members(1).await?;
    let result = async {
        harness.ready_members(1).await?;
        harness.cluster.scale_to(2).await?;
        let target = loop {
            let observed = harness.cluster.observe().await?;
            if let Some(target) = observed.replicas().find(|reference| reference.replica_id() == 2).cloned() { break target; }
            harness.cluster.step().await?;
        };
        let service = harness.cluster.service(&target).await?;
        let before = service.role_callback_count();
        // Negative setup only; actual controller dispatch invokes the real callback.
        service.fail_next_role();
        let first = loop {
            harness.cluster.step().await?;
            let observed = harness.cluster.observe().await?;
            let snapshot = observed.snapshot(&target).context("Role target missing")?;
            if snapshot.goals.iter().any(|entry| matches!(entry.goal.desired, ReplicaGoalKind::Prepare { role: ReplicaRole::IdleSecondary })
                && matches!(&entry.status, ReplicaGoalStatus::Failed(detail) if detail.contains("injected undeclared KV application Role failure"))) {
                break snapshot.clone();
            }
        };
        ensure!(
            service.role_callback_count() == before + 1,
            "actual retained Role did not execute once"
        );
        ensure!(
            first.withdrawn && first.publication.is_none()
                && first.read_status != AccessStatus::Granted
                && first.write_status != AccessStatus::Granted
        );
        harness.cluster.recreate_controller().await?;
        for _ in 0..3 { harness.cluster.step().await?; }
        let observed = harness.cluster.observe().await?;
        let second = observed.snapshot(&target).context("Role target missing")?;
        ensure!(
            second.goals == first.goals
                && service.role_callback_count() == before + 1,
            "undeclared application failure re-entered its failed callback"
        );
        ensure!(
            service.get("not-serving").await.is_err()
                && service
                    .replicate_put("not-serving".into(), "forbidden".into())
                    .await
                    .is_err()
        );
        Ok(())
    }
    .await;
    harness.finish(result).await
}

async fn delete_secondary(harness: &mut Harness) -> Result<Vec<(Kind, String, String)>> {
    let old = harness.report(2).await?;
    let uid = old.replica.instance_id.as_str();
    let pod = harness
        .api
        .snapshot(Kind::Pod)
        .into_iter()
        .find(|pod| pod["metadata"]["uid"].as_str() == Some(uid))
        .context("old Pod missing")?;
    let pvc_name = pod["spec"]["volumes"]
        .as_array()
        .context("volumes missing")?
        .iter()
        .find_map(|volume| volume["persistentVolumeClaim"]["claimName"].as_str())
        .context("claim missing")?;
    let mut resources = vec![(
        Kind::Pod,
        pod["metadata"]["name"].as_str().unwrap().to_owned(),
        uid.to_owned(),
    )];
    let pvc = harness
        .api
        .get(Kind::Pvc, pvc_name)
        .context("old PVC missing")?;
    resources.push((
        Kind::Pvc,
        pvc_name.to_owned(),
        pvc["metadata"]["uid"].as_str().unwrap().to_owned(),
    ));
    for service in harness.api.snapshot(Kind::Service) {
        if service["spec"]["selector"][kuberic_controller::crd::INSTANCE_LABEL].as_str()
            == Some(uid)
        {
            resources.push((
                Kind::Service,
                service["metadata"]["name"].as_str().unwrap().to_owned(),
                service["metadata"]["uid"].as_str().unwrap().to_owned(),
            ));
        }
    }
    ensure!(
        resources.iter().any(|(kind, _, _)| *kind == Kind::Service),
        "endpoint Service missing"
    );
    let reference = harness.reference(2).await?;
    harness.cluster.delete_replica(&reference).await?;
    Ok(resources)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn supporting_resources_are_repaired_without_losing_quorum() -> Result<()> {
    let mut harness = Harness::new().await?;
    let result = async {
        harness.write("repair-before", "acknowledged").await?;
        harness
            .assert_values("repair-before", "acknowledged")
            .await?;
        harness.refresh().await?;
        let old_service: Service = serde_json::from_value(
            harness
                .api
                .get(Kind::Service, "sample-peer")
                .context("peer missing")?,
        )?;
        let old_secret: Secret = serde_json::from_value(
            harness
                .api
                .get(Kind::Secret, "sample-agent-credentials")
                .context("credentials missing")?,
        )?;
        ensure!(harness.cluster.observe().await?.credentials_consistent());
        harness
            .cluster
            .delete_support_resource(SupportResource::PeerService)
            .await?;
        harness
            .cluster
            .delete_support_resource(SupportResource::CredentialsSecret)
            .await?;
        loop {
            harness.step().await?;
            if let (Some(service), Some(secret)) = (
                harness
                    .api
                    .get(Kind::Service, "sample-peer")
                    .map(serde_json::from_value::<Service>)
                    .transpose()?,
                harness
                    .api
                    .get(Kind::Secret, "sample-agent-credentials")
                    .map(serde_json::from_value::<Secret>)
                    .transpose()?,
            ) && service.uid() != old_service.uid()
                && secret.uid() != old_secret.uid()
            {
                let set = harness.set().await?;
                for metadata in [&service.metadata, &secret.metadata] {
                    ensure!(
                        metadata.owner_references.as_ref()
                            == Some(&vec![
                                set.controller_owner_ref(&()).context("owner missing")?
                            ])
                    );
                    let labels = metadata.labels.as_ref().context("labels missing")?;
                    ensure!(
                        labels.get(kuberic_controller::crd::SET_UID_LABEL)
                            == set.metadata.uid.as_ref()
                    );
                    ensure!(
                        labels.get("app.kubernetes.io/name").map(String::as_str) == Some("kuberic")
                    );
                    ensure!(
                        labels
                            .get("app.kubernetes.io/managed-by")
                            .map(String::as_str)
                            == Some("kuberic-controller")
                    );
                    ensure!(
                        labels
                            .get("operator.kuberic.io/set-name")
                            .map(String::as_str)
                            == Some("sample")
                    );
                }
                let spec = service.spec.context("service spec missing")?;
                ensure!(
                    spec.cluster_ip.as_deref() == Some("None")
                        && spec.publish_not_ready_addresses == Some(true)
                );
                ensure!(
                    spec.selector
                        == Some(std::collections::BTreeMap::from([(
                            kuberic_controller::crd::SET_UID_LABEL.to_owned(),
                            set.uid().unwrap()
                        )]))
                );
                let ports = spec.ports.context("ports missing")?;
                ensure!(ports.len() == 2);
                for (name, port) in [("control", 50051), ("replication", 50052)] {
                    ensure!(ports.iter().any(|p| p.name.as_deref() == Some(name)
                        && p.port == port
                        && p.target_port
                            == Some(
                                k8s_openapi::apimachinery::pkg::util::intstr::IntOrString::Int(
                                    port
                                )
                            )));
                }
                ensure!(secret.type_.as_deref() == Some("Opaque"));
                ensure!(
                    harness.cluster.observe().await?.credentials_consistent(),
                    "repair changed startup credential contract"
                );
                break;
            }
        }
        harness
            .assert_values("repair-before", "acknowledged")
            .await?;
        harness.write("repair-after", "quorum").await?;
        harness.assert_values("repair-after", "quorum").await
    }
    .await;
    harness.finish(result).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn replacement_then_restart_publishes_exact_retained_storage_session() -> Result<()> {
    let mut harness = Harness::new().await?;
    let result = async {
        harness.write("combined-before", "acknowledged").await?;
        harness
            .assert_values("combined-before", "acknowledged")
            .await?;
        let old = harness.report(2).await?;
        let old_root = harness.data_root(old.replica.instance_id.as_str()).await?;
        let resources = delete_secondary(&mut harness).await?;
        let _pending_replacement = loop {
            if let Some(replacement) = harness
                .set()
                .await?
                .status
                .and_then(|s| s.public_v1_replacement)
            {
                break replacement;
            }
            harness.step().await?;
        };
        harness.ready().await?;
        let replacement = harness.report(2).await?;
        let replacement_root = harness
            .data_root(replacement.replica.instance_id.as_str())
            .await?;
        ensure!(!old_root.exists(), "predecessor storage directory remains");
        ensure!(replacement_root.exists() && replacement_root != old_root);
        ensure!(
            harness.api.snapshot(Kind::Service).iter().all(|service| {
                service["spec"]["selector"][kuberic_controller::crd::INSTANCE_LABEL].as_str()
                    != Some(old.replica.instance_id.as_str())
            }),
            "a Service still selects the predecessor"
        );
        ensure!(
            old.replica != replacement.replica
                && old.storage_incarnation != replacement.storage_incarnation
        );
        for (kind, name, uid) in resources {
            ensure!(
                harness
                    .api
                    .snapshot(kind)
                    .iter()
                    .all(|object| object["metadata"]["uid"].as_str() != Some(&uid)),
                "predecessor {name}/{uid} remains"
            );
        }
        harness
            .assert_values("combined-before", "acknowledged")
            .await?;
        harness.write("combined-middle", "replacement").await?;
        harness
            .assert_values("combined-middle", "replacement")
            .await?;
        harness.stop(2, true).await?;
        harness.ready().await?;
        let restarted = harness.report(2).await?;
        ensure!(
            harness
                .data_root(restarted.replica.instance_id.as_str())
                .await?
                == replacement_root
                && replacement_root.exists()
        );
        ensure!(
            restarted.replica == replacement.replica
                && restarted.storage_incarnation == replacement.storage_incarnation
        );
        ensure!(
            restarted.process_session_id != replacement.process_session_id
                && restarted.replicator_creation_id != replacement.replicator_creation_id
        );
        let topology = harness
            .set()
            .await?
            .status
            .unwrap()
            .public_v1_topology
            .unwrap();
        let admitted = topology
            .sessions
            .iter()
            .find(|session| session.identity == restarted.replica)
            .context("replacement not admitted")?;
        ensure!(
            admitted.process_session_id == restarted.process_session_id
                && admitted.replicator_creation_id == restarted.replicator_creation_id
                && admitted.storage_incarnation == restarted.storage_incarnation
                && admitted.role == restarted.role
        );
        harness
            .assert_values("combined-before", "acknowledged")
            .await?;
        harness
            .assert_values("combined-middle", "replacement")
            .await?;
        harness.write("combined-after", "restarted").await?;
        harness.assert_values("combined-after", "restarted").await
    }
    .await;
    harness.finish(result).await
}

async fn assert_exact_completion(
    harness: &mut Harness,
    persisted: &serde_json::Value,
) -> Result<PublicV1AcceptedOperationCompletion> {
    let accepted: PublicV1OperationStatus =
        serde_json::from_value(persisted["status"]["publicV1Operation"].clone())?;
    let command = accepted.command().map_err(anyhow::Error::msg)?;
    let transition: kuberic_controller::crd::PublicV1TransitionState =
        serde_json::from_value(persisted["status"]["publicV1Transition"].clone())?;
    let frozen = transition.transition().map_err(anyhow::Error::msg)?.intent;
    let report = harness
        .report_uid(command.target.instance_id.as_str())
        .await?;
    ensure!(
        report.replica == command.target,
        "completion target changed"
    );
    let completions = report
        .completed_operations
        .iter()
        .filter(|completion| {
            completion.operation_id == command.intent.operation_id
                && completion.revision == command.intent.revision
        })
        .collect::<Vec<_>>();
    ensure!(
        completions.len() == 1,
        "expected one exact operation completion"
    );
    let completion = completions[0];
    ensure!(
        completion.process_session_id == command.intent.process_session_id
            && completion.input_digest == command.intent.input_digest
            && completion.replay_disposition == command.intent.replay_disposition
            && completion.call_kind == frozen.call_kind
            && completion.history == frozen.history
            && completion.storage_incarnation == frozen.source.storage_incarnation
            && completion.previous_configuration_id == frozen.previous_configuration_id
            && completion.current_configuration_id == frozen.current_configuration_id
            && completion.catch_up_mode == frozen.catch_up_mode
            && completion.catch_up_specific_quorum == frozen.catch_up_specific_quorum
            && completion.result == PublicV1OperationResult::Succeeded,
        "completion differs from the persisted operation"
    );
    Ok(PublicV1AcceptedOperationCompletion::from_completion(
        completion,
    ))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bootstrap_quorum_write_recovers_status_conflict() -> Result<()> {
    let mut harness = Harness::new().await?;
    let result = async {
        let mut conflict = harness.cluster.completion_status_conflict()?;
        let (lsn, persisted) =
            tokio::try_join!(harness.write("bootstrap", "committed"), async move {
                let persisted = serde_json::to_value(conflict.entered().await?.set())?;
                conflict.release();
                Ok::<_, anyhow::Error>(persisted)
            })?;
        ensure!(lsn > 0, "write did not allocate an LSN");
        harness.assert_values("bootstrap", "committed").await?;
        let mut addresses = std::collections::BTreeSet::new();
        for id in 1..=3 {
            let reference = harness.reference(id).await?;
            let observed = harness.cluster.observe().await?;
            let address = observed
                .snapshot(&reference)
                .and_then(|snapshot| snapshot.publication.as_ref())
                .map(|publication| publication.replication_address.clone())
                .context("Open endpoint missing")?;
            let endpoint: http::Uri = address.parse()?;
            ensure!(endpoint.host() == Some("127.0.0.1"));
            ensure!(endpoint.port_u16().is_some_and(|port| port != 0));
            ensure!(
                addresses.insert(address),
                "replicas share a replication endpoint"
            );
        }
        ensure!(persisted["metadata"]["resourceVersion"].is_string());
        ensure!(
            harness.reconcile_errors > 0,
            "injected conflict was not retried"
        );
        let pending =
            serde_json::from_value::<kuberic_controller::crd::KubericSet>(persisted.clone())?
                .status
                .context("conflict status missing")?
                .replica_goals
                .into_iter()
                .filter(|entry| entry.outcome.is_none())
                .collect::<Vec<_>>();
        ensure!(
            !pending.is_empty(),
            "conflict lost its pending ordinary goals"
        );
        let observed = harness.cluster.observe().await?;
        for pending in pending {
            pending.validate().map_err(anyhow::Error::msg)?;
            let retained = observed
                .set()
                .status
                .as_ref()
                .context("settled status missing")?
                .replica_goals
                .iter()
                .find(|entry| entry.goal == pending.goal)
                .context("conflicted ordinary goal was lost")?;
            retained.validate().map_err(anyhow::Error::msg)?;
            ensure!(retained.digest == pending.digest);
            ensure!(retained.outcome.as_ref().is_some_and(|outcome| {
                outcome.goal == pending.goal
                    && outcome.status
                        == kuberic_runtime::protocol::public_operations::ReplicaGoalStatus::Achieved
            }));
        }
        Ok(())
    }
    .await;
    harness.finish(result).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn contained_restart_renews_session_and_dynamic_endpoints() -> Result<()> {
    let mut harness = Harness::new().await?;
    let result = async {
        harness.write("restart", "before").await?;
        harness.assert_values("restart", "before").await?;
        let before = harness.report(2).await?;
        let before_ref = harness.reference(2).await?;
        let old_endpoint: http::Uri = before_ref
            .replication_address()
            .context("pre-restart Open endpoint missing")?
            .parse()?;
        let old_service = harness.service(2).await?;
        harness.stop(2, true).await?;
        ensure!(
            old_service.get("restart").await.is_err(),
            "stopped service retained access"
        );
        harness.ready().await?;
        let after = harness.report(2).await?;
        ensure!(
            before.replica == after.replica,
            "restart changed Pod identity"
        );
        ensure!(
            before.storage_incarnation == after.storage_incarnation,
            "restart changed storage identity"
        );
        ensure!(
            before.process_session_id != after.process_session_id,
            "restart reused process session"
        );
        ensure!(
            before.replicator_creation_id != after.replicator_creation_id,
            "restart reused replicator creation"
        );
        let after_ref = harness.reference(2).await?;
        let new_endpoint: http::Uri = after_ref
            .replication_address()
            .context("post-restart Open endpoint missing")?
            .parse()?;
        ensure!(new_endpoint.host() == old_endpoint.host());
        ensure!(new_endpoint.port_u16().is_some_and(|port| port != 0));
        harness.assert_values("restart", "before").await?;
        harness.write("restart", "after").await?;
        harness.assert_values("restart", "after").await?;
        Ok(())
    }
    .await;
    harness.finish(result).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn secondary_replacement_waits_for_exact_resource_absence() -> Result<()> {
    let mut harness = Harness::new().await?;
    let result = async {
        harness.write("replacement", "before").await?;
        harness.assert_values("replacement", "before").await?;
        let old = harness.report(2).await?;
        let uid = old.replica.instance_id.to_string();
        let pod: Pod = harness
            .api
            .snapshot(Kind::Pod)
            .into_iter()
            .map(serde_json::from_value)
            .collect::<std::result::Result<Vec<Pod>, _>>()?
            .into_iter()
            .find(|pod| pod.uid().as_deref() == Some(&uid))
            .context("old Pod missing")?;
        let resource = harness.api.reference(Kind::Pod, &uid)?;
        let held = harness.cluster.hold_deletion(&resource).await?;
        let reference = harness.reference(2).await?;
        harness.cluster.delete_replica(&reference).await?;
        loop {
            let set = harness.set().await?;
            let status = set.status.as_ref().context("status missing")?;
            if let Some(topology) = &status.public_v1_topology {
                let configuration = topology
                    .configuration()
                    .map_err(|error| anyhow::anyhow!("{error}"))?;
                if configuration.members.iter().any(|member| {
                    member.identity.replica_id.value() == 2
                        && member.identity.instance_id.as_str() != uid
                }) {
                    ensure!(
                        status.public_v1_replacement.is_some(),
                        "cleanup authority cleared before exact absence"
                    );
                    ensure!(
                        harness.api.get(Kind::Pod, &pod.name_any()).is_some(),
                        "held predecessor disappeared"
                    );
                    break;
                }
            }
            harness.step().await?;
        }
        drop(held);
        harness.ready().await?;
        let replacement = harness.report(2).await?;
        ensure!(
            replacement.replica != old.replica,
            "replacement reused predecessor incarnation"
        );
        ensure!(
            replacement.storage_incarnation != old.storage_incarnation,
            "replacement reused predecessor storage"
        );
        ensure!(
            harness.api.get(Kind::Pod, &pod.name_any()).is_none(),
            "predecessor Pod remains"
        );
        harness.assert_values("replacement", "before").await?;
        harness.write("replacement", "after").await?;
        harness.assert_values("replacement", "after").await?;
        Ok(())
    }
    .await;
    harness.finish(result).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stopped_primary_failover_preserves_acknowledged_data() -> Result<()> {
    stopped_primary_failover(false).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retained_commit_observer_does_not_block_stopped_primary_failover() -> Result<()> {
    recovery_before_lifecycle(RecoveryLifecycle::PrimaryFailover).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retained_commit_observer_does_not_reuse_a_restarted_peer_session() -> Result<()> {
    recovery_before_lifecycle(RecoveryLifecycle::PeerRestart).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retained_commit_observer_does_not_retire_the_primary_after_peer_deletion() -> Result<()> {
    recovery_before_lifecycle(RecoveryLifecycle::PeerReplacement).await
}

enum RecoveryLifecycle {
    PrimaryFailover,
    PeerRestart,
    PeerReplacement,
}

async fn recovery_before_lifecycle(lifecycle: RecoveryLifecycle) -> Result<()> {
    use kuberic_controller::test_util::TestCluster;
    let mut cluster = TestCluster::builder(super::harness::KvFactory)
        .image("in-process/kvstore2")
        .scenario_timeout(std::time::Duration::from_secs(45))
        .start()
        .await?;
    let mut writes = tokio::task::JoinSet::new();
    let mut phase = "initial topology";
    let result: Result<()> = tokio::time::timeout(std::time::Duration::from_secs(90), async {
        let topology = cluster.settle().await?;
        phase = "acknowledged prefix and pending service commitment";
        let primary = topology.primary().clone();
        let secondary = topology.secondaries().next().unwrap().clone();
        let writer = cluster.service(&primary).await?;
        let baseline = cluster
            .probe(writer.replicate_put("retained-prefix".into(), "acknowledged".into()))
            .await?;
        let observer = writer.clone();
        let mut gate = writer.pause_next_commit()?;
        writes.spawn(async move {
            writer
                .replicate_put("accepted-recovery".into(), "acknowledged".into())
                .await
        });
        cluster
            .probe(async {
                gate.entered().await;
                Ok::<_, anyhow::Error>(())
            })
            .await?;
        ensure!(observer.get("accepted-recovery").await?.is_none());
        ensure!(
            tokio::time::timeout(std::time::Duration::from_millis(50), writes.join_next())
                .await
                .is_err(),
            "native completion cannot prematurely acknowledge local commitment"
        );
        let failed_primary = matches!(lifecycle, RecoveryLifecycle::PrimaryFailover);
        let settled = match lifecycle {
            RecoveryLifecycle::PeerRestart => {
                phase = "peer restart";
                let fresh = {
                    let restart = cluster.restart(&secondary);
                    tokio::pin!(restart);
                    if let Ok(result) =
                        tokio::time::timeout(std::time::Duration::from_millis(75), &mut restart)
                            .await
                    {
                        result?;
                        bail!("cold restart bypassed the pending service-owned committed prefix");
                    }
                    gate.release();
                    let lsn = tokio::time::timeout(
                        std::time::Duration::from_secs(10),
                        writes.join_next(),
                    )
                    .await?
                    .context("writer missing")???;
                    ensure!(lsn > baseline);
                    restart.await?
                };
                ensure!(fresh.process_session_id() != secondary.process_session_id());
                ensure!(
                    fresh.identity() == secondary.identity()
                        && fresh.storage_incarnation() == secondary.storage_incarnation(),
                    "a contained restart became a corrective storage replacement"
                );
                cluster.settle().await?
            }
            RecoveryLifecycle::PrimaryFailover => {
                phase = "primary stop/failover";
                cluster.stop(&primary).await?;
                gate.release();
                ensure!(
                    cluster
                        .probe(async {
                            Ok::<_, anyhow::Error>(
                                writes.join_next().await.context("writer missing")??,
                            )
                        })
                        .await?
                        .is_err(),
                    "Abort must fail the retained local commit observer"
                );
                ensure!(observer.get("accepted-recovery").await.is_err());
                let topology = cluster.settle_members(2).await?;
                ensure!(topology.primary().identity() != primary.identity());
                topology
            }
            RecoveryLifecycle::PeerReplacement => {
                phase = "peer deletion during accepted recovery";
                cluster.delete_replica(&secondary).await?;
                gate.release();
                ensure!(
                    cluster
                        .probe(async {
                            Ok::<_, anyhow::Error>(
                                writes.join_next().await.context("writer missing")???,
                            )
                        })
                        .await?
                        > baseline
                );
                loop {
                    let observation = cluster.observe().await?;
                    if !observation
                        .resources()
                        .iter()
                        .any(|resource| resource.reference().uid() == secondary.instance_id())
                    {
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                }
                let topology = cluster.settle().await?;
                ensure!(
                    topology.primary() == &primary,
                    "a lost peer caused retirement of the still-current primary"
                );
                ensure!(cluster.service(&secondary).await.is_err());
                topology
            }
        };
        for reference in settled.members() {
            let service = cluster.service(reference).await?;
            if reference == settled.primary() {
                ensure!(
                    cluster
                        .probe(service.get("retained-prefix"))
                        .await?
                        .as_deref()
                        == Some("acknowledged")
                );
                if !failed_primary {
                    ensure!(
                        cluster
                            .probe(service.get("accepted-recovery"))
                            .await?
                            .as_deref()
                            == Some("acknowledged")
                    );
                }
            } else {
                ensure!(
                    service.persistence().get("retained-prefix").as_deref() == Some("acknowledged")
                );
            }
        }
        ensure!(
            cluster
                .observe()
                .await?
                .set()
                .status
                .as_ref()
                .is_some_and(|status| status.public_v1_operation.is_none()
                    && status.public_v1_transition.is_none()
                    && status.public_v1_session_renewal.is_none())
        );
        Ok(())
    })
    .await
    .context("bounded retained service commit/lifecycle journey")
    .and_then(|outcome| outcome);
    writes.shutdown().await;
    if result.is_err()
        && let Ok(observation) = cluster.observe().await
    {
        eprintln!(
            "retained observer pre-cleanup snapshot: {}",
            serde_json::json!({
                "set": observation.set(),
                "reconcileErrors": observation.reconcile_errors(),
                "agents": observation.replicas()
                    .map(|reference| observation.report(reference)).collect::<Vec<_>>(),
            })
        );
    }
    if let Err(error) = cluster.finish(result).await {
        eprintln!("accepted recovery failure phase: {phase}");
        if let Some(diagnostics) = error.diagnostics() {
            eprintln!("accepted recovery diagnostics: {diagnostics}");
        }
        return Err(error.into());
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unaccepted_switchover_does_not_block_unavailable_primary_failover() -> Result<()> {
    stopped_primary_failover(true).await
}

async fn stopped_primary_failover(request_transfer: bool) -> Result<()> {
    let mut harness = Harness::new().await?;
    let result = async {
        harness.write("failover", "before").await?;
        harness.assert_values("failover", "before").await?;
        let primary = harness.ready().await?;
        let old_service = harness.service(primary).await?;
        harness.stop(primary, false).await?;
        let target = (1..=3).find(|id| *id != primary).unwrap();
        let request = if request_transfer {
            let request = harness.submit(target).await?;
            let set = harness.set().await?;
            ensure!(
                set.status
                    .unwrap()
                    .public_v1_topology
                    .unwrap()
                    .pending_switchover
                    .is_none()
            );
            Some(request)
        } else {
            None
        };
        let promoted = harness.ready_members(2).await?;
        ensure!(promoted != primary, "failover did not promote a survivor");
        if request_transfer {
            let set = harness.set().await?;
            ensure!(
                    promoted == target
                        && set.spec.switchover.as_ref().is_some_and(
                            |current| current.request_id == request.as_ref().unwrap().id()
                        )
                );
            ensure!(
                set.status
                    .unwrap()
                    .public_v1_topology
                    .unwrap()
                    .completed_switchover_request_id
                    .as_deref()
                    == Some(request.as_ref().unwrap().id())
            );
        }
        ensure!(
            old_service
                .replicate_put("stale".into(), "rejected".into())
                .await
                .is_err(),
            "stopped primary accepted a write"
        );
        harness
            .assert_values_on_members("failover", "before", 2)
            .await?;
        harness
            .service(promoted)
            .await?
            .replicate_put("failover".into(), "after".into())
            .await?;
        harness
            .assert_values_on_members("failover", "after", 2)
            .await?;
        Ok(())
    }
    .await;
    harness.finish(result).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn controller_recreation_resumes_committed_operation() -> Result<()> {
    use kuberic_runtime::protocol::public_operations::ReplicaGoalStatus;

    let mut harness = Harness::new().await?;
    let result = async {
        let pause = harness
            .cluster
            .pause_at(Barrier::Operation {
                stage: BarrierStage::Intent,
                kind: None,
                participant: None,
            })
            .await?;
        let pending = pause
            .evidence()
            .set()
            .status
            .as_ref()
            .context("barrier status missing")?
            .replica_goals
            .iter()
            .filter(|entry| entry.outcome.is_none())
            .cloned()
            .collect::<Vec<_>>();
        ensure!(
            !pending.is_empty(),
            "barrier did not capture an accepted ordinary goal"
        );
        for selected in &pending {
            selected.validate().map_err(anyhow::Error::msg)?;
        }
        harness.recreate_controller().await?;
        harness.cluster.resume(pause).await?;
        let persisted = harness.set().await?.status.context("status missing")?;
        for selected in &pending {
            let retained = persisted
                .replica_goals
                .iter()
                .find(|entry| entry.goal == selected.goal)
                .context("controller recreation replaced the accepted goal")?;
            retained.validate().map_err(anyhow::Error::msg)?;
            ensure!(
                retained.digest == selected.digest,
                "controller recreation changed accepted authority"
            );
        }
        harness.write("controller", "resumed").await?;
        harness.assert_values("controller", "resumed").await?;
        let observed = harness.cluster.observe().await?;
        let status = observed
            .set()
            .status
            .as_ref()
            .context("settled status missing")?;
        for selected in pending {
            let retained = status
                .replica_goals
                .iter()
                .find(|entry| entry.goal == selected.goal)
                .context("accepted ordinary goal was lost")?;
            retained.validate().map_err(anyhow::Error::msg)?;
            ensure!(retained.digest == selected.digest);
            ensure!(retained.outcome.as_ref().is_some_and(|outcome| {
                outcome.goal == selected.goal && outcome.status == ReplicaGoalStatus::Achieved
            }));
            let (_, snapshot) = observed
                .snapshots()
                .find(|(_, snapshot)| {
                    snapshot.storage == selected.goal.scope.storage
                        && snapshot.process_session_id == selected.goal.scope.process_session_id
                })
                .context("accepted goal's live owner missing")?;
            let receipts = snapshot
                .goals
                .iter()
                .filter(|entry| entry.goal.operation_id == selected.goal.operation_id)
                .collect::<Vec<_>>();
            ensure!(
                matches!(receipts.as_slice(), [entry]
                    if entry.goal == selected.goal && entry.status == ReplicaGoalStatus::Achieved),
                "accepted goal did not retain one exact successful result"
            );
        }
        Ok(())
    }
    .await;
    harness.finish(result).await
}

async fn reject_write(service: &kvstore2::service::KvService, key: &str) -> Result<()> {
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(12),
        service.replicate_put(key.into(), "ambiguous-attempt".into()),
    )
    .await;
    if key == "recreation-stale-isolated" {
        eprintln!("recreation isolated predecessor write observer: {result:?}");
    }
    ensure!(
        !matches!(result, Ok(Ok(_))),
        "unsafe write acknowledged: {key}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn live_primary_isolation_fences_retained_observer_before_safe_rejoin() -> Result<()> {
    live_primary_isolation(true).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn renewed_primary_rejoins_with_service_owned_committed_prefix() -> Result<()> {
    live_primary_isolation(false).await
}

async fn live_primary_isolation(attempt_isolated_writes: bool) -> Result<()> {
    let mut harness = Harness::new().await?;
    let mut writes = tokio::task::JoinSet::new();
    let result = tokio::time::timeout(std::time::Duration::from_secs(90), async {
        harness.write("live-before", "acknowledged").await?;
        harness.assert_values("live-before", "acknowledged").await?;
        let before_restart = harness.report(2).await?;
        eprintln!("native isolation: retained secondary restart");
        harness.stop(2, true).await?;
        harness.ready().await?;
        eprintln!("native isolation: physical primary partition");
        ensure!(
            harness.report(2).await?.process_session_id != before_restart.process_session_id,
            "secondary did not renew its session before failover"
        );
        let primary = harness.ready().await?;
        let old = harness.report(primary).await?;
        let uid = old.replica.instance_id.to_string();
        let service = harness.service(primary).await?;
        use kuberic_runtime::engine::DurableState;
        let committed = service.persistence().durable_progress().await?;
        ensure!(
            committed.applied_lsn == committed.committed_lsn
                && matches!(
                    &old.progress.current_progress,
                    kuberic_runtime::protocol::public_operations::ProgressQueryResult::NotQueried(_)
                )
                && service.get("live-before").await?.as_deref() == Some("acknowledged"),
            "failover requires the service-owned committed view; ordinary reports must not sample progress callbacks"
        );
        let isolation = harness.isolate_uids(std::slice::from_ref(&uid)).await?;
        let promoted = harness.ready_survivors(2, &isolation).await?;
        eprintln!("native isolation: survivor promotion completed");
        ensure!(promoted != primary);
        let before_return = harness.report(promoted).await?.completed_operations;
        harness
            .service(promoted)
            .await?
            .replicate_put("live-after".into(), "new-primary".into())
            .await?;
        if attempt_isolated_writes {
            let writer = service.clone();
            writes.spawn(async move {
                writer
                    .replicate_put("live-stale-isolated".into(), "ambiguous-attempt".into())
                    .await
            });
            if let Ok(result) =
                tokio::time::timeout(std::time::Duration::from_millis(150), writes.join_next())
                    .await
            {
                ensure!(
                    result.context("isolated observer missing")??.is_err(),
                    "isolated old primary granted client success"
                );
            }
        }
        ensure!(harness.live_uids().await.contains(&uid));
        harness.heal(isolation).await?;
        eprintln!("native isolation: primary healing and retirement");
        harness.ready().await?;
        let settled = harness.report(primary).await?;
        ensure!(settled.role == kuberic_runtime::protocol::types::ReplicaRole::ActiveSecondary);
        let same_owner = settled.process_session_id == old.process_session_id
            && settled.replicator_creation_id == old.replicator_creation_id
            && settled.replica == old.replica;
        let source = harness.report(promoted).await?;
        let rebuilt = source.completed_operations.iter().any(|completion| {
            completion.call_kind == PublicV1CallKind::Build
                && !before_return
                    .iter()
                    .any(|previous| previous.operation_id == completion.operation_id)
        });
        if rebuilt {
            ensure!(
                !same_owner,
                "native successor copy must replace the process, never reset an old receiver"
            );
        } else if same_owner {
            ensure!(
                std::sync::Arc::ptr_eq(&service, &harness.service(primary).await?),
                "retained native owner changed service identity"
            );
        }
        while !writes.is_empty() {
            ensure!(
                tokio::time::timeout(std::time::Duration::from_secs(10), writes.join_next())
                    .await?
                    .context("retained isolated observer missing")??
                    .is_err(),
                "old owner late-granted an isolated admitted write"
            );
        }
        ensure!(
            tokio::time::timeout(
                std::time::Duration::from_secs(2),
                service.replicate_put("live-stale-settled".into(), "forbidden".into())
            )
            .await?
            .is_err(),
            "retired/secondary old owner admitted a new write"
        );
        harness.assert_values("live-before", "acknowledged").await?;
        harness.assert_values("live-after", "new-primary").await?;
        harness.write("live-recovered", "quorum").await?;
        harness.assert_values("live-recovered", "quorum").await
    })
    .await
    .context("bounded native primary isolation and rejoin")
    .and_then(|outcome| outcome);
    let result = harness.finish(result).await;
    writes.shutdown().await;
    result
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn quorum_loss_rejects_acknowledgements_and_recovers_ambiguous_suffix() -> Result<()> {
    let mut harness = Harness::new().await?;
    let result = async {
        harness.write("quorum-before", "acknowledged").await?;
        harness
            .assert_values("quorum-before", "acknowledged")
            .await?;
        let primary = harness.ready().await?;
        let service = harness.service(primary).await?;
        let before = harness
            .set()
            .await?
            .status
            .unwrap()
            .public_v1_topology
            .unwrap();
        let mut isolated = Vec::new();
        for id in 1..=3 {
            if id != primary {
                let uid = harness.report(id).await?.replica.instance_id.to_string();
                isolated.push(uid);
            }
        }
        let isolation = harness.isolate_uids(&isolated).await?;
        reject_write(&service, "quorum-ambiguous").await?;
        for _ in 0..3 {
            harness.step().await?;
            let topology = harness
                .set()
                .await?
                .status
                .unwrap()
                .public_v1_topology
                .unwrap();
            ensure!(
                topology.configuration_id == before.configuration_id && topology.member_count == 3,
                "lone member rewrote quorum authority"
            );
            reject_write(&service, "quorum-lone").await?;
        }
        harness.heal(isolation).await?;
        ensure!(harness.ready().await? == primary);
        harness
            .assert_values("quorum-before", "acknowledged")
            .await?;
        harness.write("quorum-after", "recovered").await?;
        harness.assert_values("quorum-after", "recovered").await?;
        for key in ["quorum-ambiguous", "quorum-lone"] {
            let value = harness.service(primary).await?.get(key).await?;
            ensure!(
                value
                    .as_deref()
                    .is_none_or(|value| value == "ambiguous-attempt")
            );
            for id in 1..=3 {
                use kuberic_runtime::engine::DurableState;
                let service = harness.service(id).await?;
                let committed = service
                    .persistence()
                    .durable_progress()
                    .await?
                    .committed_lsn;
                ensure!(
                    service.persistence().snapshot_at(committed)?.get(key) == value.as_ref(),
                    "ambiguous write diverged"
                );
            }
        }
        Ok(())
    }
    .await;
    harness.finish(result).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sf01_primary_local_commit_isolation_and_abort_never_late_grants_success() -> Result<()> {
    use kuberic_runtime::engine::DurableState;
    use kuberic_runtime::protocol::types::AccessStatus;
    use std::time::Duration;

    let mut harness = Harness::new().await?;
    let result = tokio::time::timeout(Duration::from_secs(90), async {
        eprintln!("SF-01: bootstrap and acknowledged prefix");
        harness.write("sf01-before", "committed").await?;
        harness.assert_values("sf01-before", "committed").await?;
        eprintln!("SF-01: service-local commit barrier");
        let primary = harness.ready().await?;
        let service = harness.service(primary).await?;
        let before = harness.report(primary).await?;
        let mut isolated = Vec::new();
        for id in 1..=3 {
            if id != primary {
                let uid = harness.report(id).await?.replica.instance_id.to_string();
                isolated.push(uid);
            }
        }
        let mut gate = service.pause_next_commit()?;
        let mut writes = tokio::task::JoinSet::new();
        let writer = service.clone();
        writes.spawn(async move {
            writer
                .replicate_put("sf01-visible".into(), "uncommitted".into())
                .await
        });
        tokio::time::timeout(Duration::from_secs(10), gate.entered()).await?;
        ensure!(!writes.is_empty());
        ensure!(
            tokio::time::timeout(Duration::from_millis(50), writes.join_next())
                .await
                .is_err(),
            "client succeeded before service-local commitment"
        );
        let raw = service.persistence().durable_progress().await?;
        ensure!(raw.applied_lsn == 2 && raw.committed_lsn == 1);
        ensure!(
            tokio::time::timeout(Duration::from_secs(2), service.get("sf01-visible"))
                .await??
                .is_none(),
            "speculative data leaked through the committed service view"
        );
        let during = harness.report(primary).await?;
        let mut durably_applied_peers = 0;
        for id in 1..=3 {
            if id != primary
                && harness.service(id).await?.persistence().get("sf01-visible").as_deref()
                    == Some("uncommitted")
            {
                durably_applied_peers += 1;
            }
        }
        ensure!(durably_applied_peers >= 1, "the actual native completion had no durably applied quorum peer");
        ensure!(
            during.process_session_id == before.process_session_id
                && during.replicator_creation_id == before.replicator_creation_id
                && during.progress.current().is_none()
                && during.read_status == AccessStatus::Granted,
            "the actual post-native-completion gate and peer apply must remain distinct from service-local commitment; the ordinary report must not sample progress callbacks"
        );
        eprintln!("SF-01: physical isolation and explicit owner stop");
        let isolation = harness.isolate_uids(&isolated).await?;
        harness.stop(primary, false).await?;
        gate.release();
        ensure!(
            tokio::time::timeout(Duration::from_secs(10), writes.join_next())
                .await?
                .context("writer missing")??
                .is_err()
        );
        ensure!(service.get("sf01-visible").await.is_err());
        ensure!(
            service
                .persistence()
                .durable_progress()
                .await?
                .committed_lsn
                == 1
        );
        ensure!(
            service
                .replicate_put("sf01-late".into(), "forbidden".into())
                .await
                .is_err()
        );
        eprintln!("SF-01: heal and recover acknowledged prefix");
        harness.heal(isolation).await?;
        let topology = harness.cluster.settle_members(2).await?;
        let promoted = topology.primary().replica_id();
        harness.refresh().await?;
        ensure!(promoted != primary);
        harness
            .assert_values_on_members("sf01-before", "committed", 2)
            .await?;
        harness
            .service(promoted)
            .await?
            .replicate_put("sf01-after".into(), "committed".into())
            .await?;
        harness
            .assert_values_on_members("sf01-after", "committed", 2)
            .await
    })
    .await
    .context("bounded SF-01 local commitment/Abort journey")
    .and_then(|outcome| outcome);
    harness.finish(result).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn specification_switchover_preserves_data_and_fences_old_writer() -> Result<()> {
    switchover(None).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn switchover_recreation_after_catch_up_resumes_accepted_handoff() -> Result<()> {
    switchover(Some(false)).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn switchover_recreation_after_durable_handoff_resumes_promotion() -> Result<()> {
    switchover(Some(true)).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn switchover_cancellation_after_peer_preparation_finishes_frozen_request() -> Result<()> {
    changed_switchover(false).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn switchover_retarget_after_peer_preparation_serializes_frozen_requests() -> Result<()> {
    changed_switchover(true).await
}

async fn changed_switchover(retarget: bool) -> Result<()> {
    let mut harness = Harness::new().await?;
    let result =
        async {
            harness.write("frozen-before", "acknowledged").await?;
            harness
                .assert_values("frozen-before", "acknowledged")
                .await?;
            let old = harness.ready().await?;
            let old_service = harness.service(old).await?;
            let target = if old == 2 { 3 } else { 2 };
            let last_peer = (1..=3).find(|id| *id != old && *id != target).unwrap();
            ensure!(target < last_peer, "fixture must prepare target first");
            let initial = harness
                .set()
                .await?
                .status
                .unwrap()
                .public_v1_topology
                .unwrap();
            let request = harness.submit(target).await?;
            let peer = harness.reference(last_peer).await?;
            let pause = harness
                .cluster
                .pause_at(Barrier::SwitchoverPeerConfiguration {
                    request: request.clone(),
                    peer,
                })
                .await?;
            let frozen = serde_json::to_value(pause.evidence().set())?;
            let completed = assert_exact_completion(&mut harness, &frozen).await?;
            let set = harness.set().await?;
            let topology = set
                .status
                .as_ref()
                .unwrap()
                .public_v1_topology
                .as_ref()
                .unwrap();
            ensure!(
                topology
                    .pending_switchover
                    .as_ref()
                    .is_some_and(|pending| pending.request_id == request.id()
                        && i64::from(pending.target_replica_id) == target)
                    && topology.configuration_id == initial.configuration_id
                    && topology.sessions == initial.sessions
            );
            let superseding = if retarget {
                Some(harness.submit(last_peer).await?)
            } else {
                harness.cluster.clear_switchover(&request).await?;
                None
            };
            let updated = harness.set().await?;
            ensure!(
                updated.status == set.status,
                "spec edit changed frozen authority"
            );
            harness.recreate_controller().await?;
            harness.cluster.resume(pause).await?;
            loop {
                let set = harness.set().await?;
                let topology = set
                    .status
                    .as_ref()
                    .unwrap()
                    .public_v1_topology
                    .as_ref()
                    .unwrap();
                if topology.completed_switchover_request_id.as_deref() == Some(request.id()) {
                    ensure!(
                        topology.primary_id.value() == target
                            && topology.pending_switchover.is_none(),
                        "completion identified the edited request instead of the accepted transfer"
                    );
                    for session in &topology.sessions {
                        ensure!(initial.sessions.iter().any(|old| old.identity
                            == session.identity
                            && old.process_session_id == session.process_session_id
                            && old.replicator_creation_id == session.replicator_creation_id
                            && old.storage_incarnation == session.storage_incarnation));
                    }
                    break;
                }
                harness.step().await?;
            }
            ensure!(harness.ready().await? == target);
            let status = harness.set().await?.status.unwrap();
            ensure!(
                status
                    .public_v1_authority
                    .unwrap()
                    .authority()
                    .map_err(anyhow::Error::msg)?
                    .completed_operations
                    .iter()
                    .filter(|entry| **entry == completed)
                    .count()
                    == 1
            );
            reject_write(&old_service, "frozen-stale").await?;
            harness
                .assert_values("frozen-before", "acknowledged")
                .await?;
            harness.write("frozen-after", "continued").await?;
            harness.assert_values("frozen-after", "continued").await?;
            if retarget {
                loop {
                    let set = harness.set().await?;
                    if set
                        .status
                        .as_ref()
                        .unwrap()
                        .public_v1_topology
                        .as_ref()
                        .unwrap()
                        .completed_switchover_request_id
                        .as_deref()
                        == Some(superseding.as_ref().unwrap().id())
                    {
                        break;
                    }
                    harness.step().await?;
                }
                ensure!(harness.ready().await? == last_peer);
                let superseded_service = harness.service(target).await?;
                reject_write(&superseded_service, "superseded-stale").await?;
                harness
                    .assert_values("frozen-before", "acknowledged")
                    .await?;
                harness.assert_values("frozen-after", "continued").await?;
                harness.write("superseding-after", "serialized").await?;
                harness
                    .assert_values("superseding-after", "serialized")
                    .await?;
            }
            Ok(())
        }
        .await;
    harness.finish(result).await
}

async fn switchover(cut: Option<bool>) -> Result<()> {
    let mut harness = Harness::new().await?;
    let result = async {
        harness.write("switch-before", "acknowledged").await?;
        harness
            .assert_values("switch-before", "acknowledged")
            .await?;
        let old = harness.ready().await?;
        let old_service = harness.service(old).await?;
        let mut sessions = Vec::new();
        for uid in harness.live_uids().await {
            let report = harness.report_uid(&uid).await?;
            ensure!(report.extensions.catch_up_specific_quorum == Some(true));
            sessions.push((
                uid,
                report.process_session_id,
                report.replicator_creation_id,
            ));
        }
        let target = if old == 2 { 3 } else { 2 };
        let last_peer = (1..=3).find(|id| *id != old && *id != target).unwrap();
        ensure!(target < last_peer, "fixture must prepare the target first");
        let set = harness.set().await?;
        let generation = set.metadata.generation;
        let status = set.status.clone();
        let request = harness.submit(target).await?;
        let updated = harness.set().await?;
        ensure!(
            updated.metadata.generation == generation.map(|generation| generation + 1)
                && updated.status == status
        );
        let peer = harness.reference(last_peer).await?;
        let preparation = harness
            .cluster
            .pause_at(Barrier::SwitchoverPeerEpoch {
                request: request.clone(),
                peer,
            })
            .await?;
        // A first catch-up is not a write fence. Acknowledge another write
        // after target preparation. Only the other old-epoch peer can ACK;
        // source Swap must transfer this missing committed prefix to the target.
        harness
            .cluster
            .probe(tokio::time::timeout(
                std::time::Duration::from_secs(5),
                old_service.replicate_put(
                    "switch-after-first-catch-up".into(),
                    "racing-transfer".into(),
                ),
            ))
            .await??;
        ensure!(
            harness
                .service(target)
                .await?
                .persistence()
                .get("switch-after-first-catch-up")
                .is_none(),
            "the prepared target unexpectedly received the old-epoch write"
        );
        harness.cluster.resume(preparation).await?;
        let frozen = if let Some(completion) = cut {
            let source = harness.reference(old).await?;
            let pause = harness
                .cluster
                .pause_at(Barrier::SwitchoverHandoff {
                    request: request.clone(),
                    source,
                    stage: if completion {
                        BarrierStage::Completion
                    } else {
                        BarrierStage::Intent
                    },
                })
                .await?;
            let frozen = serde_json::to_value(pause.evidence().set())?;
            ensure!(
                harness
                    .set()
                    .await?
                    .status
                    .unwrap()
                    .public_v1_topology
                    .unwrap()
                    .primary_id
                    .value()
                    == old,
                "handoff published final topology too early"
            );
            if cut == Some(true) {
                assert_exact_completion(&mut harness, &frozen).await?;
                reject_write(&old_service, "switch-handoff-boundary").await?;
            }
            harness.recreate_controller().await?;
            harness.cluster.resume(pause).await?;
            Some(frozen)
        } else {
            None
        };
        loop {
            let set = harness.set().await?;
            ensure!(
                set.status
                    .as_ref()
                    .is_none_or(|status| status.public_v1_replacement.is_none()),
                "switchover requested corrective replacement: {:?}",
                set.status
            );
            if set
                .status
                .as_ref()
                .and_then(|s| s.public_v1_topology.as_ref())
                .is_some_and(|t| {
                    t.primary_id.value() == target
                        && t.completed_switchover_request_id.as_deref() == Some(request.id())
                })
            {
                break;
            }
            harness.step().await?;
        }
        ensure!(harness.ready().await? == target);
        for (uid, session, creation) in sessions {
            let report = harness.report_uid(&uid).await?;
            ensure!(
                report.process_session_id == session && report.replicator_creation_id == creation,
                "switchover replaced/restarted an existing host"
            );
        }
        harness
            .assert_values("switch-after-first-catch-up", "racing-transfer")
            .await?;
        if let Some(frozen) = frozen {
            assert_exact_completion(&mut harness, &frozen).await?;
        }
        reject_write(&old_service, "switch-stale").await?;
        harness
            .assert_values("switch-before", "acknowledged")
            .await?;
        harness.write("switch-after", "new-primary").await?;
        harness.assert_values("switch-after", "new-primary").await
    }
    .await;
    harness.finish(result).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fresh_build_destination_isolation_precedes_assertion_reads() -> Result<()> {
    build_transport_cut(false).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fresh_build_source_isolation_cuts_the_real_dial() -> Result<()> {
    build_transport_cut(true).await
}

async fn build_transport_cut(source: bool) -> Result<()> {
    use kuberic_runtime::engine::DurableState;
    use kuberic_runtime::protocol::public_operations::ReplicaGoalKind;
    let mut harness = Harness::with_members(1).await?;
    let result = async {
        let primary = harness.ready_members(1).await?;
        let service = harness.service(primary).await?;
        harness
            .cluster
            .probe(service.replicate_put("build-cut-before".into(), "acknowledged".into()))
            .await?;
        harness.cluster.scale_to(2).await?;
        let pause = harness
            .cluster
            .pause_at(Barrier::intent(PublicV1CallKind::Build))
            .await?;
        let frozen = serde_json::to_value(pause.evidence().set())?;
        let entry = pause
            .evidence()
            .set()
            .status
            .as_ref()
            .context("Build intent status missing")?
            .replica_goals
            .iter()
            .rev()
            .find(|entry| matches!(entry.goal.desired, ReplicaGoalKind::Build { .. }))
            .context("ordinary Build intent missing")?;
        entry.validate().map_err(anyhow::Error::msg)?;
        ensure!(entry.outcome.is_none());
        let ReplicaGoalKind::Build { remote } = &entry.goal.desired else {
            bail!("selected barrier was not Build");
        };
        let remote = remote.clone();
        ensure!(frozen["status"]["publicV1Operation"].is_null());
        let target = harness
            .service_uid(remote.target.scope.storage.replica.instance_id.as_str())
            .await?;
        ensure!(target.persistence().durable_progress().await?.applied_lsn == 0);
        let cut = if source {
            remote.source.scope.storage.replica.instance_id.as_str()
        } else {
            remote.target.scope.storage.replica.instance_id.as_str()
        };
        harness.refresh().await?;
        let resource = harness.api.reference(Kind::Pod, cut)?;
        let source_ref = harness
            .reference_uid(remote.source.scope.storage.replica.instance_id.as_str())
            .await?;
        let destination_ref = harness
            .reference_uid(remote.target.scope.storage.replica.instance_id.as_str())
            .await?;
        let mut isolation = harness.cluster.arm_next_build_dial(&resource).await?;
        harness.cluster.resume(pause).await?;
        harness
            .cluster
            .drive(async { isolation.entered().await })
            .await?;
        ensure!(
            target.persistence().durable_progress().await?.applied_lsn == 0,
            "isolated first Build dial applied provider effects"
        );
        let address = harness
            .cluster
            .physical_route(&source_ref, &destination_ref)
            .await?
            .context("real Build did not dial the owned route")?;
        let uri: http::Uri = address.parse()?;
        let mut blocked = tokio::net::TcpStream::connect(uri.authority().unwrap().as_str()).await?;
        use tokio::io::AsyncReadExt;
        ensure!(
            tokio::time::timeout(std::time::Duration::from_secs(2), blocked.read_u8())
                .await?
                .is_err()
        );
        harness.heal(isolation).await?;
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            tonic::transport::Endpoint::from_shared(address)?.connect(),
        )
        .await??;
        let report = harness
            .report_uid(remote.target.scope.storage.replica.instance_id.as_str())
            .await?;
        ensure!(report.process_session_id == remote.target.scope.process_session_id);
        ensure!(target.persistence().durable_progress().await?.applied_lsn == 0);
        Ok(())
    }
    .await;
    harness.finish(result).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn teardown_with_isolation_and_entered_barriers_drains_owned_tasks() -> Result<()> {
    for completion in [false, true] {
        let mut harness = Harness::with_members(2).await?;
        let mut uids = Vec::new();
        let mut addresses = Vec::new();
        let result = async {
            let primary = harness.ready_members(2).await?;
            let service = harness.service(primary).await?;
            harness
                .cluster
                .probe(service.replicate_put("cleanup-before".into(), "acknowledged".into()))
                .await?;
            harness.cluster.scale_to(3).await?;
            let pause = harness
                .cluster
                .pause_at(Barrier::Operation {
                    stage: if completion {
                        BarrierStage::Completion
                    } else {
                        BarrierStage::Intent
                    },
                    kind: Some(PublicV1CallKind::Build),
                    participant: None,
                })
                .await?;
            uids = harness.live_uids().await;
            addresses = harness
                .api
                .snapshot(Kind::Pod)
                .iter()
                .filter_map(|pod| {
                    pod["metadata"]["annotations"]
                        [kuberic_controller::crd::CONTROL_ADDRESS_ANNOTATION]
                        .as_str()
                        .map(str::to_owned)
                })
                .collect();
            let uid = harness
                .report(primary)
                .await?
                .replica
                .instance_id
                .to_string();
            let resource = harness.api.reference(Kind::Pod, &uid)?;
            let _held = harness.cluster.hold_deletion(&resource).await?;
            let _isolation = harness.isolate_uids(std::slice::from_ref(&uid)).await?;
            let write = tokio::time::timeout(
                std::time::Duration::from_millis(50),
                service.replicate_put("cleanup-ambiguous".into(), "attempt".into()),
            )
            .await;
            ensure!(!matches!(write, Ok(Ok(_))));
            drop(pause);
            Ok(())
        }
        .await;
        tokio::time::timeout(std::time::Duration::from_secs(60), harness.finish(result)).await??;
        for uid in uids {
            let route =
                kuberic_runtime::replicator::TestingTcpRoute::register(uid, |_, endpoint| {
                    Box::pin(async move { Ok(endpoint) })
                })?;
            drop(route);
        }
        for address in addresses {
            let uri: http::Uri = address.parse()?;
            ensure!(
                tokio::net::TcpStream::connect(uri.authority().unwrap().as_str())
                    .await
                    .is_err(),
                "owned control frontend survived teardown"
            );
        }
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn replacement_recreation_at_persisted_build_intent_preserves_effects() -> Result<()> {
    recreate_during_change(false, false).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn replacement_recreation_after_durable_build_consumes_completion_once() -> Result<()> {
    recreate_during_change(false, true).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failover_recreation_at_persisted_promotion_preserves_effects() -> Result<()> {
    recreate_during_change(true, false).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failover_recreation_after_durable_promotion_consumes_completion_once() -> Result<()> {
    recreate_during_change(true, true).await
}

async fn recreate_during_change(failover: bool, completion: bool) -> Result<()> {
    use kuberic_runtime::engine::DurableState;
    let mut harness = Harness::new().await?;
    let result = async {
        let baseline_lsn = harness.write("recreation-before", "acknowledged").await?;
        harness
            .assert_values("recreation-before", "acknowledged")
            .await?;
        let primary = harness.ready().await?;
        let predecessor = harness.report(if failover { primary } else { 2 }).await?;
        let predecessor_uid = predecessor.replica.instance_id.to_string();
        let predecessor_root = harness.data_root(&predecessor_uid).await?;
        let predecessor_service = harness.service_uid(&predecessor_uid).await?;
        let baseline = predecessor_service
            .persistence()
            .snapshot_at(baseline_lsn)?;
        let participant = if failover {
            Some(harness.reference(2).await?)
        } else {
            None
        };
        let isolation = if failover {
            Some(
                harness
                    .isolate_uids(std::slice::from_ref(&predecessor_uid))
                    .await?,
            )
        } else {
            None
        };
        let removed = if failover {
            Vec::new()
        } else {
            delete_secondary(&mut harness).await?
        };
        let pause = harness
            .cluster
            .pause_at(Barrier::Operation {
                stage: if completion {
                    BarrierStage::Completion
                } else {
                    BarrierStage::Intent
                },
                kind: Some(if failover {
                    PublicV1CallKind::Role
                } else {
                    PublicV1CallKind::Build
                }),
                participant,
            })
            .await?;
        let frozen = serde_json::to_value(pause.evidence().set())?;
        let accepted: PublicV1OperationStatus =
            serde_json::from_value(frozen["status"]["publicV1Operation"].clone())?;
        let command = accepted.command().map_err(anyhow::Error::msg)?;
        let source_before = harness
            .report_uid(command.target.instance_id.as_str())
            .await?;
        let target =
            if let Some(PublicOperationProgram::Build(build)) = command.intent.program.as_ref() {
                let target = harness
                    .service_uid(build.replica.instance_id.as_str())
                    .await?;
                let progress = target.persistence().durable_progress().await?;
                ensure!(progress.applied_lsn == if completion { baseline_lsn } else { 0 });
                Some((build.replica.instance_id.to_string(), target))
            } else {
                ensure!(failover, "replacement cut missed real Build");
                ensure!(
                    matches!(
                        command.intent.program.as_ref(),
                        Some(PublicOperationProgram::Role {
                            role: kuberic_runtime::protocol::types::ReplicaRole::Primary,
                            ..
                        })
                    ),
                    "failover cut missed the accepted primary promotion"
                );
                None
            };
        if completion {
            let completed = assert_exact_completion(&mut harness, &frozen).await?;
            let authority = harness
                .set()
                .await?
                .status
                .unwrap()
                .public_v1_authority
                .unwrap()
                .authority()
                .map_err(anyhow::Error::msg)?;
            ensure!(
                !authority.completed_operations.contains(&completed),
                "completion cut occurred after authority advancement"
            );
        } else {
            ensure!(
                !source_before
                    .completed_operations
                    .iter()
                    .any(|entry| entry.operation_id == command.intent.operation_id
                        && entry.revision == command.intent.revision),
                "intent cut already executed"
            );
        }
        let topology_before = harness
            .set()
            .await?
            .status
            .unwrap()
            .public_v1_topology
            .unwrap();
        ensure!(topology_before.primary_id.value() == primary);
        harness.recreate_controller().await?;
        let persisted = harness
            .api
            .get(Kind::KubericSet, "sample")
            .context("set missing")?;
        for field in [
            "publicV1Operation",
            "publicV1Transition",
            "publicV1Authority",
            "publicV1Replacement",
        ] {
            ensure!(
                persisted["status"][field] == frozen["status"][field],
                "recreation rewrote persisted {field}"
            );
        }
        harness.cluster.resume(pause).await?;
        let settled_primary = if failover {
            harness
                .ready_survivors(2, isolation.as_ref().unwrap())
                .await?
        } else {
            harness.ready().await?
        };
        ensure!((settled_primary != primary) == failover);
        let completed = assert_exact_completion(&mut harness, &frozen).await?;
        let authority = harness
            .set()
            .await?
            .status
            .unwrap()
            .public_v1_authority
            .unwrap()
            .authority()
            .map_err(anyhow::Error::msg)?;
        ensure!(
            authority
                .completed_operations
                .iter()
                .filter(|entry| **entry == completed)
                .count()
                == 1,
            "matching completion was not consumed exactly once"
        );
        let source_after = harness
            .report_uid(command.target.instance_id.as_str())
            .await?;
        ensure!(
            source_before.process_session_id == source_after.process_session_id
                && source_before.replicator_creation_id == source_after.replicator_creation_id
                && source_before.storage_incarnation == source_after.storage_incarnation
        );
        for uid in harness.live_uids().await {
            if failover && uid == predecessor_uid {
                continue;
            }
            let service = harness.service_uid(&uid).await?;
            let progress = service.persistence().durable_progress().await?;
            ensure!(
                progress.applied_lsn == baseline_lsn && progress.committed_lsn == baseline_lsn,
                "recovery duplicated logical application effects: {progress:?}"
            );
            ensure!(service.persistence().snapshot_at(baseline_lsn)? == baseline);
        }
        if let Some((uid, target)) = target {
            let replacement = harness.report(2).await?;
            ensure!(
                replacement.replica.instance_id.as_str() == uid
                    && replacement.replica != predecessor.replica
                    && replacement.storage_incarnation != predecessor.storage_incarnation
            );
            ensure!(std::sync::Arc::ptr_eq(&target, &harness.service(2).await?));
            for (kind, _, uid) in removed {
                ensure!(
                    !harness
                        .api
                        .snapshot(kind)
                        .iter()
                        .any(|object| object["metadata"]["uid"].as_str() == Some(&uid)),
                    "exact predecessor resource retained: {uid}"
                );
            }
            ensure!(!predecessor_root.exists(), "retired PVC storage retained");
        }
        let next_lsn = harness
            .service(settled_primary)
            .await?
            .replicate_put("recreation-after".into(), "resumed".into())
            .await?;
        ensure!(
            next_lsn == baseline_lsn + 1,
            "recovery advanced application LSN"
        );
        if failover {
            let before_probe = predecessor_service.persistence().durable_progress().await?;
            reject_write(&predecessor_service, "recreation-stale-isolated").await?;
            let after_probe = predecessor_service.persistence().durable_progress().await?;
            let prepared = predecessor_service.persistence().snapshot_at(after_probe.applied_lsn)?;
            eprintln!(
                "recreation predecessor probe: uid={predecessor_uid} session={} before={before_probe:?} after={after_probe:?} prepared={:?} committed_view={:?}",
                predecessor.process_session_id,
                prepared.get("recreation-stale-isolated"),
                predecessor_service.get("recreation-stale-isolated").await,
            );
            harness.heal(isolation.unwrap()).await?;
            harness.ready().await?;
            let returned = harness.report(primary).await?;
            ensure!(
                returned.role == kuberic_runtime::protocol::types::ReplicaRole::ActiveSecondary,
                "the old logical member did not safely rejoin as a secondary"
            );
            let current_service = harness.service(primary).await?;
            if returned.process_session_id == predecessor.process_session_id {
                ensure!(
                    returned.replica == predecessor.replica
                        && returned.replicator_creation_id == predecessor.replicator_creation_id
                        && returned.storage_incarnation == predecessor.storage_incarnation
                        && std::sync::Arc::ptr_eq(&predecessor_service, &current_service),
                    "retained predecessor owner changed its exact identity"
                );
            } else {
                ensure!(
                    returned.replicator_creation_id != predecessor.replicator_creation_id
                        && !std::sync::Arc::ptr_eq(&predecessor_service, &current_service),
                    "native cold-copy rejoin reused the retired owner"
                );
                ensure!(
                    matches!(
                        predecessor_service.get("recreation-before").await,
                        Err(kuberic_runtime::RuntimeError::NotOpen)
                    ),
                    "retired predecessor retained client read access"
                );
                if returned.replica != predecessor.replica {
                    ensure!(
                        !harness.live_uids().await.contains(&predecessor_uid),
                        "retired predecessor Pod remained live"
                    );
                }
                if returned.storage_incarnation != predecessor.storage_incarnation {
                    ensure!(
                        !predecessor_root.exists(),
                        "retired predecessor storage retained"
                    );
                }
            }
            ensure!(
                tokio::time::timeout(
                    std::time::Duration::from_secs(2),
                    predecessor_service
                        .replicate_put("recreation-stale-returned".into(), "forbidden".into(),),
                )
                .await?
                .is_err(),
                "retained/retired old owner admitted a write after rejoin"
            );
        }
        harness
            .assert_values("recreation-before", "acknowledged")
            .await?;
        harness.assert_values("recreation-after", "resumed").await
    }
    .await;
    harness.finish(result).await
}
