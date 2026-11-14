use anyhow::{bail, ensure, Context, Result};
use kube::ResourceExt;
use kuberic_fm::Phase;

use crate::{
    plan::{admitted, component, metadata, owned, pod_spec_matches, validate_allocation},
    resources::{self, NODE_ID, POD_UID},
    CleanupState, ControllerPhase, FmObservation, FrozenCohort, NodeCleanup, Observation, Plan,
    Settings, TeardownStage,
};

pub(crate) fn process_nodes<'a>(
    observation: &'a Observation,
    cohort: &FrozenCohort,
) -> Result<Option<&'a [NodeCleanup]>> {
    ensure!(
        observation.process_allocation.as_ref() == Some(&cohort.allocation),
        "original process allocation is unavailable"
    );
    let Some(processes) = &observation.processes else {
        return Ok(None);
    };
    ensure!(
        processes.cohort == *cohort
            && processes.nodes.len() == cohort.bindings.len()
            && cohort.bindings.iter().all(|binding| {
                processes
                    .nodes
                    .iter()
                    .filter(|node| node.pod_uid == binding.pod_uid)
                    .count()
                    == 1
            }),
        "process cleanup evidence belongs to a different or incomplete original cohort"
    );
    for node in &processes.nodes {
        match &node.outcome {
            CleanupState::Pending => {}
            CleanupState::Complete(receipt) => receipt.validate()?,
            CleanupState::Failed(error) => {
                bail!("original Pod {} cleanup failed: {error}", node.pod_uid)
            }
        }
    }
    Ok(Some(&processes.nodes))
}

fn native<'a>(observation: &'a Observation, cohort: &FrozenCohort) -> Result<&'a FmObservation> {
    let report = observation
        .fm
        .as_ref()
        .context("original native FM session is unavailable")?;
    ensure!(
        report.cohort == *cohort,
        "native teardown evidence changed original authority"
    );
    report.primary_index()?;
    ensure!(
        report.status.phase != Phase::ControlUnknown && report.status.error.is_none(),
        "native teardown uncertainty: {}",
        report
            .status
            .error
            .as_deref()
            .unwrap_or("original control state is unknown")
    );
    Ok(report)
}

fn resources(
    observation: &Observation,
    cohort: &FrozenCohort,
    service_uid: &str,
    stage: TeardownStage,
    begin: bool,
) -> Result<()> {
    cohort.fm_config()?;
    let allocation = &cohort.allocation;
    for (index, binding) in cohort.bindings.iter().enumerate() {
        ensure!(
            component(&binding.pod_uid) && component(&binding.config_map_uid),
            "invalid original resource UID"
        );
        if let Some(pod) = &observation.pods[index] {
            metadata(
                &pod.metadata,
                &observation.set,
                allocation,
                stage != TeardownStage::NativeDeletion,
            )?;
            let expected = resources::pod(&observation.set, allocation, index)?;
            let expected_spec = expected.spec.as_ref().context("missing desired Pod spec")?;
            ensure!(
                pod.uid().as_deref() == Some(&binding.pod_uid)
                    && pod.labels().get(POD_UID) == Some(&binding.pod_uid)
                    && pod.labels().get(NODE_ID) == Some(&allocation.nodes[index].node_id)
                    && pod
                        .spec
                        .as_ref()
                        .is_some_and(|actual| pod_spec_matches(actual, expected_spec))
                    && pod.status.as_ref().is_none_or(|status| {
                        status.container_statuses.as_ref().is_none_or(|containers| {
                            containers
                                .iter()
                                .all(|container| container.restart_count == 0)
                        })
                    }),
                "teardown cannot adopt a changed Pod UID, process or spec"
            );
            if stage == TeardownStage::NativeDeletion {
                ensure!(
                    pod.status
                        .as_ref()
                        .and_then(|status| status.pod_ip.as_deref())
                        == Some(&binding.ip),
                    "original Pod IP changed before native deletion"
                );
            }
        } else {
            ensure!(
                stage != TeardownStage::NativeDeletion,
                "original Pod disappeared before native deletion completed"
            );
        }
        if let Some(map) = &observation.config_maps[index] {
            metadata(
                &map.metadata,
                &observation.set,
                allocation,
                stage != TeardownStage::NativeDeletion,
            )?;
            ensure!(
                map.uid().as_deref() == Some(&binding.config_map_uid),
                "teardown cannot delete a replacement ConfigMap"
            );
            let expected = serde_json::to_string(&resources::runner_config(cohort, index)?)?;
            ensure!(
                map.immutable == Some(true)
                    && map.data.as_ref().is_some_and(
                        |data| data.len() == 1 && data.get("node.json") == Some(&expected)
                    )
                    && map
                        .binary_data
                        .as_ref()
                        .is_none_or(std::collections::BTreeMap::is_empty),
                "original immutable node configuration changed"
            );
        } else {
            ensure!(
                !begin,
                "original ConfigMap is missing before teardown acceptance"
            );
        }
    }
    if let Some(service) = &observation.service {
        metadata(
            &service.metadata,
            &observation.set,
            allocation,
            stage != TeardownStage::NativeDeletion,
        )?;
        ensure!(
            service.uid().as_deref() == Some(service_uid),
            "teardown cannot delete a replacement Service"
        );
    } else {
        ensure!(
            !begin,
            "original Service is missing before teardown acceptance"
        );
    }
    if matches!(stage, TeardownStage::Resources | TeardownStage::Complete) {
        ensure!(
            observation.pods.iter().all(Option::is_none),
            "an original or replacement Pod appeared after process cleanup"
        );
    }
    Ok(())
}

