use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use serde_json::Value;

const NAMESPACE: &str = "kuberic-system";
const SET_NAME: &str = "kvstore2";

fn cluster_args() -> Result<(String, String, String)> {
    let kubeconfig = std::env::var("KUBECONFIG").context("KUBECONFIG is required")?;
    let context = std::env::var("KUBE_CONTEXT").context("KUBE_CONTEXT is required")?;
    let cluster = std::env::var("KIND_CLUSTER_NAME").context("KIND_CLUSTER_NAME is required")?;
    let receipt = std::fs::read_to_string(format!("{kubeconfig}.kuberic-owner"))
        .context("reading explicit cluster ownership receipt")?;
    validate_cluster_contract(&kubeconfig, &context, &cluster, &receipt)?;
    ensure!(
        kubectl(&kubeconfig, &context, &["config", "current-context"])?.trim() == context,
        "owned kubeconfig current context does not match KUBE_CONTEXT"
    );
    Ok((kubeconfig, context, cluster))
}

fn validate_cluster_contract(
    kubeconfig: &str,
    context: &str,
    cluster: &str,
    receipt: &str,
) -> Result<()> {
    if kubeconfig.is_empty()
        || context.is_empty()
        || cluster.is_empty()
        || cluster == "kind"
        || cluster.len() > 40
        || !cluster
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        || context != format!("kind-{cluster}")
        || kubeconfig.contains([':', '\n', '\r'])
        || kubeconfig == format!("{}/.kube/config", std::env::var("HOME").unwrap_or_default())
    {
        bail!("level-triggered KinD tests require an isolated explicit cluster");
    }
    ensure!(
        receipt == format!("cluster={cluster}\ncontext={context}\nkubeconfig={kubeconfig}\n"),
        "cluster ownership receipt does not match the explicit cluster/context/kubeconfig"
    );
    Ok(())
}

