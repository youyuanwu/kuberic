use std::{fs, net::Ipv4Addr, time::Duration};

use crate::{
    fm as support,
    process::{binaries, reserve, stage_package, start},
};
use fube_local_host::process::OwnedApp;
use serde_json::Value;
use tokio::time::Instant;

pub struct Cluster {
    pub fm: OwnedApp,
    pub nodes: Vec<OwnedApp>,
    pub configs: Vec<fube_node_runner::Config>,
    _root: tempfile::TempDir,
}

impl Cluster {
    pub fn start() -> Self {
        let binaries = binaries();
        let root = tempfile::tempdir().unwrap();
        let package = stage_package(root.path(), &binaries.app);

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
        let fm = start(&binaries.fm, &path);
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
            nodes.push(start(&binaries.node, &path));
        }
        Self {
            fm,
            nodes,
            configs,
            _root: root,
        }
    }

    pub fn status(&self) -> Option<Value> {
        self.fm
            .logs()
            .iter()
            .rev()
            .filter(|line| line.starts_with('{'))
            .map(|line| serde_json::from_str::<Value>(line).expect("valid FM JSON event"))
            .find(|event| event["event"] == "status")
            .map(|event| event["status"].clone())
    }

    pub fn diagnostics(&self) -> String {
        format!(
            "FM={:?}; nodes={:?}",
            self.fm.logs(),
            self.nodes.iter().map(OwnedApp::logs).collect::<Vec<_>>()
        )
    }

    pub async fn until(&mut self, phase: &str) -> Value {
        self.until_status(phase, |status| status["phase"] == phase)
            .await
    }

    pub async fn until_status(
        &mut self,
        description: &str,
        ready: impl Fn(&Value) -> bool,
    ) -> Value {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Some(status) = self.status() {
                assert!(status["error"].is_null(), "{}", self.diagnostics());
                if ready(&status) {
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
                "FM {description} deadline: {}",
                self.diagnostics()
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}
