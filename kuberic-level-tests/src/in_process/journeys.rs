use anyhow::{Context, Result, bail, ensure};
use k8s_openapi::api::core::v1::{Pod, Secret, Service};
use kube::{
    Api, Resource, ResourceExt,
    api::{DeleteParams, Preconditions},
};
use kuberic_controller::crd::PublicV1OperationStatus;
use kuberic_runtime::protocol::public_operations::{
    PublicOperationProgram, PublicV1AcceptedOperationCompletion, PublicV1CallKind,
    PublicV1OperationResult,
};

use super::{harness::Harness, kube::Kind};

fn rewrite_detached_configuration(
    status: &kuberic_controller::crd::KubericSetStatus,
    evidence: &super::harness::ConfigurationEvidence,
    change: impl Fn(&mut PublicOperationProgram),
) -> Result<(
    kuberic_controller::crd::KubericSetStatus,
    super::harness::ConfigurationEvidence,
)> {
    let mut status = status.clone();
    let mut evidence = evidence.clone();
    let mut authority = status
        .public_v1_authority
        .as_ref()
        .unwrap()
        .authority()
        .map_err(anyhow::Error::msg)?;
    for (intent, completion) in evidence.operations.values_mut() {
        let old = PublicV1AcceptedOperationCompletion::from_completion(completion);
        let accepted = authority
            .completed_operations
            .iter_mut()
            .find(|accepted| **accepted == old)
            .context("detached baseline not accepted")?;
        let program = intent.program.as_mut().unwrap();
        change(program);
        intent.input_digest = program.digest();
        completion.input_digest = intent.input_digest.clone();
        if matches!(program, PublicOperationProgram::Epoch { .. }) {
            completion.call_kind = PublicV1CallKind::Epoch;
            completion.current_configuration_id = None;
        }
        intent.validate().map_err(anyhow::Error::msg)?;
        *accepted = PublicV1AcceptedOperationCompletion::from_completion(completion);
    }
    status.public_v1_authority = Some(
        kuberic_controller::crd::PublicV1AuthorityStatus::from_authority(&authority)
            .map_err(anyhow::Error::msg)?,
    );
    Ok((status, evidence))
}