fn kubectl(kubeconfig: &str, context: &str, args: &[&str]) -> Result<String> {
    let output = Command::new("kubectl")
        .args([
            "--kubeconfig",
            kubeconfig,
            "--context",
            context,
            "--request-timeout=15s",
        ])
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

fn set_status(kubeconfig: &str, context: &str) -> Result<Value> {
    let status = kubectl(
        kubeconfig,
        context,
        &["-n", NAMESPACE, "get", "kubericset", SET_NAME, "-o", "json"],
    )?;
    Ok(serde_json::from_str(&status)?)
}

fn public_v1_authority_present(value: &Value) -> bool {
    value["status"]["publicV1Authority"].is_object()
}

fn public_v1_primary(kubeconfig: &str, context: &str) -> Result<Option<i64>> {
    let mut granted = Vec::new();
    for replica_id in 1..=3 {
        if replica_diagnostics(kubeconfig, context, replica_id)
            .is_ok_and(|status| status["writeStatus"] == "Granted")
        {
            granted.push(replica_id);
        }
    }
    ensure!(
        granted.len() <= 1,
        "Public V1 admitted more than one writer: {granted:?}"
    );
    Ok(granted.into_iter().next())
}

fn all_replicas_reporting(kubeconfig: &str, context: &str) -> bool {
    (1..=3).all(|replica_id| replica_diagnostics(kubeconfig, context, replica_id).is_ok())
}

fn pod_uid_for_replica(kubeconfig: &str, context: &str, replica_id: i64) -> Result<String> {
    let pod = pod_for_replica(kubeconfig, context, replica_id)?;
    let value: Value = serde_json::from_str(&kubectl(
        kubeconfig,
        context,
        &["-n", NAMESPACE, "get", "pod", &pod, "-o", "json"],
    )?)?;
    value["metadata"]["uid"]
        .as_str()
        .map(str::to_string)
        .context("replica Pod UID")
}

fn pod_for_replica(kubeconfig: &str, context: &str, replica_id: i64) -> Result<String> {
    let selector = format!(
        "operator.kuberic.io/set-name={SET_NAME},operator.kuberic.io/replica-id={replica_id}"
    );
    let pods: Value = serde_json::from_str(&kubectl(
        kubeconfig,
        context,
        &["-n", NAMESPACE, "get", "pod", "-l", &selector, "-o", "json"],
    )?)?;
    let items = pods["items"].as_array().context("replica Pod list")?;
    ensure!(
        items.len() == 1,
        "expected one exact {SET_NAME} replica {replica_id} Pod"
    );
    items[0]["metadata"]["name"]
        .as_str()
        .map(str::to_string)
        .context("replica Pod name")
}

fn stop_replica_process(kubeconfig: &str, context: &str, replica_id: i64) -> Result<()> {
    let pod = pod_for_replica(kubeconfig, context, replica_id)?;
    let node = kubectl(
        kubeconfig,
        context,
        &[
            "-n",
            NAMESPACE,
            "get",
            "pod",
            &pod,
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
            &format!("io.kubernetes.pod.name={pod}"),
            "-q",
        ])
        .output()
        .context("resolving exact replica container")?;
    if !container.status.success() {
        bail!(
            "cannot resolve replica {replica_id} container: {}",
            String::from_utf8_lossy(&container.stderr)
        );
    }
    let containers = String::from_utf8(container.stdout)?;
    let running: Vec<_> = containers
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    if running.is_empty() {
        return Ok(());
    }
    ensure!(
        running.len() == 1,
        "expected one exact replica container, observed {containers:?}"
    );
    let stopped = Command::new("docker")
        .args(["exec", node.trim(), "crictl", "stop", running[0].trim()])
        .output()
        .context("stopping exact replica container")?;
    if !stopped.status.success() {
        bail!(
            "cannot stop replica {replica_id} container: {}",
            String::from_utf8_lossy(&stopped.stderr)
        );
    }
    Ok(())
}

fn replica_diagnostics(kubeconfig: &str, context: &str, replica_id: i64) -> Result<Value> {
    let pod = pod_for_replica(kubeconfig, context, replica_id)?;
    let response = kubectl(
        kubeconfig,
        context,
        &[
            "-n",
            NAMESPACE,
            "exec",
            &pod,
            "--",
            "curl",
            "--fail",
            "--silent",
            "--max-time",
            "5",
            "http://127.0.0.1:8080/status",
        ],
    )?;
    Ok(serde_json::from_str(&response)?)
}

fn put_from_replica(
    kubeconfig: &str,
    context: &str,
    replica_id: i64,
    key: &str,
    value: &str,
) -> Result<u16> {
    let pod = pod_for_replica(kubeconfig, context, replica_id)?;
    let url = format!("http://127.0.0.1:8080/kv/{key}");
    let response = kubectl(
        kubeconfig,
        context,
        &[
            "-n",
            NAMESPACE,
            "exec",
            &pod,
            "--",
            "curl",
            "--silent",
            "--show-error",
            "--max-time",
            "10",
            "-X",
            "PUT",
            "--data-binary",
            value,
            "-o",
            "/dev/null",
            "-w",
            "%{http_code}",
            &url,
        ],
    )?;
    Ok(response.trim().parse()?)
}

fn get_from_replica(kubeconfig: &str, context: &str, replica_id: i64, key: &str) -> Result<String> {
    let pod = pod_for_replica(kubeconfig, context, replica_id)?;
    let url = format!("http://127.0.0.1:8080/kv/{key}");
    kubectl(
        kubeconfig,
        context,
        &[
            "-n",
            NAMESPACE,
            "exec",
            &pod,
            "--",
            "curl",
            "--fail",
            "--silent",
            "--show-error",
            "--max-time",
            "10",
            &url,
        ],
    )
}

#[test]
fn owned_cluster_contract_rejects_ambient_or_mismatched_contexts() {
    let receipt =
        "cluster=kuberic-test\ncontext=kind-kuberic-test\nkubeconfig=target/test.kubeconfig\n";
    assert!(
        validate_cluster_contract(
            "target/test.kubeconfig",
            "kind-kuberic-test",
            "kuberic-test",
            receipt
        )
        .is_ok()
    );
    for (config, context, cluster, receipt) in [
        (
            "target/test.kubeconfig",
            "kind-other",
            "kuberic-test",
            receipt,
        ),
        (
            "target/other.kubeconfig",
            "kind-kuberic-test",
            "kuberic-test",
            receipt,
        ),
        (
            "target/test.kubeconfig",
            "kind-kuberic-test",
            "kuberic-test",
            "",
        ),
        ("target/test.kubeconfig", "kind-kind", "kind", receipt),
        ("a:b", "kind-kuberic-test", "kuberic-test", receipt),
        ("", "", "", ""),
    ] {
        assert!(validate_cluster_contract(config, context, cluster, receipt).is_err());
    }
}

#[test]
#[ignore = "requires an explicitly owned isolated KinD cluster"]
fn bootstrap_reaches_three_member_topology_and_quorum_write() -> Result<()> {
    let (kubeconfig, context, _) = cluster_args()?;
    kubectl(
        &kubeconfig,
        &context,
        &[
            "-n",
            NAMESPACE,
            "delete",
            "kubericset",
            SET_NAME,
            "--ignore-not-found=true",
            "--wait=true",
        ],
    )?;
    kubectl(
        &kubeconfig,
        &context,
        &[
            "-n",
            NAMESPACE,
            "delete",
            "pods,pvc,services",
            &format!("--selector=operator.kuberic.io/set-name={SET_NAME}"),
            "--ignore-not-found=true",
        ],
    )?;
    kubectl(
        &kubeconfig,
        &context,
        &[
            "-n",
            NAMESPACE,
            "delete",
            "secret",
            "kvstore2-agent-credentials",
            "--ignore-not-found=true",
        ],
    )?;
    let sample = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .context("level-test crate has no repository parent")?
        .join("examples/kvstore2/deploy/sample.yaml");
    let sample = sample
        .to_str()
        .context("KVStore2 sample path is not UTF-8")?;
    kubectl(&kubeconfig, &context, &["apply", "-f", sample])?;

    let bootstrap_deadline = Instant::now() + Duration::from_secs(120);
    let primary = loop {
        let value = set_status(&kubeconfig, &context)?;
        if public_v1_authority_present(&value)
            && all_replicas_reporting(&kubeconfig, &context)
            && let Some(primary) = public_v1_primary(&kubeconfig, &context)?
        {
            break primary;
        }
        if Instant::now() >= bootstrap_deadline {
            bail!("bootstrap did not produce three Public V1 reports and exactly one writer");
        }
        std::thread::sleep(Duration::from_secs(2));
    };

    kubectl(
        &kubeconfig,
        &context,
        &[
            "-n",
            NAMESPACE,
            "delete",
            "service/kvstore2-peer",
            "secret/kvstore2-agent-credentials",
            "--ignore-not-found=true",
        ],
    )?;
    let support_deadline = Instant::now() + Duration::from_secs(120);
    loop {
        let peer = kubectl(
            &kubeconfig,
            &context,
            &["-n", NAMESPACE, "get", "service", "kvstore2-peer"],
        );
        let credentials = kubectl(
            &kubeconfig,
            &context,
            &[
                "-n",
                NAMESPACE,
                "get",
                "secret",
                "kvstore2-agent-credentials",
            ],
        );
        if peer.is_ok() && credentials.is_ok() {
            break;
        }
        if Instant::now() >= support_deadline {
            bail!("controller did not reconverge peer Service and credentials");
        }
        std::thread::sleep(Duration::from_secs(1));
    }

    kubectl(
        &kubeconfig,
        &context,
        &[
            "-n",
            NAMESPACE,
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
            NAMESPACE,
            "rollout",
            "status",
            "deployment/kuberic-controller",
            "--timeout=180s",
        ],
    )?;

    let secondary = 2;
    let before = replica_diagnostics(&kubeconfig, &context, secondary)?;
    stop_replica_process(&kubeconfig, &context, secondary)?;
    let restart_deadline = Instant::now() + Duration::from_secs(90);
    loop {
        if replica_diagnostics(&kubeconfig, &context, secondary).is_ok_and(|after| {
            after["processSession"] != before["processSession"]
                && after["currentConfiguration"] == before["currentConfiguration"]
                && after["readStatus"] == "Granted"
        }) {
            break;
        }
        if Instant::now() >= restart_deadline {
            bail!("secondary process did not reconstruct its accepted authority");
        }
        std::thread::sleep(Duration::from_secs(2));
    }

    let access_deadline = Instant::now() + Duration::from_secs(120);
    loop {
        if replica_diagnostics(&kubeconfig, &context, primary)
            .is_ok_and(|status| status["writeStatus"] == "Granted")
        {
            break;
        }
        if Instant::now() >= access_deadline {
            bail!("primary did not restore write access after bootstrap restart");
        }
        std::thread::sleep(Duration::from_secs(2));
    }
    ensure!(
        put_from_replica(&kubeconfig, &context, primary, "bootstrap", "phase6")? == 200,
        "quorum write failed"
    );
    let value = get_from_replica(&kubeconfig, &context, primary, "bootstrap")?;
    ensure!(
        value.trim() == "phase6",
        "unexpected read after quorum write: {value}"
    );
    Ok(())
}

#[test]
#[ignore = "requires an explicitly owned isolated KinD cluster"]
fn replacement_preserves_quorum_write_and_retires_old_incarnation() -> Result<()> {
    let (kubeconfig, context, _) = cluster_args()?;
    let ready_deadline = Instant::now() + Duration::from_secs(120);
    let (old_instance, primary) = loop {
        let status = set_status(&kubeconfig, &context)?;
        if public_v1_authority_present(&status)
            && all_replicas_reporting(&kubeconfig, &context)
            && let Some(primary) = public_v1_primary(&kubeconfig, &context)?
        {
            break (pod_uid_for_replica(&kubeconfig, &context, 2)?, primary);
        }
        if Instant::now() >= ready_deadline {
            bail!("bootstrap did not become ready before replacement");
        }
        std::thread::sleep(Duration::from_secs(2));
    };

    ensure!(
        put_from_replica(
            &kubeconfig,
            &context,
            primary,
            "replacement",
            "before-replacement",
        )? == 200,
        "write before replacement failed"
    );

    let secondary = pod_for_replica(&kubeconfig, &context, 2)?;
    kubectl(
        &kubeconfig,
        &context,
        &[
            "-n",
            NAMESPACE,
            "delete",
            "pod",
            &secondary,
            "--wait=true",
            "--timeout=120s",
        ],
    )?;

    let replacement_deadline = Instant::now() + Duration::from_secs(120);
    loop {
        if pod_uid_for_replica(&kubeconfig, &context, 2)
            .is_ok_and(|instance| instance != old_instance)
            && replica_diagnostics(&kubeconfig, &context, 2).is_ok()
            && public_v1_primary(&kubeconfig, &context)?.is_some()
        {
            break;
        }
        if Instant::now() >= replacement_deadline {
            bail!("same-cardinality replacement did not converge");
        }
        std::thread::sleep(Duration::from_secs(2));
    }

    let primary = public_v1_primary(&kubeconfig, &context)?
        .context("replacement omitted a Public V1 writer")?;
    ensure!(
        put_from_replica(
            &kubeconfig,
            &context,
            primary,
            "replacement",
            "after-replacement",
        )? == 200,
        "write after replacement failed"
    );

    let before_restart = replica_diagnostics(&kubeconfig, &context, 2)?;
    stop_replica_process(&kubeconfig, &context, 2)?;
    let restart_deadline = Instant::now() + Duration::from_secs(90);
    loop {
        if replica_diagnostics(&kubeconfig, &context, 2).is_ok_and(|restarted| {
            restarted["processSession"] != before_restart["processSession"]
                && restarted["currentConfiguration"] == before_restart["currentConfiguration"]
        }) {
            break;
        }
        if Instant::now() >= restart_deadline {
            bail!("replacement process did not reconstruct accepted authority");
        }
        std::thread::sleep(Duration::from_secs(2));
    }

    ensure!(
        put_from_replica(
            &kubeconfig,
            &context,
            primary,
            "replacement-restart",
            "after-restart",
        )? == 200,
        "write after replacement restart failed"
    );
    Ok(())
}

#[test]
#[ignore = "requires an explicitly owned isolated KinD cluster"]
fn failover_fences_old_primary_and_preserves_committed_data() -> Result<()> {
    let (kubeconfig, context, _) = cluster_args()?;
    let ready_deadline = Instant::now() + Duration::from_secs(120);
    let old_primary = loop {
        let status = set_status(&kubeconfig, &context)?;
        if public_v1_authority_present(&status)
            && all_replicas_reporting(&kubeconfig, &context)
            && let Some(primary) = public_v1_primary(&kubeconfig, &context)?
        {
            break primary;
        }
        if Instant::now() >= ready_deadline {
            bail!("bootstrap did not become ready before failover");
        }
        std::thread::sleep(Duration::from_secs(2));
    };

    let write_ready_deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if replica_diagnostics(&kubeconfig, &context, old_primary).is_ok_and(|diagnostics| {
            diagnostics["readStatus"] == "Granted" && diagnostics["writeStatus"] == "Granted"
        }) {
            break;
        }
        if Instant::now() >= write_ready_deadline {
            bail!("pre-failover primary did not expose granted read/write readiness");
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    ensure!(
        put_from_replica(
            &kubeconfig,
            &context,
            old_primary,
            "failover",
            "before-failover",
        )? == 200,
        "pre-failover quorum write failed"
    );

    let failover_deadline = Instant::now() + Duration::from_secs(120);
    stop_replica_process(&kubeconfig, &context, old_primary)?;
    let new_primary = loop {
        if let Some(primary) = public_v1_primary(&kubeconfig, &context)?
            && primary != old_primary
        {
            break primary;
        }
        if Instant::now() >= failover_deadline {
            bail!("ordinary failover did not select and publish a newer primary");
        }
        std::thread::sleep(Duration::from_millis(500));
    };

    let value = get_from_replica(&kubeconfig, &context, new_primary, "failover")?;
    ensure!(
        value.trim() == "before-failover",
        "committed value was not preserved across failover: {value}"
    );
    ensure!(
        put_from_replica(
            &kubeconfig,
            &context,
            new_primary,
            "failover",
            "after-failover",
        )? == 200,
        "post-failover quorum write failed"
    );
    Ok(())
}
