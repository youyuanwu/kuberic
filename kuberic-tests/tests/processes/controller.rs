use std::{fs, net::Ipv4Addr, path::Path, sync::Arc, time::Duration};

use k8s_openapi::api::core::v1::Service;
use kube::{
    api::{DeleteParams, PostParams, Preconditions},
    Api, ResourceExt,
};
use kuberic_operator::{resources, ControllerPhase, FmApi, KubericSet, ProcessFm, Reconciler};
use kuberic_tests::{
    controller as support,
    process::{
        application, assert_application_stopped, controller::Supervisor, crash_application,
        reconcile, reserve, wait_replicated,
    },
};
use serde_json::Value;
use support::{NAME, NAMESPACE};
use tokio::time::Instant;

#[tokio::test]
async fn mock_pods_drive_real_controller_fm_nodes_and_application_lifecycle() {
    lifecycle(false, Fault::None).await;
}

#[tokio::test]
async fn deleting_during_node_startup_finishes_real_bootstrap_without_publishing_clients() {
    lifecycle(true, Fault::None).await;
}

#[tokio::test]
async fn real_primary_app_crash_updates_controller_routing_without_replacing_original_cohort() {
    lifecycle(false, Fault::Primary).await;
}

#[tokio::test]
async fn real_secondary_app_crashes_preserve_controller_primary_routing_and_original_cohort() {
    lifecycle(false, Fault::Secondary).await;
}

