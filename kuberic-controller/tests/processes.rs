mod support;

use std::{
    collections::BTreeMap,
    fs,
    net::{Ipv4Addr, SocketAddrV4},
    path::{Path, PathBuf},
    process::Command,
    sync::Arc,
    time::Duration,
};

use fube_local_host::process::OwnedApp;
use k8s_openapi::api::core::v1::{ConfigMap, Pod, Service};
use kube::{
    api::{ListParams, PostParams},
    Api, Client, ResourceExt,
};
use kuberic_controller::{resources, ControllerPhase, FmApi, KubericSet, ProcessFm, Reconciler};
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
    nodes: BTreeMap<String, OwnedApp>,
    configs: Vec<fube_node_runner::Config>,
    root: tempfile::TempDir,
}

impl Supervisor {
    async fn tick(&mut self, resource_uid: &str) {
        let pods: Api<Pod> = Api::namespaced(self.client.clone(), NAMESPACE);
        let maps: Api<ConfigMap> = Api::namespaced(self.client.clone(), NAMESPACE);
        for pod in pods
            .list(&ListParams::default().labels(&format!("{}={resource_uid}", resources::SET_UID)))
            .await
            .unwrap()
        {
            let uid = pod.uid().unwrap();
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
            let config: fube_node_runner::Config = serde_json::from_str(json).unwrap();
            config.validate().unwrap();
            assert_eq!(config.run_directory.file_name().unwrap(), uid.as_str());
            fs::create_dir_all(config.run_directory.parent().unwrap()).unwrap();
            let path = self.root.path().join(format!("{uid}.json"));
            fs::write(&path, json).unwrap();
            let mut command = Command::new(&self.executable);
            command.arg("--config").arg(path);
            let mut child = None;
            OwnedApp::start(&mut child, command).unwrap();
            self.nodes.insert(uid, child.unwrap());
            self.configs.push(config);
        }
        for node in self.nodes.values_mut() {
            node.check_alive()
                .unwrap_or_else(|error| panic!("{error}: {:?}", node.logs()));
        }
    }
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

async fn finish(child: &mut OwnedApp) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(status) = child.observed_exit().unwrap() {
            assert!(status.success(), "node failed: {:?}", child.logs());
            assert_eq!(child.cleanup().unwrap()["forced"], false);
            return;
        }
        assert!(
            Instant::now() < deadline,
            "node join timed out: {:?}",
            child.logs()
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
#[ignore = "requires prebuilt fube-node and hosting-local-app; see root README"]
async fn mock_pods_drive_real_controller_fm_nodes_and_application_lifecycle() {
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
    let mut settings = support::settings();
    settings.fm.address = reserve(Ipv4Addr::LOCALHOST).to_string();
    settings.node_port = reserve(Ipv4Addr::new(127, 0, 0, 12)).port();
    settings.run_root = root.path().join("run").to_str().unwrap().into();
    let client_port = reserve(Ipv4Addr::new(127, 0, 0, 12)).port();
    let sets: Api<KubericSet> = Api::namespaced(client.clone(), NAMESPACE);
    let set = sets
        .create(
            &PostParams::default(),
            &KubericSet::new(
                NAME,
                support::spec(package.to_str().unwrap().into(), client_port),
            ),
        )
        .await
        .unwrap();
    let controller = Reconciler::new(client.clone(), settings, fm.clone()).unwrap();
    let mut supervisor = Supervisor {
        client: client.clone(),
        executable: binary("fube-node", "FUBE_NODE_EXE"),
        nodes: BTreeMap::new(),
        configs: Vec::new(),
        root,
    };
    let deadline = Instant::now() + Duration::from_secs(30);
    let serving = loop {
        let result = controller.reconcile(NAME).await.unwrap();
        assert!(
            result.blocked.is_none(),
            "{result:?}; FM={:?}",
            fm.observe().await.unwrap()
        );
        supervisor.tick(&set.uid().unwrap()).await;
        let current = sets.get(NAME).await.unwrap();
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
    let cohort = serving.status.as_ref().unwrap().cohort().unwrap();
    let endpoint = serving
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
    let services: Api<Service> = Api::namespaced(client.clone(), NAMESPACE);
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

    fm.shutdown(&cohort).await.unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let outcome = controller.reconcile(NAME).await.unwrap();
        assert!(outcome.blocked.is_none(), "{outcome:?}");
        if fm.observe().await.unwrap().unwrap().status.phase
            == kuberic_fm::Phase::AwaitingNodeShutdown
        {
            controller.reconcile(NAME).await.unwrap();
            break;
        }
        assert!(Instant::now() < deadline, "native deletion deadline");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(
        services
            .get(&resources::write_service_name(NAME))
            .await
            .unwrap()
            .spec
            .unwrap()
            .selector,
        Some(resources::disabled_selector(&sets.get(NAME).await.unwrap()).unwrap())
    );
    for node in supervisor.nodes.values() {
        assert!(Command::new("kill")
            .args(["-TERM", &node.pid().to_string()])
            .status()
            .unwrap()
            .success());
    }
    for node in supervisor.nodes.values_mut() {
        finish(node).await;
    }
    fm.join().await.unwrap();
    assert_eq!(
        fm.observe().await.unwrap().unwrap().status.phase,
        kuberic_fm::Phase::Stopped
    );
    for config in &supervisor.configs {
        let status: Value =
            serde_json::from_slice(&fs::read(config.run_directory.join("status.json")).unwrap())
                .unwrap();
        assert_eq!(status["cleanup"]["clean"], true, "{status}");
        let pid = status["application_pid"].as_u64().unwrap();
        assert!(!Path::new("/proc").join(pid.to_string()).exists());
    }
    api.check();
}
