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

fn public_v1_topology_idle(value: &Value) -> bool {
    let status = &value["status"];
    status["publicV1Topology"].is_object()
        && !status["publicV1Operation"].is_object()
        && !status["publicV1Transition"].is_object()
        && !status["publicV1Replacement"].is_object()
        && !status["publicV1SessionRenewal"].is_object()
        && status["publicV1Topology"]["unavailablePrimaryId"].is_null()
}

fn topology_session_matches(value: &Value, replica_id: i64, process_session: &Value) -> bool {
    let Some(process_session) = process_session.as_str() else {
        return false;
    };
    value["status"]["publicV1Topology"]["sessions"]
        .as_array()
        .is_some_and(|sessions| {
            sessions.iter().any(|session| {
                session["identity"]["replicaId"] == replica_id
                    && session["processSessionId"] == process_session
            })
        })
}

fn secondary_reconstructed_authority(
    value: &Value,
    before: &Value,
    after: &Value,
    replica_id: i64,
) -> Result<bool> {
    use kuberic_controller::crd::{PublicV1AuthorityStatus, PublicV1TopologyState};
    use kuberic_runtime::protocol::public_operations::{PublicV1AcceptedResult, PublicV1CallKind};
    use kuberic_runtime::protocol::types::ReplicaRole;

    let before_session = before["processSession"]
        .as_str()
        .filter(|session| !session.is_empty())
        .context("pre-restart process session is missing")?;
    let Some(after_session) = after["processSession"]
        .as_str()
        .filter(|session| !session.is_empty() && *session != before_session)
    else {
        return Ok(false);
    };
    if after["replicaId"] != replica_id
        || after["role"] != "ActiveSecondary"
        || after["readStatus"] != "NotPrimary"
        || after["writeStatus"] != "NotPrimary"
        || after["retired"] != false
        || !public_v1_topology_idle(value)
        || !public_v1_authority_present(value)
    {
        return Ok(false);
    }
    for field in ["instanceId", "agentGeneration"] {
        let expected = before[field]
            .as_str()
            .filter(|identity| !identity.is_empty())
            .with_context(|| format!("pre-restart {field} is missing"))?;
        if after[field].as_str() != Some(expected) {
            return Ok(false);
        }
    }
    let topology: PublicV1TopologyState =
        serde_json::from_value(value["status"]["publicV1Topology"].clone())?;
    let configuration = topology.configuration().map_err(anyhow::Error::msg)?;
    let Some(endpoint) = topology.sessions.iter().find(|endpoint| {
        endpoint.identity.replica_id.value() == replica_id
            && Some(endpoint.identity.instance_id.as_str()) == after["instanceId"].as_str()
            && Some(endpoint.identity.agent_generation.as_str())
                == after["agentGeneration"].as_str()
            && endpoint.process_session_id.as_str() == after_session
            && endpoint.role == ReplicaRole::ActiveSecondary
    }) else {
        return Ok(false);
    };
    let authority: PublicV1AuthorityStatus =
        serde_json::from_value(value["status"]["publicV1Authority"].clone())?;
    let authority = authority.authority().map_err(anyhow::Error::msg)?;
    Ok(
        authority.history.data_loss_number == configuration.epoch.data_loss_number
            && authority.completed_operations.iter().any(|completion| {
                completion.process_session_id == endpoint.process_session_id
                    && completion.storage_incarnation == endpoint.storage_incarnation
                    && completion.call_kind == PublicV1CallKind::Configuration
                    && completion.result == PublicV1AcceptedResult::Succeeded
                    && completion.previous_configuration_id.is_none()
                    && completion.current_configuration_id.as_ref()
                        == Some(&configuration.configuration_id)
            }),
    )
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
    let (node, container) = replica_container(kubeconfig, context, replica_id)?;
    let stopped = Command::new("docker")
        .args(["exec", &node, "crictl", "stop", &container])
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

fn set_replica_process_paused(
    kubeconfig: &str,
    context: &str,
    replica_id: i64,
    paused: bool,
) -> Result<()> {
    let (node, container) = replica_container(kubeconfig, context, replica_id)?;
    let action = if paused { "pause" } else { "resume" };
    let updated = Command::new("docker")
        .args([
            "exec",
            &node,
            "ctr",
            "--namespace",
            "k8s.io",
            "tasks",
            action,
            &container,
        ])
        .output()
        .context("updating exact replica process pause state")?;
    if !updated.status.success() {
        bail!(
            "cannot {action} replica {replica_id} process: {}",
            String::from_utf8_lossy(&updated.stderr)
        );
    }
    Ok(())
}

fn replica_container(kubeconfig: &str, context: &str, replica_id: i64) -> Result<(String, String)> {
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
        bail!("replica {replica_id} has no running container");
    }
    ensure!(
        running.len() == 1,
        "expected one exact replica container, observed {containers:?}"
    );
    Ok((node.trim().to_string(), running[0].trim().to_string()))
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
fn secondary_restart_requires_current_accepted_configuration_not_legacy_diagnostics() -> Result<()>
{
    use kuberic_controller::crd::{PublicV1AuthorityStatus, PublicV1TopologyState};
    use kuberic_runtime::protocol::public_operations::*;
    use kuberic_runtime::protocol::types::*;

    let epoch = Epoch::new(0, 2);
    let members = (1..=2)
        .map(|id| ConfigurationMember {
            identity: ReplicaIdentity {
                replica_id: ReplicaId::new(id),
                instance_id: ReplicaInstanceId::new(format!("pod-{id}")),
                agent_generation: AgentGeneration::new(format!("agent-{id}")),
            },
            role: if id == 1 {
                ReplicaRole::Primary
            } else {
                ReplicaRole::ActiveSecondary
            },
        })
        .collect::<Vec<_>>();
    let configuration = ConfigurationDescriptor::new(epoch, ReplicaId::new(1), members.clone(), 2);
    let sessions = members
        .iter()
        .map(|member| ProcessSessionEndpoint {
            identity: member.identity.clone(),
            storage_incarnation: ReplicaStorageIncarnationContext {
                replica: member.identity.clone(),
                storage: ReplicaStorageIdentity::Persistent {
                    pod_uid: PodUid::new(member.identity.instance_id.as_str()),
                    pvc_uid: PvcUid::new(format!("pvc-{}", member.identity.replica_id)),
                },
                storage_generation: StorageGeneration::new("storage"),
            },
            process_session_id: ProcessSessionId::new("current"),
            replicator_creation_id: ReplicatorCreationId::new("creation"),
            role: member.role,
        })
        .collect::<Vec<_>>();
    let topology = PublicV1TopologyState::from_configuration(&configuration, &sessions)
        .map_err(anyhow::Error::msg)?;
    let history = HistoryContext {
        resource_uid: ResourceUid::new("resource"),
        protocol_generation: PublicProtocolGeneration::new(1),
        history_id: HistoryId::new("history"),
        data_loss_number: 0,
    };
    let mut accepted = PublicV1AcceptedStatus {
        identity: PublicV1Identity::new(1),
        history_initialization_id: HistoryInitializationId::new("initialization"),
        history: history.clone(),
        data_loss_recovery: None,
        completed_operations: vec![PublicV1AcceptedOperationCompletion {
            operation_id: OperationId::new("configuration"),
            revision: 1,
            call_kind: PublicV1CallKind::Configuration,
            process_session_id: sessions[1].process_session_id.clone(),
            input_digest_version: 1,
            input_digest_sha256: "a".repeat(64),
            replay_disposition: PublicV1AcceptedReplayDisposition::Convergent,
            history,
            storage_incarnation: sessions[1].storage_incarnation.clone(),
            previous_configuration_id: None,
            current_configuration_id: Some(configuration.configuration_id.clone()),
            catch_up_mode: None,
            catch_up_specific_quorum: None,
            result: PublicV1AcceptedResult::Succeeded,
            failure_detail: None,
            rebuild_required: None,
        }],
        extensions: PublicV1Extensions::default(),
    };
    let authority =
        PublicV1AuthorityStatus::from_authority(&accepted).map_err(anyhow::Error::msg)?;
    let mut status = serde_json::json!({"status": {
        "publicV1Authority": authority,
        "publicV1Topology": topology,
    }});
    let before = serde_json::json!({
        "replicaId": 2, "instanceId": "pod-2", "agentGeneration": "agent-2",
        "processSession": "previous", "currentConfiguration": null,
    });
    let after = serde_json::json!({
        "replicaId": 2, "instanceId": "pod-2", "agentGeneration": "agent-2",
        "processSession": "current", "role": "ActiveSecondary", "epoch": "0.1",
        "readStatus": "NotPrimary", "writeStatus": "NotPrimary", "retired": false,
        "currentConfiguration": null,
    });
    assert!(secondary_reconstructed_authority(
        &status, &before, &after, 2
    )?);
    for (field, value) in [
        ("processSession", serde_json::json!("previous")),
        ("instanceId", serde_json::json!("other-pod")),
        ("agentGeneration", serde_json::json!("other-agent")),
        ("role", serde_json::json!("Primary")),
        ("readStatus", serde_json::json!("Granted")),
        ("writeStatus", serde_json::json!("Granted")),
        ("retired", serde_json::json!(true)),
    ] {
        let mut pending = after.clone();
        pending[field] = value;
        assert!(
            !secondary_reconstructed_authority(&status, &before, &pending, 2)?,
            "{field}"
        );
    }
    let mut pending = status.clone();
    pending["status"]["publicV1SessionRenewal"] = serde_json::json!({});
    assert!(!secondary_reconstructed_authority(
        &pending, &before, &after, 2
    )?);
    pending = status.clone();
    pending["status"]["publicV1Topology"]["sessions"][1]["processSessionId"] =
        serde_json::json!("previous");
    assert!(!secondary_reconstructed_authority(
        &pending, &before, &after, 2
    )?);
    for completion in [
        {
            let mut completion = accepted.completed_operations[0].clone();
            completion.process_session_id = ProcessSessionId::new("previous");
            completion
        },
        {
            let mut completion = accepted.completed_operations[0].clone();
            completion.current_configuration_id = Some(ConfigurationId::new("other"));
            completion
        },
        {
            let mut completion = accepted.completed_operations[0].clone();
            completion.storage_incarnation.storage_generation = StorageGeneration::new("other");
            completion
        },
        {
            let mut completion = accepted.completed_operations[0].clone();
            completion.result = PublicV1AcceptedResult::Cancelled;
            completion
        },
    ] {
        accepted.completed_operations = vec![completion];
        status["status"]["publicV1Authority"] = serde_json::to_value(
            PublicV1AuthorityStatus::from_authority(&accepted).map_err(anyhow::Error::msg)?,
        )?;
        assert!(!secondary_reconstructed_authority(
            &status, &before, &after, 2
        )?);
    }
    accepted.completed_operations.clear();
    status["status"]["publicV1Authority"] = serde_json::to_value(
        PublicV1AuthorityStatus::from_authority(&accepted).map_err(anyhow::Error::msg)?,
    )?;
    assert!(!secondary_reconstructed_authority(
        &status, &before, &after, 2
    )?);
    status["status"]["publicV1Topology"]["canonicalConfigurationJson"] = serde_json::json!("{}");
    assert!(secondary_reconstructed_authority(&status, &before, &after, 2).is_err());
    Ok(())
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
            && public_v1_topology_idle(&value)
            && value["status"]["publicV1Topology"]["sessions"]
                .as_array()
                .is_some_and(|sessions| sessions.len() == 3)
            && all_replicas_reporting(&kubeconfig, &context)
            && let Some(primary) = public_v1_primary(&kubeconfig, &context)?
            && (1..=3).all(|replica_id| {
                replica_diagnostics(&kubeconfig, &context, replica_id).is_ok_and(|report| {
                    topology_session_matches(&value, replica_id, &report["processSession"])
                })
            })
        {
            break primary;
        }
        if Instant::now() >= bootstrap_deadline {
            bail!("bootstrap did not produce three Public V1 reports and exactly one writer");
        }
        std::thread::sleep(Duration::from_secs(2));
    };
    ensure!(
        put_from_replica(
            &kubeconfig,
            &context,
            primary,
            "bootstrap-prefix",
            "acknowledged-before-restart",
        )? == 200,
        "initial three-member quorum write failed"
    );
    ensure!(
        get_from_replica(&kubeconfig, &context, primary, "bootstrap-prefix")?.trim()
            == "acknowledged-before-restart",
        "initial acknowledged bootstrap value was not readable"
    );
    eprintln!("M2 live bootstrap: exact three-member sessions and actual quorum write/read passed");

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

    let secondary = (1..=3)
        .find(|replica_id| *replica_id != primary)
        .context("bootstrap omitted a secondary")?;
    let before = replica_diagnostics(&kubeconfig, &context, secondary)?;
    stop_replica_process(&kubeconfig, &context, secondary)?;
    let restart_deadline = Instant::now() + Duration::from_secs(90);
    loop {
        if let Ok(after) = replica_diagnostics(&kubeconfig, &context, secondary)
            && secondary_reconstructed_authority(
                &set_status(&kubeconfig, &context)?,
                &before,
                &after,
                secondary,
            )?
        {
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
            && put_from_replica(&kubeconfig, &context, primary, "bootstrap", "phase6")
                .is_ok_and(|status| status == 200)
        {
            break;
        }
        if Instant::now() >= access_deadline {
            bail!("primary did not restore quorum writes after bootstrap restart");
        }
        std::thread::sleep(Duration::from_secs(2));
    }
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
        if let Ok(instance) = pod_uid_for_replica(&kubeconfig, &context, 2)
            && instance != old_instance
            && replica_diagnostics(&kubeconfig, &context, 2).is_ok()
        {
            let status = set_status(&kubeconfig, &context)?;
            let replacement_complete = status.pointer("/status/publicV1Replacement").is_none();
            let replacement_admitted = status
                .pointer("/status/publicV1Topology/sessions")
                .and_then(serde_json::Value::as_array)
                .is_some_and(|sessions| {
                    sessions.iter().any(|session| {
                        session
                            .pointer("/identity/replicaId")
                            .and_then(serde_json::Value::as_i64)
                            == Some(2)
                            && session
                                .pointer("/identity/instanceId")
                                .and_then(serde_json::Value::as_str)
                                == Some(instance.as_str())
                    })
                });
            if replacement_complete
                && replacement_admitted
                && public_v1_primary(&kubeconfig, &context)?.is_some()
            {
                break;
            }
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
        if let Ok(restarted) = replica_diagnostics(&kubeconfig, &context, 2)
            && secondary_reconstructed_authority(
                &set_status(&kubeconfig, &context)?,
                &before_restart,
                &restarted,
                2,
            )?
        {
            break;
        }
        if Instant::now() >= restart_deadline {
            bail!("replacement process did not reconstruct and publish accepted authority");
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
            && public_v1_topology_idle(&status)
            && all_replicas_reporting(&kubeconfig, &context)
            && let Some(primary) = public_v1_primary(&kubeconfig, &context)?
            && (1..=3).all(|replica_id| {
                replica_diagnostics(&kubeconfig, &context, replica_id).is_ok_and(|report| {
                    topology_session_matches(&status, replica_id, &report["processSession"])
                })
            })
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
    set_replica_process_paused(&kubeconfig, &context, old_primary, true)?;
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

    set_replica_process_paused(&kubeconfig, &context, old_primary, false)?;
    let fencing_deadline = Instant::now() + Duration::from_secs(120);
    loop {
        let old_fenced = replica_diagnostics(&kubeconfig, &context, old_primary)
            .is_ok_and(|status| status["writeStatus"] != "Granted");
        let new_still_primary = replica_diagnostics(&kubeconfig, &context, new_primary)
            .is_ok_and(|status| status["writeStatus"] == "Granted");
        if old_fenced && new_still_primary {
            break;
        }
        if Instant::now() >= fencing_deadline {
            bail!("recovered old primary was not fenced after failover");
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    ensure!(
        put_from_replica(
            &kubeconfig,
            &context,
            old_primary,
            "failover-stale-primary",
            "must-be-rejected",
        )? != 200,
        "recovered old primary accepted a post-failover write"
    );
    Ok(())
}
