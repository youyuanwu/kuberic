use std::{sync::Arc, time::Duration};

use anyhow::{bail, ensure, Context, Result};
use k8s_openapi::api::core::v1::{ConfigMap, Pod, Service};
use kube::{
    api::{DeleteParams, ListParams, PostParams, Preconditions},
    Api, Client, ResourceExt,
};
use kuberic_controller::{
    resources, CleanupState, ControllerPhase, FmApi, FmAuthority, FrozenCohort, KubericSet,
    ProcessFm, Reconciler, Settings, StaleObservation, TeardownStage,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::TcpStream,
    time::Instant,
};

use crate::{processes::PodProcesses, Scenario};

const BUDGET: Duration = Duration::from_secs(180);

pub async fn run(scenario: Scenario) -> Result<()> {
    let early_delete = scenario == Scenario::DeleteDuringBootstrap;
    rustls::crypto::ring::default_provider()
        .install_default()
        .map_err(|_| anyhow::anyhow!("driver TLS provider was already installed"))?;
    let namespace = std::env::var("POD_NAMESPACE").context("POD_NAMESPACE is required")?;
    let ip = std::env::var("POD_IP").context("POD_IP is required")?;
    let image = std::env::var("KUBERIC_TEST_IMAGE").context("KUBERIC_TEST_IMAGE is required")?;
    let client = Client::try_default().await?;
    let fm = Arc::new(ProcessFm::new());
    let processes = Arc::new(PodProcesses::new(client.clone(), &namespace));
    let controller = Reconciler::new(
        client.clone(),
        Settings {
            namespace: namespace.clone(),
            fm: FmAuthority {
                id: uuid::Uuid::new_v4().simple().to_string(),
                instance: 1,
                address: format!("{ip}:19000"),
                generation: 1,
            },
            node_port: 19001,
            teardown_timeout_ms: u64::try_from(BUDGET.as_millis())?,
        },
        fm.clone(),
        processes.clone(),
    )?;
    let sets: Api<KubericSet> = Api::namespaced(client.clone(), &namespace);
    let pods: Api<Pod> = Api::namespaced(client.clone(), &namespace);
    let maps: Api<ConfigMap> = Api::namespaced(client.clone(), &namespace);
    let services: Api<Service> = Api::namespaced(client, &namespace);
    let mut desired: KubericSet = serde_json::from_str(include_str!(
        "../../kuberic-controller/examples/kubericset.json"
    ))?;
    desired.metadata.namespace = Some(namespace);
    desired.spec.image = image;
    let original = sets.create(&PostParams::default(), &desired).await?;
    let name = original.name_any();
    let deadline = Instant::now() + BUDGET;
    let mut last_phase = None;
    let started = loop {
        reconcile(&controller, &name).await?;
        let current = sets.get(&name).await?;
        let phase = current.status.as_ref().map(|status| status.phase);
        if phase != last_phase {
            println!("{}", serde_json::json!({"event": "phase", "phase": phase}));
            last_phase = phase;
        }
        if early_delete {
            if let Some(cohort) = current.status.as_ref().and_then(|status| status.cohort()) {
                let mut configured = false;
                for node in &cohort.allocation.nodes {
                    configured |= maps
                        .get(&node.config_map_name)
                        .await?
                        .data
                        .as_ref()
                        .is_some_and(|data| data.contains_key("node.json"));
                }
                if configured {
                    ensure!(
                        fm.observe()
                            .await?
                            .context("missing native FM")?
                            .status
                            .phase
                            == kuberic_fm::Phase::Bootstrap,
                        "early deletion missed native bootstrap"
                    );
                    break current;
                }
            }
        } else if phase == Some(ControllerPhase::Serving) {
            break current;
        }
        ensure!(
            Instant::now() < deadline,
            "bootstrap timed out: {current:?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    let cohort = started
        .status
        .as_ref()
        .and_then(|status| status.cohort())
        .context("missing original cohort")?;
    if !early_delete {
        let native = fm.observe().await?.context("missing native FM")?;
        ensure!(
            native.status.phase == kuberic_fm::Phase::Serving
                && started
                    .status
                    .as_ref()
                    .and_then(|status| status.service_endpoint.as_ref())
                    == native.status.service_endpoint.as_ref(),
            "controller publication preceded native configuration completion"
        );
        let service = services.get(&resources::write_service_name(&name)).await?;
        ensure!(
            service
                .spec
                .as_ref()
                .and_then(|spec| spec.selector.as_ref())
                .and_then(|selector| selector.get(resources::POD_UID))
                == Some(&cohort.bindings[1].pod_uid),
            "client Service does not select the exact original primary"
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
        ensure!(
            written.starts_with("OK"),
            "application write failed: {written}"
        );
        let read = application(&endpoint, "read").await?;
        ensure!(
            read.contains("value=acknowledged"),
            "application read failed: {read}"
        );
        println!(
            "{}",
            serde_json::json!({"event": "application_write_read", "write": written, "read": read})
        );
    }

    sets.delete(
        &name,
        &DeleteParams {
            preconditions: Some(Preconditions {
                uid: started.uid(),
                resource_version: started.resource_version(),
            }),
            ..DeleteParams::default()
        },
    )
    .await?;
    let deadline = Instant::now() + BUDGET * if early_delete { 2 } else { 1 };
    let mut bootstrap_wait = false;
    let mut staggered_shutdown = false;
    loop {
        reconcile(&controller, &name).await?;
        let Some(current) = sets.get_opt(&name).await? else {
            break;
        };
        if scenario == Scenario::StaggeredShutdown
            && !staggered_shutdown
            && current
                .status
                .as_ref()
                .and_then(|status| status.teardown.as_ref())
                .is_some_and(|teardown| teardown.stage == TeardownStage::Pods)
        {
            let first = pods.get(&cohort.allocation.nodes[0].pod_name).await?;
            ensure!(
                first.uid().as_deref() == Some(&cohort.bindings[0].pod_uid),
                "staggered shutdown cannot adopt a replacement first Pod"
            );
            if first.metadata.deletion_timestamp.is_some() {
                // Pause effects, not node signals: the first wrapper must finish
                // before reconciliation can request deletion of any remaining Pod.
                await_first_shutdown(&controller, &name, &cohort, deadline).await?;
                staggered_shutdown = true;
            }
        }
        if early_delete {
            let status = current.status.as_ref().context("missing deletion status")?;
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
            "owned teardown timed out: {current:?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    ensure!(
        bootstrap_wait == early_delete,
        "early deletion did not retain its bootstrap wait"
    );
    ensure!(
        staggered_shutdown == (scenario == Scenario::StaggeredShutdown),
        "staggered shutdown was not independently verified"
    );
    processes.verify_complete(&cohort).await?;
    verify_native_cleanup(&fm, &cohort).await?;
    let selector = ListParams::default().labels(&format!(
        "{}={}",
        resources::SET_UID,
        original.uid().context("original set UID")?
    ));
    ensure!(
        pods.list(&selector).await?.items.is_empty()
            && maps.list(&selector).await?.items.is_empty()
            && services.list(&selector).await?.items.is_empty(),
        "owned resources remain after finalizer completion"
    );
    println!(
        "{}",
        serde_json::json!({
            "event": "scenario_complete",
            "scenario": scenario.name(),
            "set_uid": original.uid(), "native_joined": true, "resources_absent": true
        })
    );
    Ok(())
}

async fn await_first_shutdown(
    controller: &Reconciler,
    name: &str,
    cohort: &FrozenCohort,
    deadline: Instant,
) -> Result<()> {
    let deadline = deadline.min(Instant::now() + Duration::from_secs(60));
    let first_uid = &cohort.bindings[0].pod_uid;
    loop {
        ensure!(
            Instant::now() < deadline,
            "first original Pod did not finish before remaining Pod deletions"
        );
        let observation = match controller.observe(name).await {
            Ok(observation) => observation,
            Err(error) if retry_observation(&error) => {
                tokio::time::sleep(Duration::from_millis(50)).await;
                continue;
            }
            Err(error) => return Err(error),
        };
        ensure!(
            observation.set.uid().as_deref() == Some(&cohort.allocation.resource_uid)
                && observation.set.metadata.deletion_timestamp.is_some()
                && observation
                    .set
                    .status
                    .as_ref()
                    .and_then(|status| status.teardown.as_ref())
                    .is_some_and(|teardown| teardown.stage == TeardownStage::Pods
                        && teardown.cohort == *cohort),
            "staggered shutdown lost original teardown authority"
        );
        let native = observation
            .fm
            .as_ref()
            .context("missing original native owner")?;
        ensure!(
            native.cohort == *cohort
                && native.status.phase == kuberic_fm::Phase::AwaitingNodeShutdown
                && native.status.deleted_nodes == cohort.bindings.len(),
            "staggered Pod termination preceded native deletion completion"
        );
        let mut remaining_running = true;
        for (pod, binding) in observation.pods.iter().zip(&cohort.bindings).skip(1) {
            let pod = pod.as_ref().context("remaining original Pod disappeared")?;
            ensure!(
                pod.uid().as_deref() == Some(&binding.pod_uid)
                    && pod.metadata.deletion_timestamp.is_none(),
                "remaining original Pod was replaced or deletion started before first cleanup"
            );
            let state = pod
                .status
                .as_ref()
                .and_then(|status| status.container_statuses.as_ref())
                .and_then(|containers| containers.iter().find(|container| container.name == "node"))
                .and_then(|container| container.state.as_ref());
            ensure!(
                state.is_none_or(|state| state.terminated.is_none()),
                "remaining node terminated before its Pod deletion"
            );
            remaining_running &= state.is_some_and(|state| state.running.is_some());
        }
        let processes = observation
            .processes
            .as_ref()
            .context("missing physical cleanup observation")?;
        ensure!(
            processes.cohort == *cohort,
            "staggered cleanup observation changed the original cohort"
        );
        let first = processes
            .nodes
            .iter()
            .find(|node| &node.pod_uid == first_uid)
            .context("missing first original cleanup observation")?;
        match &first.outcome {
            CleanupState::Complete(receipt) => {
                receipt.validate()?;
                if remaining_running {
                    println!(
                        "{}",
                        serde_json::json!({
                            "event": "staggered_node_shutdown",
                            "stopped_pod_uid": first_uid,
                            "running_pod_uids": cohort.bindings.iter().skip(1)
                                .map(|binding| &binding.pod_uid).collect::<Vec<_>>()
                        })
                    );
                    return Ok(());
                }
            }
            CleanupState::Failed(reason) => bail!("first original cleanup failed: {reason}"),
            CleanupState::Pending => {}
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn verify_native_cleanup(fm: &ProcessFm, cohort: &FrozenCohort) -> Result<()> {
    let native = fm
        .observe()
        .await?
        .context("missing original native owner")?;
    ensure!(
        native.cohort == *cohort
            && native.joined
            && native.status.phase == kuberic_fm::Phase::Stopped
            && native.status.deleted_nodes == cohort.bindings.len(),
        "original native deletion or FM joining is incomplete: {native:?}"
    );
    Ok(())
}

async fn reconcile(controller: &Reconciler, name: &str) -> Result<()> {
    match controller.reconcile(name).await {
        Ok(outcome) => ensure!(outcome.blocked.is_none(), "controller blocked: {outcome:?}"),
        Err(error) if retry_observation(&error) => {}
        Err(error) => return Err(error),
    }
    Ok(())
}

fn retry_observation(error: &anyhow::Error) -> bool {
    let retry = error.is::<StaleObservation>()
        || matches!(error.downcast_ref::<kube::Error>(),
            Some(kube::Error::Api(response)) if response.code == 409 && response.reason == "Conflict");
    if retry {
        eprintln!("Reobserving concurrent controller/Kubernetes update: {error}");
    }
    retry
}

async fn wait_for_service(endpoint: &str) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        match tokio::time::timeout(Duration::from_secs(2), TcpStream::connect(endpoint)).await {
            Ok(Ok(_)) => return Ok(()),
            result if Instant::now() >= deadline => {
                anyhow::bail!("primary Service did not become reachable: {endpoint}: {result:?}")
            }
            result => eprintln!("Waiting for primary Service dataplane: {result:?}"),
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

async fn application(endpoint: &str, command: &str) -> Result<String> {
    tokio::time::timeout(Duration::from_secs(5), async {
        let mut stream = TcpStream::connect(endpoint).await?;
        stream.write_all(format!("{command}\n").as_bytes()).await?;
        let mut response = String::new();
        BufReader::new(stream).read_line(&mut response).await?;
        Ok(response)
    })
    .await
    .context("application request timed out")?
}
