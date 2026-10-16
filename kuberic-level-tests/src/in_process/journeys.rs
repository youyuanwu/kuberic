use anyhow::{Context, Result, bail, ensure};
use k8s_openapi::api::core::v1::Pod;
use kube::{
    Api, ResourceExt,
    api::{DeleteParams, Preconditions},
};
use kuberic_controller::crd::PublicV1OperationStatus;
use kuberic_runtime::protocol::public_operations::PublicV1OperationResult;

use super::{harness::Harness, kube::Kind};

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