#[tokio::test]
async fn real_planned_switchover_fences_old_writer_preserves_data_and_routes_either_secondary() {
    for candidate in [0, 2] {
        lifecycle(false, Fault::Switchover(candidate)).await;
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Fault {
    None,
    Primary,
    Secondary,
    Switchover(usize),
}

async fn lifecycle(delete_during_bootstrap: bool, fault: Fault) {
    let api = support::api::MockApi::default();
    let client = api.client(NAMESPACE);
    let fm = Arc::new(ProcessFm::new());
    let evidence = Arc::new(support::ProcessEvidence::new());
    let mut supervisor = Supervisor::new(client.clone(), api.clone(), evidence.clone());
    let mut settings = support::settings();
    settings.fm.address = reserve(Ipv4Addr::LOCALHOST).to_string();
    settings.node_port = reserve(Ipv4Addr::new(127, 0, 0, 12)).port();
    let operation_budget = Duration::from_millis(settings.teardown_timeout_ms);
    let client_port = reserve(Ipv4Addr::new(127, 0, 0, 12)).port();
    let sets: Api<KubericSet> = Api::namespaced(client.clone(), NAMESPACE);
    let set = sets
        .create(
            &PostParams::default(),
            &KubericSet::new(NAME, support::spec("/opt/fube/package".into(), client_port)),
        )
        .await
        .unwrap();
    let controller = Reconciler::new(client.clone(), settings, fm.clone(), evidence).unwrap();
    let deadline = Instant::now() + operation_budget;
    let mut started = loop {
        let result = reconcile(&controller, NAME).await.unwrap();
        assert!(
            result.blocked.is_none(),
            "{result:?}; FM={:?}",
            fm.observe().await.unwrap()
        );
        supervisor.tick(&set.uid().unwrap()).await;
        let current = sets.get(NAME).await.unwrap();
        if delete_during_bootstrap && supervisor.nodes.len() == 1 {
            assert_eq!(
                fm.observe().await.unwrap().unwrap().status.phase,
                kuberic_fm::Phase::Bootstrap
            );
            break current;
        }
        if current
            .status
            .as_ref()
            .is_some_and(|status| status.phase == ControllerPhase::Serving)
        {
            break current;
        }
        assert!(
            Instant::now() < deadline,
            "controller bootstrap deadline: {current:?}; FM={:?}; nodes={}",
            fm.observe().await.unwrap(),
            supervisor.diagnostics()
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    };
    let cohort = started.status.as_ref().unwrap().cohort().unwrap();
    let services: Api<Service> = Api::namespaced(client.clone(), NAMESPACE);
    if !delete_during_bootstrap {
        let endpoint = started
            .status
            .as_ref()
            .unwrap()
            .service_endpoint
            .as_ref()
            .unwrap();
        assert!(application(endpoint, "write").await.starts_with("OK"));
        assert!(application(endpoint, "read")
            .await
            .contains("value=acknowledged"));
        let selector = services
            .get(&resources::write_service_name(NAME))
            .await
            .unwrap()
            .spec
            .unwrap()
            .selector
            .unwrap();
        assert_eq!(
            selector.get(resources::POD_UID),
            Some(&cohort.bindings[1].pod_uid)
        );
        if let Fault::Switchover(candidate) = fault {
            let endpoint = endpoint.clone();
            wait_replicated(&endpoint).await;
            let owners: Vec<Value> = cohort
                .bindings
                .iter()
                .map(|binding| {
                    serde_json::from_slice(
                        &fs::read(
                            supervisor.configs[&binding.pod_uid]
                                .run_directory
                                .join("status.json"),
                        )
                        .unwrap(),
                    )
                    .unwrap()
                })
                .collect();
            let mut requested = started.clone();
            requested.status.as_mut().unwrap().target_primary_node_id =
                Some(cohort.allocation.nodes[candidate].node_id.clone());
            sets.replace_status(NAME, &PostParams::default(), &requested)
                .await
                .unwrap();
            let deadline = Instant::now() + operation_budget;
            started = loop {
                let outcome = reconcile(&controller, NAME).await.unwrap();
                assert!(
                    outcome.blocked.is_none(),
                    "{outcome:?}; FM={:?}; nodes={}",
                    fm.observe().await.unwrap(),
                    supervisor.diagnostics()
                );
                supervisor.tick(&set.uid().unwrap()).await;
                let current = sets.get(NAME).await.unwrap();
                let status = current.status.as_ref().unwrap();
                assert_eq!(status.cohort().as_ref(), Some(&cohort));
                if status.phase == ControllerPhase::Switching {
                    assert!(status.service_endpoint.is_none());
                    let selector = services
                        .get(&resources::write_service_name(NAME))
                        .await
                        .unwrap()
                        .spec
                        .unwrap()
                        .selector;
                    if selector != Some(resources::disabled_selector(&current).unwrap()) {
                        let native = fm.observe().await.unwrap().unwrap().status;
                        assert!(native
                            .switchover
                            .as_ref()
                            .is_some_and(|swap| swap.completed));
                        assert_eq!(
                            native.primary_node_id,
                            cohort.allocation.nodes[candidate].node_id
                        );
                        assert_eq!(
                            selector.as_ref().unwrap().get(resources::POD_UID),
                            Some(&cohort.bindings[candidate].pod_uid)
                        );
                    }
                }
                if status.phase == ControllerPhase::Serving
                    && status.primary_node_id.as_ref()
                        == Some(&cohort.allocation.nodes[candidate].node_id)
                    && status
                        .switchover
                        .as_ref()
                        .is_some_and(|swap| swap.completed)
                {
                    let next = status.service_endpoint.as_ref().unwrap();
                    assert_ne!(next, &endpoint);
                    assert!(application(&endpoint, "write-final")
                        .await
                        .contains("NotPrimary"));
                    assert!(application(next, "read")
                        .await
                        .contains("value=acknowledged"));
                    assert!(application(next, "write-more").await.starts_with("OK"));
                    assert!(application(next, "read").await.contains("value=continued"));
                    let selector = services
                        .get(&resources::write_service_name(NAME))
                        .await
                        .unwrap()
                        .spec
                        .unwrap()
                        .selector
                        .unwrap();
                    assert_eq!(
                        selector.get(resources::POD_UID),
                        Some(&cohort.bindings[candidate].pod_uid)
                    );
                    for (binding, owner) in cohort.bindings.iter().zip(&owners) {
                        let native: Value = serde_json::from_slice(
                            &fs::read(
                                supervisor.configs[&binding.pod_uid]
                                    .run_directory
                                    .join("status.json"),
                            )
                            .unwrap(),
                        )
                        .unwrap();
                        for field in [
                            "application_pid",
                            "runtime_root",
                            "replica_id",
                            "replica_instance",
                            "host_replacement_count",
                        ] {
                            assert_eq!(native[field], owner[field], "{field}: {native}");
                        }
                    }
                    break current;
                }
                assert!(
                    Instant::now() < deadline,
                    "switchover deadline: {current:?}; FM={:?}; nodes={}",
                    fm.observe().await.unwrap(),
                    supervisor.diagnostics()
                );
                tokio::time::sleep(Duration::from_millis(5)).await;
            };
        } else if fault != Fault::None {
            let cycles = if fault == Fault::Primary {
                [(1, 0), (0, 1)]
            } else {
                [(0, 1), (2, 1)]
            };
            for ((victim, primary), (before, command, after)) in cycles.into_iter().zip([
                ("acknowledged", "write-more", "continued"),
                ("continued", "write-final", "final"),
            ]) {
                let endpoint = started
                    .status
                    .as_ref()
                    .unwrap()
                    .service_endpoint
                    .as_ref()
                    .unwrap()
                    .clone();
                wait_replicated(&endpoint).await;
                let config = &supervisor.configs[&cohort.bindings[victim].pod_uid];
                let predecessor: Value = serde_json::from_slice(
                    &fs::read(config.run_directory.join("status.json")).unwrap(),
                )
                .unwrap();
                let lost = crash_application(config);
                let deadline = Instant::now() + operation_budget;
                started = loop {
                    let outcome = reconcile(&controller, NAME).await.unwrap();
                    assert!(
                        outcome.blocked.is_none(),
                        "{outcome:?}; FM={:?}",
                        fm.observe().await.unwrap()
                    );
                    supervisor.tick(&set.uid().unwrap()).await;
                    let current = sets.get(NAME).await.unwrap();
                    let status = current.status.as_ref().unwrap();
                    assert_eq!(status.cohort().as_ref(), Some(&cohort));
                    let native: Value = serde_json::from_slice(
                        &fs::read(
                            supervisor.configs[&cohort.bindings[victim].pod_uid]
                                .run_directory
                                .join("status.json"),
                        )
                        .unwrap(),
                    )
                    .unwrap();
                    if status.phase == ControllerPhase::Serving
                        && status.primary_node_id.as_ref()
                            == Some(&cohort.allocation.nodes[primary].node_id)
                        && native["replica_id"].as_i64() > predecessor["replica_id"].as_i64()
                        && native["local_configuration_complete"] == true
                    {
                        let next = status.service_endpoint.as_ref().unwrap();
                        if fault == Fault::Secondary {
                            assert_eq!(next, &endpoint);
                        }
                        assert!(application(next, "read")
                            .await
                            .contains(&format!("value={before}")));
                        assert!(application(next, command).await.starts_with("OK"));
                        assert!(application(next, "read")
                            .await
                            .contains(&format!("value={after}")));
                        let selector = services
                            .get(&resources::write_service_name(NAME))
                            .await
                            .unwrap()
                            .spec
                            .unwrap()
                            .selector
                            .unwrap();
                        assert_eq!(
                            selector.get(resources::POD_UID),
                            Some(&cohort.bindings[primary].pod_uid)
                        );
                        assert_application_stopped(lost);
                        break current;
                    }
                    assert!(
                        Instant::now() < deadline,
                        "controller failover timed out: {status:?}; FM={:?}; nodes={}",
                        fm.observe().await.unwrap(),
                        supervisor.diagnostics()
                    );
                    tokio::time::sleep(Duration::from_millis(5)).await;
                };
            }
        }
    }

    sets.delete(
        NAME,
        &DeleteParams {
            preconditions: Some(Preconditions {
                uid: started.uid(),
                resource_version: started.resource_version(),
            }),
            ..DeleteParams::default()
        },
    )
    .await
    .unwrap();
    let deletion_budget = operation_budget * if delete_during_bootstrap { 2 } else { 1 };
    let deadline = Instant::now() + deletion_budget + Duration::from_secs(5);
    let mut saw_bootstrap_wait = false;
    loop {
        let outcome = reconcile(&controller, NAME).await.unwrap();
        assert!(outcome.blocked.is_none(), "{outcome:?}");
        if outcome.resource_absent || sets.get_opt(NAME).await.unwrap().is_none() {
            break;
        }
        if delete_during_bootstrap {
            let current = sets.get(NAME).await.unwrap();
            let status = current.status.as_ref().unwrap();
            assert_ne!(status.phase, ControllerPhase::Serving);
            assert!(status.service_endpoint.is_none());
            saw_bootstrap_wait |= status.bootstrap_wait.is_some();
            if let Some(service) = services
                .get_opt(&resources::write_service_name(NAME))
                .await
                .unwrap()
            {
                assert_eq!(
                    service.spec.unwrap().selector,
                    Some(resources::disabled_selector(&current).unwrap())
                );
            }
        }
        supervisor.tick(&set.uid().unwrap()).await;
        assert!(
            Instant::now() < deadline,
            "controller-owned teardown deadline; set={:?}; FM={:?}; nodes={}",
            sets.get_opt(NAME).await.unwrap(),
            fm.observe().await.unwrap(),
            supervisor.diagnostics()
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(saw_bootstrap_wait, delete_during_bootstrap);
    assert!(services
        .get_opt(&resources::write_service_name(NAME))
        .await
        .unwrap()
        .is_none());
    assert!(sets.get_opt(NAME).await.unwrap().is_none());
    assert_eq!(supervisor.finished.len(), cohort.bindings.len());
    assert_eq!(
        fm.observe().await.unwrap().unwrap().status.phase,
        kuberic_fm::Phase::Stopped
    );
    assert!(fm.observe().await.unwrap().unwrap().joined);
    assert_eq!(fm.observe().await.unwrap().unwrap().status.deleted_nodes, 3);
    for config in supervisor.configs.values() {
        let status: Value =
            serde_json::from_slice(&fs::read(config.run_directory.join("status.json")).unwrap())
                .unwrap();
        assert_eq!(status["cleanup"]["clean"], true, "{status}");
        let pid = status["application_pid"].as_u64().unwrap();
        assert!(!Path::new("/proc").join(pid.to_string()).exists());
    }
    api.check();
}