fn disabled(observation: &Observation) -> Result<Option<Plan>> {
    let Some(service) = &observation.service else {
        return Ok(None);
    };
    let selector = resources::disabled_selector(&observation.set)?;
    if service
        .spec
        .as_ref()
        .context("owned Service has no spec")?
        .selector
        .as_ref()
        != Some(&selector)
    {
        return Ok(Some(Plan::Route(selector)));
    }
    Ok(None)
}

fn validate_window(now: u64, started: u64, deadline: u64) -> Result<()> {
    ensure!(
        now >= started
            && deadline
                .checked_sub(started)
                .is_some_and(|duration| (100..=600_000).contains(&duration)),
        "operation clock or retained deadline changed"
    );
    Ok(())
}

pub(crate) fn evaluate(observation: &Observation, settings: &Settings) -> Result<Plan> {
    let set = &observation.set;
    let finalizer = set
        .finalizers()
        .iter()
        .any(|finalizer| finalizer == resources::FINALIZER);
    let status = set.status.as_ref();
    let allocation = status.and_then(|status| status.allocation.as_ref());
    if allocation.is_none() {
        let owned_resource = observation
            .pods
            .iter()
            .flatten()
            .any(|pod| owned(&pod.metadata, set))
            || observation
                .config_maps
                .iter()
                .flatten()
                .any(|map| owned(&map.metadata, set))
            || observation
                .service
                .as_ref()
                .is_some_and(|service| owned(&service.metadata, set));
        let native_owner = observation.fm.as_ref().is_some_and(|report| {
            Some(&report.cohort.allocation.resource_uid) == set.metadata.uid.as_ref()
        });
        ensure!(
            !owned_resource && !native_owner,
            "missing allocation cannot certify existing owned resources or native cleanup"
        );
        return Ok(if finalizer {
            Plan::RemoveFinalizer
        } else {
            Plan::Wait
        });
    }
    let status = status.context("missing accepted status")?;
    if status.phase == ControllerPhase::Blocked {
        return Ok(Plan::Blocked(
            status
                .reason
                .clone()
                .context("blocked teardown has no reason")?,
        ));
    }
    if status.teardown.is_none()
        && (status.bootstrap_wait.is_some()
            || !observation
                .fm
                .as_ref()
                .is_some_and(|report| report.status.phase == Phase::Serving))
    {
        ensure!(finalizer, "owned teardown finalizer disappeared");
        let bootstrap = crate::plan::bootstrap(observation, settings)?;
        ensure!(
            observation.fm.as_ref().is_none_or(|report| !matches!(
                report.status.phase,
                Phase::Deleting | Phase::AwaitingNodeShutdown | Phase::Stopped
            )),
            "native shutdown began before accepted bootstrap completed"
        );
        let Some(wait) = &status.bootstrap_wait else {
            return Ok(Plan::AwaitBootstrap);
        };
        validate_window(
            observation.now_unix_ms,
            wait.started_unix_ms,
            wait.deadline_unix_ms,
        )?;
        ensure!(
            observation.now_unix_ms < wait.deadline_unix_ms,
            "bootstrap completion deadline exceeded; cleanup requires intervention"
        );
        if let Some(plan) = disabled(observation)? {
            return Ok(plan);
        }
        if bootstrap != Plan::Wait {
            return Ok(bootstrap);
        }
        if !observation
            .fm
            .as_ref()
            .is_some_and(|report| report.status.phase == Phase::Serving)
        {
            return Ok(Plan::Wait);
        }
    }
    let cohort = status
        .cohort()
        .context("native teardown requires the original frozen cohort")?;
    validate_allocation(
        set,
        &cohort.allocation,
        settings,
        status.teardown.is_none() && status.bootstrap_wait.is_none(),
    )?;
    admitted(observation, &cohort.allocation)?;
    let report = native(observation, &cohort)?;
    let nodes = process_nodes(observation, &cohort)?;
    let Some(teardown) = &status.teardown else {
        ensure!(finalizer, "owned teardown finalizer disappeared");
        ensure!(
            report.status.phase == Phase::Serving,
            "only completed healthy native create admits controller teardown"
        );
        ensure!(
            nodes.is_none_or(|nodes| nodes
                .iter()
                .all(|node| matches!(node.outcome, CleanupState::Pending))),
            "a process exited before teardown acceptance"
        );
        let service_uid = observation
            .service
            .as_ref()
            .context("missing original Service")?
            .uid()
            .context("Service has no UID")?;
        resources(
            observation,
            &cohort,
            &service_uid,
            TeardownStage::NativeDeletion,
            true,
        )?;
        return Ok(Plan::BeginTeardown {
            cohort,
            service_uid,
        });
    };
    ensure!(
        teardown.cohort == cohort && component(&teardown.service_uid),
        "retained teardown changed the original frozen cohort"
    );
    validate_window(
        observation.now_unix_ms,
        teardown.started_unix_ms,
        teardown.deadline_unix_ms,
    )?;
    ensure!(
        finalizer || teardown.stage == TeardownStage::Complete,
        "owned teardown finalizer disappeared before cleanup completion"
    );
    ensure!(
        teardown.stage == TeardownStage::Complete
            || observation.now_unix_ms < teardown.deadline_unix_ms,
        "owned teardown deadline exceeded; cleanup remains unconfirmed"
    );
    resources(
        observation,
        &cohort,
        &teardown.service_uid,
        teardown.stage,
        false,
    )?;
    if let Some(plan) = disabled(observation)? {
        return Ok(plan);
    }
    match teardown.stage {
        TeardownStage::NativeDeletion => match report.status.phase {
            Phase::Serving => Ok(Plan::ShutdownFm(cohort)),
            Phase::Deleting => Ok(Plan::Wait),
            Phase::AwaitingNodeShutdown | Phase::Stopped => {
                ensure!(
                    report.status.deleted_nodes == 3,
                    "native deletion completion omitted original replicas"
                );
                Ok(Plan::AdvanceTeardown(TeardownStage::Pods))
            }
            _ => bail!("native create/deletion authority is incomplete"),
        },
        TeardownStage::Pods => {
            ensure!(
                matches!(
                    report.status.phase,
                    Phase::AwaitingNodeShutdown | Phase::Stopped
                ) && report.status.deleted_nodes == 3,
                "retained native deletion receipt lost original authority"
            );
            for (index, pod) in observation.pods.iter().enumerate() {
                if pod
                    .as_ref()
                    .is_some_and(|pod| pod.metadata.deletion_timestamp.is_none())
                {
                    return Ok(Plan::DeletePod(index));
                }
            }
            if observation.pods.iter().any(Option::is_some)
                || nodes.is_none_or(|nodes| {
                    nodes
                        .iter()
                        .any(|node| matches!(node.outcome, CleanupState::Pending))
                })
            {
                return Ok(Plan::Wait);
            }
            if report.status.phase != Phase::Stopped {
                return Ok(Plan::Wait);
            }
            if !report.joined {
                return Ok(Plan::JoinFm(cohort));
            }
            Ok(Plan::AdvanceTeardown(TeardownStage::Resources))
        }
        TeardownStage::Resources | TeardownStage::Complete => {
            ensure!(
                report.status.phase == Phase::Stopped
                    && report.joined
                    && report.status.deleted_nodes == 3,
                "native FM shutdown/join is not complete"
            );
            ensure!(
                nodes.is_some_and(|nodes| nodes
                    .iter()
                    .all(|node| matches!(node.outcome, CleanupState::Complete(_)))),
                "original backend process cleanup evidence is unavailable or incomplete"
            );
            for (index, map) in observation.config_maps.iter().enumerate() {
                if let Some(map) = map {
                    ensure!(
                        teardown.stage != TeardownStage::Complete,
                        "ConfigMap reappeared after teardown completion"
                    );
                    return Ok(if map.metadata.deletion_timestamp.is_none() {
                        Plan::DeleteConfigMap(index)
                    } else {
                        Plan::Wait
                    });
                }
            }
            if let Some(service) = &observation.service {
                ensure!(
                    teardown.stage != TeardownStage::Complete,
                    "Service reappeared after teardown completion"
                );
                return Ok(if service.metadata.deletion_timestamp.is_none() {
                    Plan::DeleteService
                } else {
                    Plan::Wait
                });
            }
            if teardown.stage == TeardownStage::Resources {
                Ok(Plan::AdvanceTeardown(TeardownStage::Complete))
            } else {
                Ok(if finalizer {
                    Plan::RemoveFinalizer
                } else {
                    Plan::Wait
                })
            }
        }
    }
}