async fn delete_secondary(harness: &Harness) -> Result<Vec<(Kind, String, String)>> {
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
    Api::<Pod>::namespaced(harness.api.client(), "in-process")
        .delete(
            pod["metadata"]["name"].as_str().unwrap(),
            &DeleteParams {
                preconditions: Some(Preconditions {
                    uid: Some(uid.to_owned()),
                    resource_version: None,
                }),
                ..Default::default()
            },
        )
        .await?;
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
        let services = Api::<Service>::namespaced(harness.api.client(), "in-process");
        let secrets = Api::<Secret>::namespaced(harness.api.client(), "in-process");
        let old_service = services.get("sample-peer").await?;
        let old_secret = secrets.get("sample-agent-credentials").await?;
        services
            .delete(
                "sample-peer",
                &DeleteParams {
                    preconditions: Some(Preconditions {
                        uid: old_service.uid(),
                        resource_version: old_service.resource_version(),
                    }),
                    ..Default::default()
                },
            )
            .await?;
        secrets
            .delete(
                "sample-agent-credentials",
                &DeleteParams {
                    preconditions: Some(Preconditions {
                        uid: old_secret.uid(),
                        resource_version: old_secret.resource_version(),
                    }),
                    ..Default::default()
                },
            )
            .await?;
        loop {
            harness.step().await?;
            if let (Some(service), Some(secret)) = (
                services.get_opt("sample-peer").await?,
                secrets.get_opt("sample-agent-credentials").await?,
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
                    secret.data == old_secret.data,
                    "repair changed startup credential contract"
                );
                ensure!(
                    secret
                        .data
                        .as_ref()
                        .and_then(|data| data.get("bearer-token"))
                        .map(|token| token.0.as_slice())
                        == Some(b"local-test-token".as_slice())
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
        let resources = delete_secondary(&harness).await?;
        let pending_replacement = loop {
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
        let pending_renewal = loop {
            if let Some(renewal) = harness
                .set()
                .await?
                .status
                .and_then(|s| s.public_v1_session_renewal)
            {
                break renewal;
            }
            harness.step().await?;
        };
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
        let status = harness.set().await?.status.context("status missing")?;
        let mut reports = Vec::new();
        for id in 1..=3 {
            reports.push(harness.report(id).await?);
        }
        ensure!(Harness::settled(&status, &reports, 3));
        let mut altered = status.clone();
        altered.public_v1_replacement = Some(pending_replacement);
        ensure!(!Harness::settled(&altered, &reports, 3));
        altered = status;
        altered.public_v1_session_renewal = Some(pending_renewal);
        ensure!(!Harness::settled(&altered, &reports, 3));
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn readiness_rejects_detached_stale_sessions_configuration_and_writers() -> Result<()> {
    let mut harness = Harness::new().await?;
    let result = async {
        let mut barrier = harness.api.arm_operation_barrier("sample")?;
        let pending = loop {
            tokio::select! {
                pending = barrier.entered() => break pending?,
                step = harness.step() => step?,
            }
        };
        barrier.release();
        harness.ready().await?;
        let status = harness.set().await?.status.context("status missing")?;
        let mut reports = Vec::new();
        for id in 1..=3 {
            reports.push(harness.report(id).await?);
        }
        ensure!(Harness::settled(&status, &reports, 3));
        let evidence = harness.configuration_evidence(&reports).await?;
        ensure!(Harness::configurations_settled(
            &status, &reports, &evidence
        ));
        let mut pending_transfer = status.clone();
        let topology = pending_transfer.public_v1_topology.as_mut().unwrap();
        topology.pending_switchover = Some(kuberic_controller::crd::PlannedSwitchoverRequestSpec {
            request_id: "pending-transfer".into(),
            target_replica_id: topology
                .sessions
                .iter()
                .find(|session| session.identity.replica_id != topology.primary_id)
                .unwrap()
                .identity
                .replica_id
                .value() as u32,
        });
        ensure!(!Harness::settled(&pending_transfer, &reports, 3));
        ensure!(!Harness::configurations_settled(
            &pending_transfer,
            &reports,
            &evidence
        ));
        let (altered_status, altered_evidence) =
            rewrite_detached_configuration(&status, &evidence, |program| {
                if let PublicOperationProgram::Configuration(public)
                | PublicOperationProgram::CatchUp {
                    configuration: public,
                    ..
                } = program
                    && let Some(peer) = public.replicas.first_mut()
                {
                    peer.process_session_id =
                        kuberic_runtime::protocol::types::ProcessSessionId::new("stale-peer");
                }
            })?;
        ensure!(!Harness::configurations_settled(
            &altered_status,
            &reports,
            &altered_evidence
        ));
        let (unrelated_status, unrelated) =
            rewrite_detached_configuration(&status, &evidence, |program| {
                *program = PublicOperationProgram::Epoch {
                    epoch: status.public_v1_topology.as_ref().unwrap().epoch,
                };
            })?;
        ensure!(!Harness::configurations_settled(
            &unrelated_status,
            &reports,
            &unrelated
        ));
        let (historical_status, historical) =
            rewrite_detached_configuration(&status, &evidence, |program| {
                if let PublicOperationProgram::Configuration(public)
                | PublicOperationProgram::CatchUp {
                    configuration: public,
                    ..
                } = program
                {
                    let mut epoch = public.current.epoch;
                    epoch.configuration_number += 1;
                    public.current = kuberic_runtime::protocol::types::ConfigurationDescriptor::new(
                        epoch,
                        public.current.primary_id,
                        public.current.members.clone(),
                        public.current.write_quorum,
                    );
                }
            })?;
        ensure!(!Harness::configurations_settled(
            &historical_status,
            &reports,
            &historical
        ));
        for field in ["publicV1Operation", "publicV1Transition"] {
            let mut altered = serde_json::to_value(&status)?;
            altered[field] = pending["status"][field].clone();
            ensure!(
                !altered[field].is_null(),
                "pending observation missing {field}"
            );
            ensure!(!Harness::settled(
                &serde_json::from_value(altered)?,
                &reports,
                3
            ));
        }
        for (field, value) in [
            ("processSessionId", serde_json::json!("stale-session")),
            ("replicatorCreationId", serde_json::json!("stale-creation")),
            (
                "storageIncarnation",
                serde_json::to_value(&reports[2].storage_incarnation)?,
            ),
            ("role", serde_json::json!("none")),
            ("writeStatus", serde_json::json!("granted")),
            ("readStatus", serde_json::json!("notPrimary")),
        ] {
            let mut altered = reports.clone();
            let mut report = serde_json::to_value(&altered[1])?;
            report[field] = value;
            altered[1] = serde_json::from_value(report)?;
            ensure!(
                !Harness::settled(&status, &altered, 3),
                "accepted stale {field}"
            );
        }
        let mut altered = reports.clone();
        let primary = altered
            .iter_mut()
            .find(|r| r.role == kuberic_runtime::protocol::types::ReplicaRole::Primary)
            .unwrap();
        primary.write_status = kuberic_runtime::protocol::types::AccessStatus::NotPrimary;
        ensure!(!Harness::settled(&status, &altered, 3));
        for index in 0..reports.len() {
            let mut altered = reports.clone();
            altered[index].progress.last = altered[index].progress.first + 1;
            ensure!(
                !Harness::settled(&status, &altered, 3)
                    && !Harness::configurations_settled(&status, &altered, &evidence),
                "uncommitted progress accepted for {:?}",
                altered[index].role
            );
        }
        let mut outsider = reports[0].clone();
        outsider.replica.instance_id =
            kuberic_runtime::protocol::types::ReplicaInstanceId::new("excluded-predecessor");
        outsider.write_status = kuberic_runtime::protocol::types::AccessStatus::Granted;
        altered = reports.clone();
        altered.push(outsider.clone());
        ensure!(!Harness::settled(&status, &altered, 3));
        outsider.write_status = kuberic_runtime::protocol::types::AccessStatus::NotPrimary;
        outsider.read_status = kuberic_runtime::protocol::types::AccessStatus::NotPrimary;
        altered = reports.clone();
        altered.push(outsider.clone());
        ensure!(
            !Harness::settled(&status, &altered, 3),
            "nonwriting unauthorized role accepted"
        );
        outsider.role = kuberic_runtime::protocol::types::ReplicaRole::None;
        outsider.read_status = kuberic_runtime::protocol::types::AccessStatus::Granted;
        altered = reports.clone();
        altered.push(outsider.clone());
        ensure!(
            !Harness::settled(&status, &altered, 3),
            "excluded reader accepted"
        );
        let uids = reports
            .iter()
            .map(|report| report.replica.instance_id.to_string())
            .chain(std::iter::once(outsider.replica.instance_id.to_string()))
            .collect::<Vec<_>>();
        ensure!(!Harness::observations_complete(&uids, &reports));
        ensure!(
            !Harness::observations_complete(&uids, &reports[..2]),
            "two admitted observations hid unreachable predecessor"
        );
        altered = reports;
        for report in &mut altered {
            report.completed_operations.clear();
        }
        ensure!(!Harness::settled(&status, &altered, 3));
        Ok(())
    }
    .await;
    harness.finish(result).await
}

async fn assert_exact_completion(
    harness: &Harness,
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
        let mut conflict = harness.api.arm_completion_conflict("sample")?;
        let (lsn, persisted) =
            tokio::try_join!(harness.write("bootstrap", "committed"), async move {
                let persisted = conflict.entered().await?;
                conflict.release();
                Ok::<_, anyhow::Error>(persisted)
            })?;
        ensure!(lsn > 0, "write did not allocate an LSN");
        harness.assert_values("bootstrap", "committed").await?;
        let mut addresses = std::collections::BTreeSet::new();
        for id in 1..=3 {
            let report = harness.report(id).await?;
            let address = report
                .replication_address
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
        assert_exact_completion(&harness, &persisted).await?;
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
        let old_endpoint: http::Uri = before
            .replication_address
            .as_ref()
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
        let new_endpoint: http::Uri = after
            .replication_address
            .as_ref()
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
        harness.hold_deletion(&uid, true).await;
        Api::<Pod>::namespaced(harness.api.client(), "in-process")
            .delete(
                &pod.name_any(),
                &DeleteParams {
                    preconditions: Some(Preconditions {
                        uid: Some(uid.clone()),
                        resource_version: None,
                    }),
                    ..Default::default()
                },
            )
            .await?;
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
        harness.hold_deletion(&uid, false).await;
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
        if request_transfer {
            let mut set = harness.set().await?;
            set.spec.switchover = Some(kuberic_controller::crd::PlannedSwitchoverRequestSpec {
                request_id: "unaccepted-transfer".into(),
                target_replica_id: target as u32,
            });
            Api::<kuberic_controller::crd::KubericSet>::namespaced(
                harness.api.client(),
                "in-process",
            )
            .replace("sample", &kube::api::PostParams::default(), &set)
            .await?;
            ensure!(
                set.status
                    .unwrap()
                    .public_v1_topology
                    .unwrap()
                    .pending_switchover
                    .is_none()
            );
        }
        let promoted = harness.ready_members(2).await?;
        ensure!(promoted != primary, "failover did not promote a survivor");
        if request_transfer {
            let set = harness.set().await?;
            ensure!(
                promoted == target
                    && set
                        .spec
                        .switchover
                        .as_ref()
                        .is_some_and(|request| request.request_id == "unaccepted-transfer")
            );
            ensure!(
                set.status
                    .unwrap()
                    .public_v1_topology
                    .unwrap()
                    .completed_switchover_request_id
                    .as_deref()
                    == Some("unaccepted-transfer")
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
    let mut harness = Harness::new().await?;
    let result = async {
        let mut barrier = harness.api.arm_operation_barrier("sample")?;
        let accepted = loop {
            tokio::select! {
                accepted = barrier.entered() => break accepted?,
                result = harness.step() => result?,
            }
        };
        let command = accepted["status"]["publicV1Operation"].clone();
        ensure!(
            command.is_object(),
            "barrier did not capture an accepted operation"
        );
        harness.recreate_controller()?;
        barrier.release();
        let persisted = harness
            .api
            .get(Kind::KubericSet, "sample")
            .context("set missing")?;
        ensure!(
            persisted["status"]["publicV1Operation"] == command,
            "controller recreation repaired authority"
        );
        harness.write("controller", "resumed").await?;
        harness.assert_values("controller", "resumed").await?;
        let status = harness.set().await?.status.context("status missing")?;
        if status.public_v1_operation.is_some() {
            bail!("accepted operation remains pending");
        }
        assert_exact_completion(&harness, &accepted).await?;
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
    ensure!(
        !matches!(result, Ok(Ok(_))),
        "unsafe write acknowledged: {key}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn live_primary_isolation_fails_over_and_rejoins_without_recreation() -> Result<()> {
    let mut harness = Harness::new().await?;
    let result = async {
        harness.write("live-before", "acknowledged").await?;
        harness.assert_values("live-before", "acknowledged").await?;
        let primary = harness.ready().await?;
        let old = harness.report(primary).await?;
        let uid = old.replica.instance_id.to_string();
        let service = harness.service(primary).await?;
        harness.isolate_uid(&uid, true).await?;
        let promoted = harness
            .ready_survivors(2, std::slice::from_ref(&uid))
            .await?;
        ensure!(promoted != primary);
        harness
            .service(promoted)
            .await?
            .replicate_put("live-after".into(), "new-primary".into())
            .await?;
        reject_write(&service, "live-stale-isolated").await?;
        ensure!(harness.live_uids().await.contains(&uid));
        harness.isolate_uid(&uid, false).await?;
        let returned = harness.report_uid(&uid).await?;
        ensure!(
            returned.process_session_id == old.process_session_id
                && returned.replicator_creation_id == old.replicator_creation_id
                && returned.replica == old.replica
        );
        ensure!(std::sync::Arc::ptr_eq(
            &service,
            &harness.service_uid(&uid).await?
        ));
        reject_write(&service, "live-stale-reconnected").await?;
        harness.ready().await?;
        let settled = harness.report_uid(&uid).await?;
        ensure!(
            settled.process_session_id == old.process_session_id
                && settled.replicator_creation_id == old.replicator_creation_id
        );
        ensure!(settled.role == kuberic_runtime::protocol::types::ReplicaRole::ActiveSecondary);
        reject_write(&service, "live-stale-settled").await?;
        harness.assert_values("live-before", "acknowledged").await?;
        harness.assert_values("live-after", "new-primary").await?;
        harness.write("live-recovered", "quorum").await?;
        harness.assert_values("live-recovered", "quorum").await
    }
    .await;
    harness.finish(result).await
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
                harness.isolate_uid(&uid, true).await?;
                isolated.push(uid);
            }
        }
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
        for uid in isolated {
            harness.isolate_uid(&uid, false).await?;
        }
        ensure!(harness.ready().await? == primary);
        harness
            .assert_values("quorum-before", "acknowledged")
            .await?;
        harness.write("quorum-after", "recovered").await?;
        harness.assert_values("quorum-after", "recovered").await?;
        for key in ["quorum-ambiguous", "quorum-lone"] {
            let value = harness.service(1).await?.get(key).await?;
            ensure!(
                value
                    .as_deref()
                    .is_none_or(|value| value == "ambiguous-attempt")
            );
            for id in 2..=3 {
                ensure!(
                    harness.service(id).await?.get(key).await? == value,
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
            let mut barrier = harness.api.arm_matching(
                "sample",
                true,
                Some("configuration"),
                Some(&format!("-prepare-{last_peer}-")),
            )?;
            let sets = Api::<kuberic_controller::crd::KubericSet>::namespaced(
                harness.api.client(),
                "in-process",
            );
            let mut set = harness.set().await?;
            let request = kuberic_controller::crd::PlannedSwitchoverRequestSpec {
                request_id: "frozen-transfer".into(),
                target_replica_id: target as u32,
            };
            set.spec.switchover = Some(request.clone());
            sets.replace("sample", &kube::api::PostParams::default(), &set)
                .await?;
            let frozen = loop {
                tokio::select! {
                    frozen = barrier.entered() => break frozen?,
                    step = harness.step() => step?,
                }
            };
            let completed = assert_exact_completion(&harness, &frozen).await?;
            let mut set = harness.set().await?;
            let topology = set
                .status
                .as_ref()
                .unwrap()
                .public_v1_topology
                .as_ref()
                .unwrap();
            ensure!(
                topology.pending_switchover.as_ref() == Some(&request)
                    && topology.configuration_id == initial.configuration_id
                    && topology.sessions == initial.sessions
            );
            set.spec.switchover =
                retarget.then(|| kuberic_controller::crd::PlannedSwitchoverRequestSpec {
                    request_id: "superseding-transfer".into(),
                    target_replica_id: last_peer as u32,
                });
            let updated = sets
                .replace("sample", &kube::api::PostParams::default(), &set)
                .await?;
            ensure!(
                updated.status == set.status,
                "spec edit changed frozen authority"
            );
            harness.recreate_controller()?;
            barrier.release();
            loop {
                let set = harness.set().await?;
                let topology = set
                    .status
                    .as_ref()
                    .unwrap()
                    .public_v1_topology
                    .as_ref()
                    .unwrap();
                if topology.completed_switchover_request_id.as_deref() == Some("frozen-transfer") {
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
                        == Some("superseding-transfer")
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
        let preparation_fragment = format!("-prepare-{last_peer}-");
        let mut set = harness.set().await?;
        let generation = set.metadata.generation;
        let status = set.status.clone();
        let mut preparation = harness.api.arm_matching(
            "sample",
            false,
            Some("epoch"),
            Some(&preparation_fragment),
        )?;
        set.spec.switchover = Some(kuberic_controller::crd::PlannedSwitchoverRequestSpec {
            request_id: "requested-transfer".into(),
            target_replica_id: target as u32,
        });
        let updated = Api::<kuberic_controller::crd::KubericSet>::namespaced(
            harness.api.client(),
            "in-process",
        )
        .replace("sample", &kube::api::PostParams::default(), &set)
        .await?;
        ensure!(
            updated.metadata.generation == generation.map(|generation| generation + 1)
                && updated.status == status
        );
        loop {
            tokio::select! {
                frozen = preparation.entered() => { frozen?; break; }
                step = harness.step() => step?,
            }
        }
        // A first catch-up is not a write fence. Acknowledge another write
        // after target preparation. Only the other old-epoch peer can ACK;
        // source Swap must transfer this missing committed prefix to the target.
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            old_service.replicate_put(
                "switch-after-first-catch-up".into(),
                "racing-transfer".into(),
            ),
        )
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
        preparation.release();
        let mut barrier = cut
            .map(|completion| {
                harness
                    .api
                    .arm_matching("sample", completion, Some("swap"), Some("-handoff-"))
            })
            .transpose()?;
        let frozen = if let Some(barrier) = &mut barrier {
            let frozen = loop {
                tokio::select! {
                    frozen = barrier.entered() => break frozen?,
                    step = harness.step() => step?,
                }
            };
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
                assert_exact_completion(&harness, &frozen).await?;
                reject_write(&old_service, "switch-handoff-boundary").await?;
            }
            harness.recreate_controller()?;
            Some(frozen)
        } else {
            None
        };
        if let Some(barrier) = barrier.take() {
            barrier.release();
        }
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
                        && t.completed_switchover_request_id.as_deref()
                            == Some("requested-transfer")
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
            assert_exact_completion(&harness, &frozen).await?;
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
    let mut harness = Harness::new().await?;
    let result = async {
        harness.write("build-cut-before", "acknowledged").await?;
        let mut barrier = harness
            .api
            .arm_matching("sample", false, Some("build"), None)?;
        delete_secondary(&harness).await?;
        let frozen = loop {
            tokio::select! {
                frozen = barrier.entered() => break frozen?,
                step = harness.step() => step?,
            }
        };
        let accepted: PublicV1OperationStatus =
            serde_json::from_value(frozen["status"]["publicV1Operation"].clone())?;
        let command = accepted.command().map_err(anyhow::Error::msg)?;
        let Some(PublicOperationProgram::Build(build)) = command.intent.program.as_ref() else {
            bail!("selected barrier was not Build");
        };
        let target = harness
            .service_uid(build.replica.instance_id.as_str())
            .await?;
        ensure!(target.persistence().durable_progress().await?.applied_lsn == 0);
        let cut = if source {
            command.target.instance_id.as_str()
        } else {
            build.replica.instance_id.as_str()
        };
        let network = harness.transport();
        network.isolate_on_next_dial(cut).await;
        barrier.release();
        while !network.is_isolated(cut).await {
            harness.step().await?;
        }
        ensure!(
            target.persistence().durable_progress().await?.applied_lsn == 0,
            "isolated first Build dial applied provider effects"
        );
        let address = network
            .physical_route(
                command.target.instance_id.as_str(),
                &build.replication_address,
            )
            .await
            .context("real Build did not dial the owned route")?;
        let uri: http::Uri = address.parse()?;
        let mut blocked = tokio::net::TcpStream::connect(uri.authority().unwrap().as_str()).await?;
        use tokio::io::AsyncReadExt;
        ensure!(
            tokio::time::timeout(std::time::Duration::from_secs(2), blocked.read_u8())
                .await?
                .is_err()
        );
        harness.isolate_uid(cut, false).await?;
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            tonic::transport::Endpoint::from_shared(address)?.connect(),
        )
        .await??;
        let report = harness
            .report_uid(build.replica.instance_id.as_str())
            .await?;
        ensure!(report.process_session_id == build.process_session_id);
        ensure!(target.persistence().durable_progress().await?.applied_lsn == 0);
        Ok(())
    }
    .await;
    harness.finish(result).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn teardown_with_isolation_and_entered_barriers_drains_owned_tasks() -> Result<()> {
    for completion in [false, true] {
        let mut harness = Harness::new().await?;
        let network = harness.transport();
        let mut uids = Vec::new();
        let mut addresses = Vec::new();
        let result = async {
            harness.write("cleanup-before", "acknowledged").await?;
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
            let service = harness.service(1).await?;
            let mut barrier = harness.api.arm_matching("sample", completion, None, None)?;
            let mut set = harness.set().await?;
            set.spec.switchover = Some(kuberic_controller::crd::PlannedSwitchoverRequestSpec {
                request_id: "cleanup-transfer".into(),
                target_replica_id: 2,
            });
            Api::<kuberic_controller::crd::KubericSet>::namespaced(
                harness.api.client(),
                "in-process",
            )
            .replace("sample", &kube::api::PostParams::default(), &set)
            .await?;
            loop {
                tokio::select! {
                    frozen = barrier.entered() => { frozen?; break; }
                    step = harness.step() => step?,
                }
            }
            let uid = harness.report(1).await?.replica.instance_id.to_string();
            harness.hold_deletion(&uid, true).await;
            harness.isolate_uid(&uid, true).await?;
            let write = tokio::time::timeout(
                std::time::Duration::from_millis(50),
                service.replicate_put("cleanup-ambiguous".into(), "attempt".into()),
            )
            .await;
            ensure!(!matches!(write, Ok(Ok(_))));
            drop(barrier);
            Ok(())
        }
        .await;
        tokio::time::timeout(std::time::Duration::from_secs(60), harness.finish(result)).await??;
        network.assert_drained().await?;
        for uid in uids {
            let route = kuberic_runtime::replicator::stateless::TestingTcpRoute::register(
                uid,
                |_, endpoint| Box::pin(async move { Ok(endpoint) }),
            )?;
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
        let promotion_session = harness.report(2).await?.process_session_id.to_string();
        let mut barrier = harness.api.arm_matching(
            "sample",
            completion,
            Some(if failover { "role" } else { "build" }),
            if failover {
                Some(promotion_session.as_str())
            } else {
                None
            },
        )?;
        let removed = if failover {
            harness.isolate_uid(&predecessor_uid, true).await?;
            Vec::new()
        } else {
            delete_secondary(&harness).await?
        };
        // Leaving this select scope cancels the old reconcile before recreation.
        let frozen = loop {
            tokio::select! {
                frozen = barrier.entered() => break frozen?,
                step = harness.step() => step?,
            }
        };
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
            let completed = assert_exact_completion(&harness, &frozen).await?;
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
        harness.recreate_controller()?;
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
        barrier.release();
        let settled_primary = if failover {
            harness
                .ready_survivors(2, std::slice::from_ref(&predecessor_uid))
                .await?
        } else {
            harness.ready().await?
        };
        ensure!((settled_primary != primary) == failover);
        let completed = assert_exact_completion(&harness, &frozen).await?;
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
            reject_write(&predecessor_service, "recreation-stale-isolated").await?;
            harness.isolate_uid(&predecessor_uid, false).await?;
            harness.ready().await?;
            let returned = harness.report_uid(&predecessor_uid).await?;
            ensure!(
                returned.process_session_id == predecessor.process_session_id
                    && returned.replicator_creation_id == predecessor.replicator_creation_id
            );
            reject_write(&predecessor_service, "recreation-stale-returned").await?;
        }
        harness
            .assert_values("recreation-before", "acknowledged")
            .await?;
        harness.assert_values("recreation-after", "resumed").await
    }
    .await;
    harness.finish(result).await
}
