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
        let (altered_status, altered_evidence) =
            rewrite_detached_configuration(&status, &evidence, |program| {
                if let PublicOperationProgram::Configuration(public)
                | PublicOperationProgram::CatchUp {
                    configuration: public,
                    ..
                } = program
                {
                    if let Some(peer) = public.replicas.first_mut() {
                        peer.process_session_id =
                            kuberic_runtime::protocol::types::ProcessSessionId::new("stale-peer");
                    }
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

async fn assert_exact_completion(harness: &Harness, persisted: &serde_json::Value) -> Result<()> {
    let accepted: PublicV1OperationStatus =
        serde_json::from_value(persisted["status"]["publicV1Operation"].clone())?;
    let command = accepted.command().map_err(anyhow::Error::msg)?;
    let report = harness.report(command.target.replica_id.value()).await?;
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
            && completion.result == PublicV1OperationResult::Succeeded,
        "completion differs from the persisted operation"
    );
    Ok(())
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
    let mut harness = Harness::new().await?;
    let result = async {
        harness.write("failover", "before").await?;
        harness.assert_values("failover", "before").await?;
        let primary = harness.ready().await?;
        let old_service = harness.service(primary).await?;
        harness.stop(primary, false).await?;
        let promoted = harness.ready_members(2).await?;
        ensure!(promoted != primary, "failover did not promote a survivor");
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
