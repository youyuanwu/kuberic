mod support;

use std::{
    fs,
    net::{Ipv4Addr, SocketAddrV4},
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

use fube_local_host::process::OwnedApp;
use serde_json::Value;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::TcpStream,
    time::Instant,
};

fn reserve(ip: Ipv4Addr) -> SocketAddrV4 {
    let listener = std::net::TcpListener::bind((ip, 0)).unwrap();
    match listener.local_addr().unwrap() {
        std::net::SocketAddr::V4(address) => address,
        _ => unreachable!(),
    }
}

fn binary(name: &str, variable: &str) -> PathBuf {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    std::env::var_os(variable)
        .map(PathBuf::from)
        .unwrap_or_else(|| root.join("target/debug").join(name))
        .canonicalize()
        .unwrap_or_else(|error| {
            panic!("missing prebuilt {name}: {error}; see README process-test commands")
        })
}

fn start(executable: &Path, config: &Path) -> OwnedApp {
    let mut command = Command::new(executable);
    command.arg("--config").arg(config);
    let mut owner = None;
    OwnedApp::start(&mut owner, command).unwrap();
    owner.unwrap()
}

fn terminate(child: &OwnedApp) {
    assert!(Command::new("kill")
        .args(["-TERM", &child.pid().to_string()])
        .status()
        .unwrap()
        .success());
}

struct Cluster {
    fm: OwnedApp,
    nodes: Vec<OwnedApp>,
    configs: Vec<fube_node_runner::Config>,
    _root: tempfile::TempDir,
}

impl Cluster {
    fn new() -> Self {
        let node_executable = binary("fube-node", "FUBE_NODE_EXE");
        let app_executable = binary("hosting-local-app", "FUBE_NODE_APP");
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
        fs::write(
            package.join("LocalService/ServiceManifest.xml"),
            include_bytes!("../../fube/crates/samples/hosting-local-app/pkg-stateful/LocalService/ServiceManifest.xml"),
        ).unwrap();
        fs::copy(
            app_executable,
            package.join("LocalService/Code/hosting-local-app"),
        )
        .unwrap();

        let mut config = support::config();
        config.deployment = uuid::Uuid::new_v4().to_string();
        config.partition = uuid::Uuid::new_v4().to_string();
        config.fm.address = reserve(Ipv4Addr::LOCALHOST);
        config.operation_timeout_ms = 30_000;
        config.shutdown_timeout_ms = 30_000;
        config.retry_interval_ms = 1_000;
        for node in &mut config.nodes {
            node.address = reserve(*node.address.ip());
        }
        config.validate().unwrap();
        let path = root.path().join("fm.json");
        fs::write(&path, serde_json::to_vec_pretty(&config).unwrap()).unwrap();
        let fm = start(Path::new(env!("CARGO_BIN_EXE_kuberic-fm")), &path);
        let configs: Vec<_> = config.nodes.iter().enumerate().map(|(index, placement)| {
            fube_node_runner::Config {
                schema_version: 1,
                deployment: config.deployment.clone(),
                name: format!("node{}", index + 1),
                node: placement.clone(),
                fm: config.fm.clone(),
                peers: config.nodes.iter().enumerate()
                    .filter(|(peer, _)| *peer != index)
                    .map(|(_, peer)| peer.clone())
                    .collect(),
                partition: config.partition.clone(),
                package: package.clone(),
                application: config.service.application.clone(),
                package_version_instance: config.service.package_version_instance,
                parameters: vec![
                    ("TargetReplicaSetSize".into(), "3".into()),
                    ("MinReplicaSetSize".into(), "2".into()),
                    ("Arguments".into(), format!(
                        "--stateful-lifecycle --data-replicator --replication-listen {}:0 --service-listen {}:0",
                        placement.address.ip(), placement.address.ip()
                    )),
                ],
                run_directory: root.path().join(format!("node{}", index + 1)),
                startup_timeout_ms: 30_000,
                shutdown_timeout_ms: 20_000,
            }
        }).collect();
        let mut nodes = Vec::new();
        for (index, node) in configs.iter().enumerate() {
            node.validate().unwrap();
            let path = root.path().join(format!("node{index}.json"));
            fs::write(&path, serde_json::to_vec(node).unwrap()).unwrap();
            nodes.push(start(&node_executable, &path));
        }
        Self {
            fm,
            nodes,
            configs,
            _root: root,
        }
    }

    fn status(&self) -> Option<Value> {
        self.fm
            .logs()
            .iter()
            .rev()
            .filter(|line| line.starts_with('{'))
            .map(|line| serde_json::from_str::<Value>(line).expect("valid FM JSON event"))
            .find(|event| event["event"] == "status")
            .map(|event| event["status"].clone())
    }

    fn diagnostics(&self) -> String {
        format!(
            "FM={:?}; nodes={:?}",
            self.fm.logs(),
            self.nodes.iter().map(OwnedApp::logs).collect::<Vec<_>>()
        )
    }

    async fn until(&mut self, phase: &str) -> Value {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Some(status) = self.status() {
                assert!(status["error"].is_null(), "{}", self.diagnostics());
                if status["phase"] == phase {
                    return status;
                }
            }
            self.fm
                .check_alive()
                .unwrap_or_else(|error| panic!("{error}: {}", self.diagnostics()));
            for node in &mut self.nodes {
                node.check_alive()
                    .unwrap_or_else(|error| panic!("{error}: {:?}", node.logs()));
            }
            assert!(
                Instant::now() < deadline,
                "FM {phase} deadline: {}",
                self.diagnostics()
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
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
            assert!(status.success(), "process failed: {:?}", child.logs());
            assert_eq!(child.cleanup().unwrap()["forced"], false);
            return;
        }
        assert!(
            Instant::now() < deadline,
            "process exit deadline: {:?}",
            child.logs()
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
#[ignore = "requires prebuilt fube-node and hosting-local-app; see root README"]
async fn real_fm_and_fube_nodes_create_serve_delete_and_join_owned_processes() {
    let mut cluster = Cluster::new();
    let status = cluster.until("serving").await;
    let endpoint = status["service_endpoint"].as_str().unwrap();
    assert!(application(endpoint, "write").await.starts_with("OK"));
    assert!(application(endpoint, "read")
        .await
        .contains("value=acknowledged"));

    terminate(&cluster.fm);
    let status = cluster.until("awaiting_node_shutdown").await;
    assert!(status["service_endpoint"].is_null());
    for node in &cluster.nodes {
        terminate(node);
    }
    for node in &mut cluster.nodes {
        finish(node).await;
    }
    finish(&mut cluster.fm).await;
    assert_eq!(cluster.status().unwrap()["phase"], "stopped");
    for config in &cluster.configs {
        let status: Value =
            serde_json::from_slice(&fs::read(config.run_directory.join("status.json")).unwrap())
                .unwrap();
        assert_eq!(status["stage"], "stopped");
        assert_eq!(status["cleanup"]["clean"], true, "{status}");
        let pid = status["application_pid"].as_u64().unwrap();
        assert!(!Path::new("/proc").join(pid.to_string()).exists());
    }
}
