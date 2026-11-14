use std::{fs, path::Path, process::Command, time::Duration};

use anyhow::{ensure, Context, Result};
use fube_local_host::process::OwnedApp;
use fube_node_runner::Config;
use kuberic_controller::CleanupReceipt;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::{
    signal::unix::{signal, SignalKind},
    time::Instant,
};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Report {
    pub deployment: String,
    pub pod_uid: String,
    pub node_id: String,
    pub node_reaped: bool,
    pub node_exit_success: bool,
    pub application_reaped: bool,
    pub resources_joined: bool,
    pub forced: bool,
}

impl Report {
    pub fn receipt(&self) -> Result<CleanupReceipt> {
        let receipt = CleanupReceipt {
            node_reaped: self.node_reaped,
            node_exit_success: self.node_exit_success,
            application_reaped: self.application_reaped,
            resources_joined: self.resources_joined,
            forced: self.forced,
        };
        receipt.validate()?;
        Ok(receipt)
    }
}

pub async fn run(path: &Path) -> Result<()> {
    // Install the handler before acquiring the child; tini forwards kubelet SIGTERM here.
    let mut term = signal(SignalKind::terminate())?;
    // ConfigMap keys are symlinks; the runner deliberately accepts only regular files.
    let path = path
        .canonicalize()
        .context("resolving projected node configuration")?;
    let config = Config::read(&path).map_err(anyhow::Error::msg)?;
    let mut command = Command::new("/opt/fube/fube-node-real");
    command.arg("--config").arg(&path);
    let mut owner = None;
    OwnedApp::start(&mut owner, command)?;
    let child = owner.as_mut().context("missing original node handle")?;
    let mut stopping = false;
    let mut deadline = None;
    let exit = loop {
        if let Some(exit) = child.observed_exit()? {
            break exit;
        }
        ensure!(
            deadline.is_none_or(|deadline| Instant::now() < deadline),
            "node exceeded graceful termination budget"
        );
        tokio::select! {
            received = term.recv(), if !stopping => {
                received.context("SIGTERM stream closed")?;
                deadline = Some(Instant::now() + Duration::from_secs(23));
                ensure!(
                    Command::new("/bin/kill")
                        .args(["-TERM", &child.pid().to_string()])
                        .status()?.success(),
                    "forwarding SIGTERM to original node failed"
                );
                stopping = true;
            }
            () = tokio::time::sleep(Duration::from_millis(20)) => {}
        }
    };
    let cleanup = child.cleanup_with_budget(Duration::from_secs(2))?;
    for line in child.logs() {
        eprintln!("{line}");
    }
    ensure!(
        stopping,
        "node exited before controller-owned Pod deletion: {exit}"
    );
    let status: Value =
        serde_json::from_slice(&fs::read(config.run_directory.join("status.json"))?)?;
    let pid = status["application_pid"]
        .as_u64()
        .context("missing original application PID")?;
    let report = Report {
        deployment: config.deployment.clone(),
        pod_uid: config
            .run_directory
            .file_name()
            .and_then(|name| name.to_str())
            .context("missing Pod UID in original run directory")?
            .into(),
        node_id: config.node.identity.id.clone(),
        node_reaped: child.status().reaped && cleanup["reaped"] == true,
        node_exit_success: exit.success(),
        application_reaped: status["cleanup"]["application_cleaned"] == true
            && !Path::new("/proc").join(pid.to_string()).try_exists()?,
        resources_joined: child.status().group_drained
            && status["deployment"] == config.deployment
            && status["stage"] == "stopped"
            && status["cleanup"]["clean"] == true
            && status["cleanup"]["executor_closed"] == true
            && status["cleanup"]["store_closed"] == true
            && status["cleanup"]["channels_closed"] == true,
        forced: cleanup["forced"]
            .as_bool()
            .context("missing owned group cleanup result")?,
    };
    report.receipt().with_context(|| {
        format!("invalid physical cleanup report: {report:?}; runner={status}; owner={cleanup}")
    })?;
    let encoded = serde_json::to_vec(&report)?;
    ensure!(
        encoded.len() < 4096,
        "cleanup report exceeds termination-message limit"
    );
    fs::write("/dev/termination-log", encoded)?;
    Ok(())
}
