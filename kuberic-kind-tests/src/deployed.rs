use std::time::Duration;

use anyhow::{ensure, Context, Result};
use k8s_openapi::api::core::v1::{ConfigMap, Pod, Service};
use kube::{
    api::{DeleteParams, ListParams, PostParams, Preconditions},
    Api, Client, ResourceExt,
};
use kuberic_operator::{resources, ControllerPhase, KubericSet};
use tokio::time::Instant;

use crate::{
    driver::{application, wait_for_service},
    Scenario,
};

pub async fn run(scenario: Scenario) -> Result<()> {
    rustls::crypto::ring::default_provider()
        .install_default()
        .map_err(|_| anyhow::anyhow!("driver TLS provider was already installed"))?;
    let namespace = std::env::var("POD_NAMESPACE").context("POD_NAMESPACE is required")?;
    let image = std::env::var("KUBERIC_TEST_IMAGE").context("KUBERIC_TEST_IMAGE is required")?;
    let client = Client::try_default().await?;
    let sets: Api<KubericSet> = Api::namespaced(client.clone(), &namespace);
    let services: Api<Service> = Api::namespaced(client.clone(), &namespace);
    let mut desired: KubericSet = serde_json::from_str(include_str!(
        "../../kuberic-operator/examples/kubericset.json"
    ))?;
    desired.metadata.namespace = Some(namespace.clone());
    desired.spec.image = image;
    let original = sets.create(&PostParams::default(), &desired).await?;
    let name = original.name_any();
    let uid = original.uid().context("missing original KubericSet UID")?;
    let early = scenario == Scenario::DeleteDuringBootstrap;
    let deadline = Instant::now() + Duration::from_secs(180);
    let mut last_phase = None;
    let started = loop {
        let current = sets.get(&name).await?;
        ensure!(
            current.uid().as_deref() == Some(&uid),
            "original KubericSet changed"
        );
        let status = current.status.as_ref();
        let phase = status.map(|status| status.phase);
        ensure!(
            phase != Some(ControllerPhase::Blocked),
            "operator blocked: {status:?}"
        );
        if phase != last_phase {
            println!("{}", serde_json::json!({"event": "phase", "phase": phase}));
            last_phase = phase;
        }
        if early && status.and_then(|status| status.cohort()).is_some() {
            ensure!(
                phase != Some(ControllerPhase::Serving)
                    && status.is_some_and(|status| status.service_endpoint.is_none()),
                "early deletion missed accepted bootstrap"
            );
            break current;
        }
        if !early && phase == Some(ControllerPhase::Serving) {
            break current;
        }
        ensure!(
            Instant::now() < deadline,
            "deployed bootstrap timed out: {current:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    let cohort = started
        .status
        .as_ref()
        .and_then(|status| status.cohort())
        .context("missing accepted original cohort")?;
    if !early {
        ensure!(
            started
                .status
                .as_ref()
                .and_then(|status| status.service_endpoint.as_ref())
                .is_some_and(|endpoint| !endpoint.is_empty()),
            "Serving omitted the native endpoint"
        );
        let service = services.get(&resources::write_service_name(&name)).await?;
        ensure!(
            service
                .spec
                .as_ref()
                .and_then(|spec| spec.selector.as_ref())
                .and_then(|selector| selector.get(resources::POD_UID))
                == Some(&cohort.bindings[1].pod_uid),
            "Service does not select the original primary Pod UID"
        );
        let endpoint = format!(
            "{}:{}",
            service
                .spec
                .context("Service spec")?
                .cluster_ip
                .context("Service ClusterIP")?,
            started.spec.client_port
        );
        wait_for_service(&endpoint).await?;
        let written = application(&endpoint, "write").await?;
        let read = application(&endpoint, "read").await?;
        ensure!(
            written.starts_with("OK") && read.contains("value=acknowledged"),
            "application write/read failed: {written:?}; {read:?}"
        );
        println!(
            "{}",
            serde_json::json!({
                "event": "application_write_read", "write": written, "read": read
            })
        );
        if scenario == Scenario::PrimaryAppFailover {
            let mut previous_endpoint = started
                .status
                .as_ref()
                .and_then(|status| status.service_endpoint.clone());
            for (cycle, victim, primary, before, command, after) in [
                (0, 1, 0, "acknowledged", "write-more", "continued"),
                (1, 0, 1, "continued", "write-final", "final"),
            ] {
                let deadline = Instant::now() + Duration::from_secs(30);
                loop {
                    let state: serde_json::Value =
                        serde_json::from_str(&application(&endpoint, "state").await?)?;
                    let peers = state["peers"].as_array().context("replication peers")?;
                    let last = state["provider"]["last"].as_i64().context("provider LSN")?;
                    if peers.len() == 2
                        && last >= 1
                        && peers
                            .iter()
                            .all(|peer| peer["applied"].as_i64().is_some_and(|lsn| lsn >= last))
                    {
                        break;
                    }
                    ensure!(
                        Instant::now() < deadline,
                        "original peers did not apply: {state}"
                    );
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                println!(
                    "{}",
                    serde_json::json!({
                        "event": "failover_ready", "set_uid": uid, "cohort": cohort,
                        "cycle": cycle, "victim": victim
                    })
                );
                let deadline = Instant::now() + Duration::from_secs(180);
                let promoted = loop {
                    let current = sets.get(&name).await?;
                    let status = current.status.as_ref().context("failover status")?;
                    ensure!(
                        current.uid().as_deref() == Some(&uid)
                            && status.cohort().as_ref() == Some(&cohort)
                            && status.phase != ControllerPhase::Blocked,
                        "failover changed original authority or blocked: {current:?}"
                    );
                    if status.phase == ControllerPhase::Serving
                        && status.primary_node_id.as_ref()
                            == Some(&cohort.allocation.nodes[primary].node_id)
                    {
                        break current;
                    }
                    ensure!(
                        Instant::now() < deadline,
                        "deployed failover timed out: {current:?}"
                    );
                    tokio::time::sleep(Duration::from_millis(20)).await;
                };
                let service = services.get(&resources::write_service_name(&name)).await?;
                ensure!(
                    service
                        .spec
                        .as_ref()
                        .and_then(|spec| spec.selector.as_ref())
                        .and_then(|selector| selector.get(resources::POD_UID))
                        == Some(&cohort.bindings[primary].pod_uid)
                        && promoted
                            .status
                            .as_ref()
                            .and_then(|status| status.service_endpoint.as_ref())
                            != previous_endpoint.as_ref(),
                    "failover did not publish the new original primary"
                );
                wait_for_service(&endpoint).await?;
                let preserved = application(&endpoint, "read").await?;
                let written = application(&endpoint, command).await?;
                let continued = application(&endpoint, "read").await?;
                ensure!(
                preserved.contains(&format!("value={before}"))
                    && written.starts_with("OK")
                    && continued.contains(&format!("value={after}")),
                "failover lost data or stopped writes: {preserved:?}; {written:?}; {continued:?}"
            );
                for (node, binding) in cohort.allocation.nodes.iter().zip(&cohort.bindings) {
                    let pod = Api::<Pod>::namespaced(client.clone(), &namespace)
                        .get(&node.pod_name)
                        .await?;
                    ensure!(
                        pod.uid().as_deref() == Some(&binding.pod_uid)
                            && pod.metadata.deletion_timestamp.is_none()
                            && pod
                                .status
                                .as_ref()
                                .is_some_and(|status| status.phase.as_deref() == Some("Running")
                                    && status.container_statuses.as_ref().is_some_and(
                                        |containers| containers.iter().all(|container| container
                                            .restart_count
                                            == 0
                                            && container
                                                .state
                                                .as_ref()
                                                .is_some_and(|state| state.running.is_some()))
                                    )),
                        "application failover replaced or restarted an original node: {pod:?}"
                    );
                }
                println!(
                    "{}",
                    serde_json::json!({
                        "event": "primary_app_failover", "set_uid": uid, "cohort": cohort,
                        "primary_node_id": cohort.allocation.nodes[primary].node_id,
                        "cycle": cycle,
                        "data_preserved": true, "writes_continued": true, "original_nodes_unchanged": true
                    })
                );
                previous_endpoint = promoted
                    .status
                    .as_ref()
                    .and_then(|status| status.service_endpoint.clone());
                println!(
                    "{}",
                    serde_json::json!({
                        "event": "repair_complete", "set_uid": uid, "cohort": cohort,
                        "cycle": cycle, "victim": victim
                    })
                );
                let maps: Api<ConfigMap> = Api::namespaced(client.clone(), &namespace);
                let deadline = Instant::now() + Duration::from_secs(60);
                loop {
                    if let Some(receipt) = maps.get_opt(&format!("host-restored-{cycle}")).await? {
                        ensure!(
                            receipt.data.as_ref().and_then(|data| data.get("setUid")) == Some(&uid),
                            "host restoration evidence changed cohort"
                        );
                        break;
                    }
                    ensure!(
                        Instant::now() < deadline,
                        "host did not verify physical restoration"
                    );
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            }
        }
    }
    sets.delete(
        &name,
        &DeleteParams {
            preconditions: Some(Preconditions {
                uid: Some(uid.clone()),
                ..Default::default()
            }),
            ..Default::default()
        },
    )
    .await?;
    let deadline = Instant::now() + Duration::from_secs(if early { 360 } else { 180 });
    let mut bootstrap_wait = false;
    loop {
        let Some(current) = sets.get_opt(&name).await? else {
            break;
        };
        ensure!(
            current.uid().as_deref() == Some(&uid),
            "deletion adopted another KubericSet"
        );
        let status = current
            .status
            .as_ref()
            .context("missing accepted deletion status")?;
        ensure!(
            status.phase != ControllerPhase::Blocked,
            "operator blocked teardown: {status:?}"
        );
        if early {
            ensure!(
                status.phase != ControllerPhase::Serving && status.service_endpoint.is_none(),
                "deleting bootstrap published client access"
            );
            bootstrap_wait |= status.bootstrap_wait.is_some();
            if let Some(service) = services
                .get_opt(&resources::write_service_name(&name))
                .await?
            {
                ensure!(
                    service.spec.context("Service spec")?.selector
                        == Some(resources::disabled_selector(&current)?),
                    "deleting bootstrap enabled client routing"
                );
            }
        }
        ensure!(
            Instant::now() < deadline,
            "deployed teardown timed out: {current:?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    ensure!(
        bootstrap_wait == early,
        "early deletion omitted accepted bootstrap completion"
    );
    let selector = ListParams::default().labels(&format!("{}={uid}", resources::SET_UID));
    ensure!(
        Api::<Pod>::namespaced(client.clone(), &namespace)
            .list(&selector)
            .await?
            .items
            .is_empty()
            && Api::<ConfigMap>::namespaced(client, &namespace)
                .list(&selector)
                .await?
                .items
                .is_empty()
            && services.list(&selector).await?.items.is_empty(),
        "owned resources remain after parent finalization"
    );
    println!(
        "{}",
        serde_json::json!({
            "event": "scenario_complete", "scenario": scenario.name(), "mode": "deployed-operator",
            "set_uid": uid, "cohort": cohort, "resources_absent": true
        })
    );
    Ok(())
}
