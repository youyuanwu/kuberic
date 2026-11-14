mod support;

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    net::{Ipv4Addr, SocketAddrV4},
    path::{Path, PathBuf},
    process::Command,
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result};
use fube_local_host::process::OwnedApp;
use k8s_openapi::api::core::v1::{ConfigMap, Pod, Service};
use kube::{
    api::{DeleteParams, ListParams, PostParams, Preconditions},
    Api, Client, ResourceExt,
};
use kuberic_controller::{
    resources, CleanupReceipt, CleanupState, ControllerPhase, FmApi, KubericSet, ProcessFm,
    Reconciler, TeardownStage,
};
use serde_json::Value;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::TcpStream,
    time::Instant,
};

use support::{NAME, NAMESPACE};

fn reserve(ip: Ipv4Addr) -> SocketAddrV4 {
    let listener = std::net::TcpListener::bind((ip, 0)).unwrap();
    match listener.local_addr().unwrap() {
        std::net::SocketAddr::V4(address) => address,
        _ => unreachable!(),
    }
}

fn binary(name: &str, variable: &str) -> PathBuf {
    std::env::var_os(variable)
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .parent()
                .unwrap()
                .join("target/debug")
                .join(name)
        })
        .canonicalize()
        .unwrap_or_else(|error| panic!("missing prebuilt {name}: {error}; see root README"))
}

struct Supervisor {
    client: Client,
    executable: PathBuf,
    package: PathBuf,
    nodes: BTreeMap<String, OwnedApp>,
    configs: BTreeMap<String, fube_node_runner::Config>,
    terminating: BTreeSet<String>,
    finished: BTreeSet<String>,
    evidence: Arc<support::ProcessEvidence>,
    api: support::api::MockApi,
    root: tempfile::TempDir,
}

impl Supervisor {
    async fn tick(&mut self, resource_uid: &str) {
        let pods: Api<Pod> = Api::namespaced(self.client.clone(), NAMESPACE);
        let maps: Api<ConfigMap> = Api::namespaced(self.client.clone(), NAMESPACE);
        let parent = Api::<KubericSet>::namespaced(self.client.clone(), NAMESPACE)
            .get(NAME)
            .await
            .unwrap();
        let cohort = parent.status.as_ref().and_then(|status| status.cohort());
        for pod in pods
            .list(&ListParams::default().labels(&format!("{}={resource_uid}", resources::SET_UID)))
            .await
            .unwrap()
        {
            let uid = pod.uid().unwrap();
            if self.finished.contains(&uid) {
                continue;
            }
            if pod.metadata.deletion_timestamp.is_some() {
                let cohort = cohort.as_ref().expect("exact cohort before Pod deletion");
                assert!(parent
                    .status
                    .as_ref()
                    .unwrap()
                    .teardown
                    .as_ref()
                    .is_some_and(|teardown| teardown.stage == TeardownStage::Pods));
                let index = cohort
                    .bindings
                    .iter()
                    .position(|binding| binding.pod_uid == uid)
                    .unwrap();
                let child = self
                    .nodes
                    .get_mut(&uid)
                    .expect("original owned node process");
                if self.terminating.insert(uid.clone()) {
                    assert!(Command::new("kill")
                        .args(["-TERM", &child.pid().to_string()])
                        .status()
                        .unwrap()
                        .success());
                }
                if let Some(exit) = child.observed_exit().unwrap() {
                    let config = self.configs.get(&uid).unwrap();
                    match cleanup_receipt(child, config, exit) {
                        Ok(receipt) => {
                            self.evidence
                                .publish(cohort, index, CleanupState::Complete(receipt))
                                .await;
                            self.api.finalize(
                                NAMESPACE,
                                support::api::Kind::Pod,
                                &pod.name_any(),
                                &uid,
                            );
                        }
                        Err(error) => {
                            self.evidence
                                .publish(cohort, index, CleanupState::Failed(format!("{error:#}")))
                                .await;
                        }
                    }
                    self.finished.insert(uid);
                }
                continue;
            }
            if pod
                .status
                .as_ref()
                .and_then(|status| status.pod_ip.as_ref())
                .is_none()
            {
                let slot: u8 = pod.name_any().rsplit('-').next().unwrap().parse().unwrap();
                support::publish_ip(&self.client, &pod, &format!("127.0.0.{}", slot + 11), true)
                    .await;
                continue;
            }
            if self.nodes.contains_key(&uid) {
                continue;
            }
            let map = maps.get(&pod.name_any()).await.unwrap();
            let Some(json) = map.data.as_ref().and_then(|data| data.get("node.json")) else {
                continue;
            };
            assert_eq!(map.immutable, Some(true));
            let mut config: fube_node_runner::Config = serde_json::from_str(json).unwrap();
            config.validate().unwrap();
            assert_eq!(config.run_directory.file_name().unwrap(), uid.as_str());
            assert_eq!(
                config.package.to_str(),
                Some(parent.spec.package_path.as_str())
            );
            let stored =
                serde_json::to_string(&parent).unwrap() + &serde_json::to_string(&map).unwrap();
            assert!(
                !stored.contains(self.root.path().to_str().unwrap()),
                "host fixture paths leaked into Kubernetes objects"
            );
            let relative = config
                .run_directory
                .strip_prefix(resources::RUN_ROOT)
                .unwrap()
                .to_path_buf();
            config.run_directory = self.root.path().join("run").join(relative);
            config.package = self.package.clone();
            config.validate().unwrap();
            fs::create_dir_all(config.run_directory.parent().unwrap()).unwrap();
            let path = self.root.path().join(format!("{uid}.json"));
            fs::write(&path, serde_json::to_vec(&config).unwrap()).unwrap();
            let mut command = Command::new(&self.executable);
            command.arg("--config").arg(path);
            let mut child = None;
            OwnedApp::start(&mut child, command).unwrap();
            self.nodes.insert(uid.clone(), child.unwrap());
            self.configs.insert(uid, config);
        }
        for (uid, node) in &mut self.nodes {
            if self.finished.contains(uid) || self.terminating.contains(uid) {
                continue;
            }
            node.check_alive()
                .unwrap_or_else(|error| panic!("{error}: {:?}", node.logs()));
        }
    }
}

