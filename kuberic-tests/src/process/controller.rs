use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    sync::Arc,
};

use crate::{
    controller as support,
    process::{binaries, stage_package, start, terminate},
};
use anyhow::{Context, Result};
use fube_local_host::process::OwnedApp;
use k8s_openapi::api::core::v1::{ConfigMap, Pod};
use kube::{api::ListParams, Api, Client, ResourceExt};
use kuberic_controller::{resources, CleanupReceipt, CleanupState, KubericSet, TeardownStage};
use serde_json::Value;
use support::{NAME, NAMESPACE};

pub struct Supervisor {
    client: Client,
    executable: PathBuf,
    package: PathBuf,
    pub nodes: BTreeMap<String, OwnedApp>,
    pub configs: BTreeMap<String, fube_node_runner::Config>,
    terminating: BTreeSet<String>,
    pub finished: BTreeSet<String>,
    evidence: Arc<support::ProcessEvidence>,
    api: support::api::MockApi,
    root: tempfile::TempDir,
}

impl Supervisor {
    pub fn new(
        client: Client,
        api: support::api::MockApi,
        evidence: Arc<support::ProcessEvidence>,
    ) -> Self {
        let binaries = binaries();
        let root = tempfile::tempdir().unwrap();
        let package = stage_package(root.path(), &binaries.app);
        Self {
            client,
            executable: binaries.node.clone(),
            package,
            nodes: BTreeMap::new(),
            configs: BTreeMap::new(),
            terminating: BTreeSet::new(),
            finished: BTreeSet::new(),
            evidence,
            api,
            root,
        }
    }

    pub fn diagnostics(&self) -> String {
        format!(
            "{:?}",
            self.nodes
                .iter()
                .map(|(uid, node)| (uid, node.logs()))
                .collect::<Vec<_>>()
        )
    }

    pub async fn tick(&mut self, resource_uid: &str) {
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
                    terminate(child);
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
            self.nodes
                .insert(uid.clone(), start(&self.executable, &path));
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
