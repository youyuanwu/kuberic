use std::{fs, net::Ipv4Addr, path::PathBuf, time::Duration};

use crate::{
    fm as support,
    process::{binaries, reap, reserve, stage_gated_package, start, terminate},
};
use fube_local_host::process::OwnedApp;
use serde_json::Value;
use tokio::time::Instant;

pub struct Cluster {
    pub fm: OwnedApp,
    pub nodes: Vec<OwnedApp>,
    pub configs: Vec<fube_node_runner::Config>,
    controls: tempfile::TempDir,
    _root: tempfile::TempDir,
}

impl Cluster {
    pub fn start() -> Self {
        Self::start_with_timeout(Duration::from_secs(30))
    }

    pub fn start_with_timeout(timeout: Duration) -> Self {
        let binaries = binaries();
        let root = tempfile::tempdir().unwrap();
        let package = stage_gated_package(root.path(), &binaries.app);
        let controls = tempfile::Builder::new()
            .prefix("kuberic-qualification-")
            .tempdir_in("/tmp")
            .unwrap();

        let mut config = support::config();
        config.deployment = uuid::Uuid::new_v4().to_string();
        config.partition = uuid::Uuid::new_v4().to_string();
        config.fm.address = reserve(Ipv4Addr::LOCALHOST);
        config.operation_timeout_ms = timeout.as_millis().try_into().unwrap();
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
                        "--kuberic-test-control-root {} --stateful-lifecycle --data-replicator --replication-listen {}:0 --service-listen {}:0",
                        controls.path().display(), placement.address.ip(), placement.address.ip()
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
            controls,
            _root: root,
        }
    }

    pub fn node_status(&self, node: usize) -> Value {
        serde_json::from_slice(
            &fs::read(self.configs[node].run_directory.join("status.json")).unwrap(),
        )
        .unwrap()
    }

    pub fn work(&self, node: usize) -> PathBuf {
        let status = self.node_status(node);
        let root = PathBuf::from(status["runtime_root"].as_str().unwrap());
        assert!(root.starts_with(self.configs[node].run_directory.join("hosting")));
        root.join("deployment/HostingLocalStatefulApp_App1/work")
    }

    pub fn hold_next_copy(&self, node: usize) {
        fs::write(
            self.controls
                .path()
                .join(format!("copy-{}", self.configs[node].node.address.ip())),
            b"hold actual fresh-host copy",
        )
        .unwrap();
    }

    pub async fn until_node(
        &mut self,
        node: usize,
        description: &str,
        ready: impl Fn(&Value) -> bool,
    ) -> Value {
        let config = self.configs[node].clone();
        self.until_status(description, |_| {
            let status = serde_json::from_slice(
                &fs::read(config.run_directory.join("status.json")).unwrap(),
            )
            .unwrap();
            ready(&status)
        })
        .await;
        self.node_status(node)
    }

    pub async fn until_failed(&mut self) -> Value {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Some(status) = self.status() {
                if status["phase"] == "control_unknown" {
                    assert!(status["error"].is_string(), "{}", self.diagnostics());
                    return status;
                }
                assert!(status["error"].is_null(), "{}", self.diagnostics());
            }
            if let Some(exit) = self.fm.observed_exit().unwrap() {
                assert!(
                    !exit.success(),
                    "FM unexpectedly succeeded: {}",
                    self.diagnostics()
                );
                assert_eq!(self.fm.cleanup().unwrap()["forced"], false);
                let status = self.status().expect("exited FM omitted status");
                assert_eq!(status["phase"], "control_unknown", "{}", self.diagnostics());
                assert!(status["error"].is_string(), "{}", self.diagnostics());
                return status;
            }
            assert!(
                Instant::now() < deadline,
                "FM refusal deadline: {}",
                self.diagnostics()
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    pub async fn cleanup_failed(&mut self) {
        assert_eq!(self.status().unwrap()["phase"], "control_unknown");
        assert!(!reap(&mut self.fm).await.success());
        for node in &self.nodes {
            terminate(node);
        }
        for (index, node) in self.nodes.iter_mut().enumerate() {
            let exit = reap(node).await;
            let status: Value = serde_json::from_slice(
                &fs::read(self.configs[index].run_directory.join("status.json")).unwrap(),
            )
            .unwrap();
            assert_eq!(status["stage"], "stopped", "{status}");
            assert_eq!(
                exit.success(),
                status["cleanup"]["clean"].as_bool().unwrap(),
                "{status}"
            );
            for field in ["store_closed", "channels_closed"] {
                assert_eq!(status["cleanup"][field], true, "{status}");
            }
            if status["cleanup"]["clean"] == true {
                assert_eq!(status["cleanup"]["application_cleaned"], true, "{status}");
                assert_eq!(status["cleanup"]["executor_closed"], true, "{status}");
            } else {
                assert!(status["cleanup"]["error"].is_string(), "{status}");
                eprintln!(
                    "blocked native shutdown did not certify lifecycle completion: {}",
                    status["cleanup"]
                );
            }
            assert_eq!(status["control_unknown"], true, "{status}");
            assert!(status["cleanup"]["control_error"].is_string(), "{status}");
            eprintln!(
                "owned local shutdown with unacknowledged failed-session reports: node={index}, pending={}",
                status["cleanup"]["pending_work"]
            );
            assert!(!std::path::Path::new("/proc")
                .join(status["application_pid"].as_u64().unwrap().to_string())
                .exists());
            assert!(!PathBuf::from(status["runtime_root"].as_str().unwrap()).exists());
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
