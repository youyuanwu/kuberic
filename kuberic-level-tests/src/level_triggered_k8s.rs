use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result, bail};

fn cluster_args() -> Result<(String, String, String)> {
    let kubeconfig = std::env::var("KUBECONFIG").context("KUBECONFIG is required")?;
    let context = std::env::var("KUBE_CONTEXT").context("KUBE_CONTEXT is required")?;
    let cluster = std::env::var("KIND_CLUSTER_NAME").context("KIND_CLUSTER_NAME is required")?;
    if kubeconfig.is_empty()
        || context.is_empty()
        || cluster.is_empty()
        || kubeconfig == format!("{}/.kube/config", std::env::var("HOME").unwrap_or_default())
    {
        bail!("level-triggered KinD tests require an isolated explicit cluster");
    }
    Ok((kubeconfig, context, cluster))
}

fn kubectl(kubeconfig: &str, context: &str, args: &[&str]) -> Result<String> {
    let output = Command::new("kubectl")
        .args(["--kubeconfig", kubeconfig, "--context", context])
        .args(args)
        .output()
        .context("running kubectl")?;
    if !output.status.success() {
        bail!(
            "kubectl failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(String::from_utf8(output.stdout)?)
}

#[test]
#[ignore = "requires an explicitly owned isolated KinD cluster"]
fn bootstrap_reaches_three_member_topology_and_quorum_write() -> Result<()> {
    let (kubeconfig, context, _) = cluster_args()?;
    let deadline = std::time::Instant::now() + Duration::from_secs(300);
    loop {
        let status = kubectl(
            &kubeconfig,
            &context,
            &[
                "-n",
                "default",
                "get",
                "kubericset",
                "kvstore2",
                "-o",
                "json",
            ],
        )?;
        let value: serde_json::Value = serde_json::from_str(&status)?;
        let authority = &value["status"]["authority"];
        let members = authority["topology"]["configuration"]["members"]
            .as_array()
            .map_or(0, Vec::len);
        let ready = authority["conditions"]
            .as_array()
            .is_some_and(|conditions| {
                conditions
                    .iter()
                    .any(|condition| condition["type"] == "Ready" && condition["status"] == "true")
            });
        if authority["initialized"] == true && members == 3 && ready {
            break;
        }
        if std::time::Instant::now() >= deadline {
            bail!("bootstrap did not accept a three-member topology");
        }
        std::thread::sleep(Duration::from_secs(2));
    }

    kubectl(
        &kubeconfig,
        &context,
        &[
            "-n",
            "default",
            "delete",
            "service/kvstore2-peer",
            "secret/kvstore2-agent-credentials",
            "--ignore-not-found=true",
        ],
    )?;
    let support_deadline = std::time::Instant::now() + Duration::from_secs(120);
    loop {
        let peer = kubectl(
            &kubeconfig,
            &context,
            &["-n", "default", "get", "service", "kvstore2-peer"],
        );
        let credentials = kubectl(
            &kubeconfig,
            &context,
            &[
                "-n",
                "default",
                "get",
                "secret",
                "kvstore2-agent-credentials",
            ],
        );
        if peer.is_ok() && credentials.is_ok() {
            break;
        }
        if std::time::Instant::now() >= support_deadline {
            bail!("controller did not reconverge peer Service and credentials");
        }
        std::thread::sleep(Duration::from_secs(1));
    }

    kubectl(
        &kubeconfig,
        &context,
        &[
            "-n",
            "kuberic-system",
            "rollout",
            "restart",
            "deployment/kuberic-controller",
        ],
    )?;
    kubectl(
        &kubeconfig,
        &context,
        &[
            "-n",
            "kuberic-system",
            "rollout",
            "status",
            "deployment/kuberic-controller",
            "--timeout=180s",
        ],
    )?;

    let secondary = kubectl(
        &kubeconfig,
        &context,
        &[
            "-n",
            "default",
            "get",
            "pod",
            "-l",
            "operator.kuberic.io/replica-id=2",
            "-o",
            "jsonpath={.items[0].metadata.name}",
        ],
    )?;
    let before: serde_json::Value = serde_json::from_str(&kubectl(
        &kubeconfig,
        &context,
        &[
            "-n",
            "default",
            "exec",
            secondary.trim(),
            "--",
            "curl",
            "--fail",
            "--silent",
            "http://127.0.0.1:8080/status",
        ],
    )?)?;
    let node = kubectl(
        &kubeconfig,
        &context,
        &[
            "-n",
            "default",
            "get",
            "pod",
            secondary.trim(),
            "-o",
            "jsonpath={.spec.nodeName}",
        ],
    )?;
    let container = Command::new("docker")
        .args([
            "exec",
            node.trim(),
            "crictl",
            "ps",
            "--label",
            &format!("io.kubernetes.pod.name={}", secondary.trim()),
            "-q",
        ])
        .output()
        .context("resolving exact replica container")?;
    if !container.status.success() {
        bail!(
            "cannot resolve replica container: {}",
            String::from_utf8_lossy(&container.stderr)
        );
    }
    let container_id = String::from_utf8(container.stdout)?;
    let container_id = container_id.trim();
    if container_id.is_empty() || container_id.lines().count() != 1 {
        bail!("expected one exact replica container, observed {container_id:?}");
    }
    let stopped = Command::new("docker")
        .args(["exec", node.trim(), "crictl", "stop", container_id])
        .output()
        .context("stopping exact replica container")?;
    if !stopped.status.success() {
        bail!(
            "cannot stop replica container: {}",
            String::from_utf8_lossy(&stopped.stderr)
        );
    }
    let restart_deadline = std::time::Instant::now() + Duration::from_secs(180);
    loop {
        let status = kubectl(
            &kubeconfig,
            &context,
            &[
                "-n",
                "default",
                "exec",
                secondary.trim(),
                "--",
                "curl",
                "--fail",
                "--silent",
                "http://127.0.0.1:8080/status",
            ],
        );
        if let Ok(status) = status {
            let after: serde_json::Value = serde_json::from_str(&status)?;
            if after["processSession"] != before["processSession"]
                && after["currentConfiguration"] == before["currentConfiguration"]
            {
                break;
            }
        }
        if std::time::Instant::now() >= restart_deadline {
            bail!("secondary process did not reconstruct its accepted authority");
        }
        std::thread::sleep(Duration::from_secs(2));
    }

    let pod = kubectl(
        &kubeconfig,
        &context,
        &[
            "-n",
            "default",
            "get",
            "pod",
            "-l",
            "operator.kuberic.io/replica-id=1",
            "-o",
            "jsonpath={.items[0].metadata.name}",
        ],
    )?;
    let output = Command::new("kubectl")
        .args([
            "--kubeconfig",
            &kubeconfig,
            "--context",
            &context,
            "-n",
            "default",
            "exec",
            pod.trim(),
            "--",
            "sh",
            "-c",
            "curl --fail --silent --show-error -X PUT --data-binary phase6 http://127.0.0.1:8080/kv/bootstrap",
        ])
        .output()?;
    if !output.status.success() {
        bail!(
            "quorum write failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let value = kubectl(
        &kubeconfig,
        &context,
        &[
            "-n",
            "default",
            "exec",
            pod.trim(),
            "--",
            "curl",
            "--fail",
            "--silent",
            "--show-error",
            "http://127.0.0.1:8080/kv/bootstrap",
        ],
    )?;
    if value.trim() != "phase6" {
        bail!("unexpected read after quorum write: {value}");
    }
    Ok(())
}