fn cleanup_receipt(
    child: &mut OwnedApp,
    config: &fube_node_runner::Config,
    exit: std::process::ExitStatus,
) -> Result<CleanupReceipt> {
    let cleanup = child.cleanup()?;
    let status: Value =
        serde_json::from_slice(&fs::read(config.run_directory.join("status.json"))?)?;
    let pid = status["application_pid"]
        .as_u64()
        .context("missing original application PID")?;
    let receipt = CleanupReceipt {
        node_reaped: child.status().reaped && cleanup["reaped"] == true,
        node_exit_success: exit.success(),
        application_reaped: status["cleanup"]["application_cleaned"] == true
            && !Path::new("/proc").join(pid.to_string()).exists(),
        resources_joined: child.status().group_drained
            && status["stage"] == "stopped"
            && status["cleanup"]["clean"] == true
            && status["cleanup"]["executor_closed"] == true
            && status["cleanup"]["store_closed"] == true
            && status["cleanup"]["channels_closed"] == true,
        forced: cleanup["forced"]
            .as_bool()
            .context("missing owned group cleanup result")?,
    };
    receipt.validate()?;
    Ok(receipt)
}

async fn application(endpoint: &str, command: &str) -> String {
    tokio::time::timeout(Duration::from_secs(3), async {
        let mut stream = TcpStream::connect(endpoint).await.unwrap();
        stream
            .write_all(format!("{command}\n").as_bytes())
            .await
            .unwrap();
        let mut response = String::new();
        BufReader::new(stream)
            .read_line(&mut response)
            .await
            .unwrap();
        response
    })
    .await
    .unwrap()
}

#[tokio::test]
#[ignore = "requires prebuilt fube-node and hosting-local-app; see root README"]
async fn mock_pods_drive_real_controller_fm_nodes_and_application_lifecycle() {
    lifecycle(false).await;
}

#[tokio::test]
#[ignore = "requires prebuilt fube-node and hosting-local-app; see root README"]
async fn deleting_during_node_startup_finishes_real_bootstrap_without_publishing_clients() {
    lifecycle(true).await;
}

async fn lifecycle(delete_during_bootstrap: bool) {
    let root = tempfile::tempdir().unwrap();
    let package = root.path().join("package");
    fs::create_dir_all(package.join("LocalService/Code")).unwrap();
    fs::write(
        package.join("ApplicationManifest.xml"),
        include_bytes!(
            "../../fube/crates/samples/hosting-local-app/pkg-stateful/ApplicationManifest.xml"
        ),
    )
    .unwrap();
    fs::write(package.join("LocalService/ServiceManifest.xml"),
        include_bytes!("../../fube/crates/samples/hosting-local-app/pkg-stateful/LocalService/ServiceManifest.xml")).unwrap();
    fs::copy(
        binary("hosting-local-app", "FUBE_NODE_APP"),
        package.join("LocalService/Code/hosting-local-app"),
    )
    .unwrap();
    let api = support::api::MockApi::default();
    let client = api.client(NAMESPACE);
    let fm = Arc::new(ProcessFm::new());
    let evidence = Arc::new(support::ProcessEvidence::new());
    let mut settings = support::settings();
    settings.fm.address = reserve(Ipv4Addr::LOCALHOST).to_string();
    settings.node_port = reserve(Ipv4Addr::new(127, 0, 0, 12)).port();
    let client_port = reserve(Ipv4Addr::new(127, 0, 0, 12)).port();
    let sets: Api<KubericSet> = Api::namespaced(client.clone(), NAMESPACE);
    let set = sets
        .create(
            &PostParams::default(),
            &KubericSet::new(NAME, support::spec("/opt/fube/package".into(), client_port)),
        )
        .await
        .unwrap();
    let controller =
        Reconciler::new(client.clone(), settings, fm.clone(), evidence.clone()).unwrap();
    let mut supervisor = Supervisor {
        client: client.clone(),
        executable: binary("fube-node", "FUBE_NODE_EXE"),
        package,
        nodes: BTreeMap::new(),
        configs: BTreeMap::new(),
        terminating: BTreeSet::new(),
        finished: BTreeSet::new(),
        evidence,
        api: api.clone(),
        root,
    };
    let deadline = Instant::now() + Duration::from_secs(30);
    let started = loop {
        let result = controller.reconcile(NAME).await.unwrap();
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
            "controller bootstrap deadline: {current:?}"
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
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut saw_bootstrap_wait = false;
    loop {
        let outcome = controller.reconcile(NAME).await.unwrap();
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
            "controller-owned teardown deadline"
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
