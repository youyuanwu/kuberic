use std::{fs, net::Ipv4Addr, path::Path, sync::Arc, time::Duration};

use k8s_openapi::api::core::v1::Service;
use kube::{
    api::{DeleteParams, PostParams, Preconditions},
    Api, ResourceExt,
};
use kuberic_controller::{resources, ControllerPhase, FmApi, KubericSet, ProcessFm, Reconciler};
use kuberic_tests::{
    controller as support,
    process::{application, controller::Supervisor, reconcile, reserve},
};
use serde_json::Value;
use support::{NAME, NAMESPACE};
use tokio::time::Instant;

#[tokio::test]
async fn mock_pods_drive_real_controller_fm_nodes_and_application_lifecycle() {
    lifecycle(false).await;
}

#[tokio::test]
async fn deleting_during_node_startup_finishes_real_bootstrap_without_publishing_clients() {
    lifecycle(true).await;
}

async fn lifecycle(delete_during_bootstrap: bool) {
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
    let started = loop {
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
