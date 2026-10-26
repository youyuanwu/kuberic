use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use kuberic_runtime::protocol::public_operations::ReplicaAgentSnapshot;
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
    before: &ReplicaAgentSnapshot,
    after: &ReplicaAgentSnapshot,
    replica_id: i64,
) -> Result<bool> {
    use kuberic_controller::crd::KubericSetStatus;
    use kuberic_runtime::protocol::public_operations::*;
    use kuberic_runtime::protocol::types::*;

    ensure!(
        !before.process_session_id.is_empty(),
        "pre-restart process session is missing"
    );
    if after.format != ORDINARY_ACTION_FORMAT
        || after.storage.replica.replica_id.value() != replica_id
        || after.storage != before.storage
        || after.identity != before.identity
        || after.history != before.history
        || after.process_session_id.is_empty()
        || after.process_session_id == before.process_session_id
        || after.creation_id.is_empty()
        || after.creation_id == before.creation_id
        || after.open_mode != OpenMode::Existing
        || after.read_status != AccessStatus::NotPrimary
        || after.write_status != AccessStatus::NotPrimary
        || after.withdrawn
        || !public_v1_topology_idle(value)
        || !public_v1_authority_present(value)
    {
        return Ok(false);
    }
    let status: KubericSetStatus = serde_json::from_value(value["status"].clone())?;
    let topology = status.public_v1_topology.as_ref().unwrap();
    let configuration = topology.configuration().map_err(anyhow::Error::msg)?;
    if topology.pending_switchover.is_some()
        || !topology.sessions.iter().any(|endpoint| {
            endpoint.identity == after.storage.replica
                && endpoint.storage_incarnation == after.storage
                && endpoint.process_session_id == after.process_session_id
                && endpoint.replicator_creation_id == after.creation_id
                && endpoint.role == ReplicaRole::ActiveSecondary
        })
    {
        return Ok(false);
    }
    let authority = status
        .public_v1_authority
        .as_ref()
        .unwrap()
        .authority()
        .map_err(anyhow::Error::msg)?;
    let Some(publication) = &after.publication else {
        return Ok(false);
    };
    if publication.validate().is_err()
        || publication.stage != ReplicaPublicationStage::AchievedScope
        || publication.role != ReplicaRole::ActiveSecondary
        || publication.scope.storage != after.storage
        || publication.scope.process_session_id != after.process_session_id
        || publication.scope.creation_id != after.creation_id
        || publication.scope.history != after.history
        || publication.scope.configuration.current != configuration
        || publication.scope.configuration.previous.is_some()
        || authority.identity != after.identity
        || authority.history != after.history
        || authority.history.data_loss_number != configuration.epoch.data_loss_number
    {
        return Ok(false);
    }
    let current_goals = status
        .replica_goals
        .iter()
        .filter(|entry| {
            entry.goal.scope.storage == after.storage
                && entry.goal.scope.process_session_id == after.process_session_id
                && entry.goal.scope.creation_id == after.creation_id
        })
        .collect::<Vec<_>>();
    if current_goals.iter().any(|entry| {
        entry.validate().is_err()
            || entry.goal.identity != after.identity
            || entry.outcome.as_ref().is_none_or(|outcome| {
                outcome.status != ReplicaGoalStatus::Achieved || !after.goals.contains(outcome)
            })
    }) {
        return Ok(false);
    }
    Ok(current_goals.iter().any(|entry| {
        entry.goal.scope == publication.scope
            && matches!(
                entry.goal.desired,
                ReplicaGoalKind::Prepare {
                    role: ReplicaRole::ActiveSecondary
                } | ReplicaGoalKind::Reconfigure {
                    role: ReplicaRole::ActiveSecondary,
                    ..
                }
            )
            && entry.outcome.as_ref().is_some_and(|outcome| {
                outcome.status == ReplicaGoalStatus::Achieved
                    && outcome.receipts > 0
                    && outcome.selected_call.is_none()
            })
    }))
}

struct OwnedPortForward(Child);

impl Drop for OwnedPortForward {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn replica_ordinary_snapshot(
    kubeconfig: &str,
    context: &str,
    replica_id: i64,
) -> Result<ReplicaAgentSnapshot> {
    use kuberic_controller::cluster_api::{AgentApi, GrpcAgentApi};
    use kuberic_controller::crd::PublicV1TopologyState;
    use kuberic_runtime::control::proto;
    use kuberic_runtime::protocol::public_operations::*;

    let value = set_status(kubeconfig, context)?;
    let topology: PublicV1TopologyState =
        serde_json::from_value(value["status"]["publicV1Topology"].clone())?;
    let target = topology
        .sessions
        .iter()
        .find(|session| session.identity.replica_id.value() == replica_id)
        .context("ordinary snapshot requires an admitted replica identity")?;
    ensure!(
        pod_uid_for_replica(kubeconfig, context, replica_id)?
            == target.identity.instance_id.as_str(),
        "ordinary snapshot target Pod changed"
    );
    let token = kubectl(
        kubeconfig,
        context,
        &[
            "-n",
            NAMESPACE,
            "get",
            "secret",
            "kvstore2-agent-credentials",
            "-o",
            "go-template={{index .data \"bearer-token\" | base64decode}}",
        ],
    )?;
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    let address = listener.local_addr()?;
    drop(listener);
    let pod = pod_for_replica(kubeconfig, context, replica_id)?;
    let mut forward = OwnedPortForward(
        Command::new("kubectl")
            .args([
                "--kubeconfig",
                kubeconfig,
                "--context",
                context,
                "-n",
                NAMESPACE,
                "port-forward",
                "--address=127.0.0.1",
                &format!("pod/{pod}"),
                &format!("{}:50051", address.port()),
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?,
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        ensure!(
            forward.0.try_wait()?.is_none(),
            "owned snapshot port-forward exited"
        );
        if std::net::TcpStream::connect_timeout(&address, Duration::from_millis(100)).is_ok() {
            break;
        }
        ensure!(
            Instant::now() < deadline,
            "owned snapshot port-forward did not become ready"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    let response = tokio::runtime::Runtime::new()?
        .block_on(GrpcAgentApi::new(Duration::from_secs(5)).get_snapshot(
            &format!("http://{address}"),
            token.trim(),
            proto::ReplicaSnapshotRequest {
                protocol_version: ORDINARY_ACTION_FORMAT,
                resource_uid: value["metadata"]["uid"].as_str().context("set UID")?.into(),
                target: Some(target.identity.clone().into()),
                expected_process_session_id: String::new(),
            },
        ))
        .map_err(|_| anyhow::anyhow!("authenticated ordinary snapshot failed"))?;
    ensure!(
        response.protocol_version == ORDINARY_ACTION_FORMAT,
        "snapshot format changed"
    );
    let ReplicaAgentQueryResult::Initialized(snapshot) =
        serde_json::from_slice(&response.canonical_json)?
    else {
        bail!("ordinary replica is not initialized")
    };
    ensure!(
        snapshot.storage.replica == target.identity,
        "ordinary snapshot identity changed"
    );
    Ok(*snapshot)
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
    use kuberic_controller::crd::{
        PublicV1AuthorityStatus, PublicV1TopologyState, RetainedReplicaGoal,
    };
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
    let accepted = PublicV1AcceptedStatus {
        identity: PublicV1Identity::new(1),
        history_initialization_id: HistoryInitializationId::new("initialization"),
        history: history.clone(),
        data_loss_recovery: None,
        completed_operations: Vec::new(),
        extensions: PublicV1Extensions::default(),
    };
    let authority =
        PublicV1AuthorityStatus::from_authority(&accepted).map_err(anyhow::Error::msg)?;
    let scope = ReplicaScope {
        history: history.clone(),
        storage: sessions[1].storage_incarnation.clone(),
        process_session_id: sessions[1].process_session_id.clone(),
        creation_id: sessions[1].replicator_creation_id.clone(),
        epoch,
        configuration: ReplicaConfigurationScope {
            current: configuration.clone(),
            previous: None,
            replicas: vec![ReplicaPeerDescription {
                identity: sessions[0].identity.clone(),
                replication_address: "http://primary:50052".into(),
                process_session_id: sessions[0].process_session_id.clone(),
                storage_incarnation: sessions[0].storage_incarnation.clone(),
                role: ReplicaRole::Primary,
                current_progress: 10,
                catch_up_capability: 10,
                is_up: true,
                is_ready: true,
                must_catchup: false,
            }],
        },
    };
    let mut retained = RetainedReplicaGoal::new(ReplicaGoal {
        format: ORDINARY_ACTION_FORMAT,
        identity: accepted.identity.clone(),
        operation_id: OperationId::new("ordinary-reopen-prepare"),
        revision: 4,
        scope: scope.clone(),
        desired: ReplicaGoalKind::Prepare {
            role: ReplicaRole::ActiveSecondary,
        },
    })
    .map_err(anyhow::Error::msg)?;
    let observation = ReplicaGoalObservation {
        goal: retained.goal.clone(),
        status: ReplicaGoalStatus::Achieved,
        selected_call: None,
        receipts: 2,
    };
    retained.outcome = Some(observation.clone());
    let status = serde_json::json!({"status": {
        "publicV1Authority": authority,
        "publicV1Topology": topology,
        "replicaGoals": [retained],
    }});
    let after = ReplicaAgentSnapshot {
        format: ORDINARY_ACTION_FORMAT,
        identity: accepted.identity.clone(),
        history,
        storage: scope.storage.clone(),
        open_mode: OpenMode::Existing,
        read_status: AccessStatus::NotPrimary,
        write_status: AccessStatus::NotPrimary,
        withdrawn: false,
        provider_writer_held: true,
        process_session_id: scope.process_session_id.clone(),
        creation_id: scope.creation_id.clone(),
        publication: Some(ReplicaPublication {
            format: ORDINARY_ACTION_FORMAT,
            scope,
            replication_address: "http://secondary:50052".into(),
            generation: 2,
            stage: ReplicaPublicationStage::AchievedScope,
            role: ReplicaRole::ActiveSecondary,
        }),
        goals: vec![observation],
        remote_operations: Vec::new(),
        progress: ProgressQueryResult::Value(10),
        capability: ProgressQueryResult::Value(10),
    };
    let mut before = after.clone();
    before.process_session_id = ProcessSessionId::new("previous");
    before.creation_id = ReplicatorCreationId::new("previous-creation");
    assert!(secondary_reconstructed_authority(
        &status, &before, &after, 2
    )?);
    let mut wrong_identity = retained.clone();
    wrong_identity.goal.identity = PublicV1Identity::new(2);
    wrong_identity.digest = wrong_identity.goal.intent().input_digest;
    wrong_identity.outcome.as_mut().unwrap().goal = wrong_identity.goal.clone();
    let mut wrong_status = status.clone();
    wrong_status["status"]["replicaGoals"] = serde_json::json!([wrong_identity]);
    let mut wrong_snapshot = after.clone();
    wrong_snapshot.goals = vec![wrong_identity.outcome.unwrap()];
    assert!(!secondary_reconstructed_authority(
        &wrong_status,
        &before,
        &wrong_snapshot,
        2
    )?);
    for (pointer, value) in [
        ("/processSessionId", serde_json::json!("previous")),
        ("/creationId", serde_json::json!("previous-creation")),
        (
            "/storage/replica/instanceId",
            serde_json::json!("other-pod"),
        ),
        (
            "/storage/replica/agentGeneration",
            serde_json::json!("other-agent"),
        ),
        (
            "/storage/storageGeneration",
            serde_json::json!("other-storage"),
        ),
        ("/storage/storage/pvc_uid", serde_json::json!("other-pvc")),
        ("/readStatus", serde_json::json!("granted")),
        ("/writeStatus", serde_json::json!("granted")),
        ("/readStatus", serde_json::json!("reconfigurationPending")),
        ("/writeStatus", serde_json::json!("reconfigurationPending")),
        ("/withdrawn", serde_json::json!(true)),
        ("/publication", Value::Null),
        ("/publication/role", serde_json::json!("idleSecondary")),
        ("/publication/role", serde_json::json!("none")),
        ("/publication/stage", serde_json::json!("DiscoveryPending")),
        (
            "/publication/scope/processSessionId",
            serde_json::json!("previous"),
        ),
        (
            "/publication/scope/creationId",
            serde_json::json!("previous-creation"),
        ),
        (
            "/publication/scope/storage/storageGeneration",
            serde_json::json!("other-storage"),
        ),
        ("/goals/0/receipts", serde_json::json!(0)),
        ("/goals", serde_json::json!([])),
    ] {
        let mut pending = serde_json::to_value(&after)?;
        *pending
            .pointer_mut(pointer)
            .with_context(|| format!("missing fixture {pointer}"))? = value;
        let pending = serde_json::from_value(pending)?;
        assert!(
            !secondary_reconstructed_authority(&status, &before, &pending, 2)?,
            "{pointer}"
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
    for (pointer, value) in [
        ("/status/replicaGoals", serde_json::json!([])),
        ("/status/replicaGoals/0/outcome", Value::Null),
        (
            "/status/replicaGoals/0/outcome/receipts",
            serde_json::json!(0),
        ),
        (
            "/status/replicaGoals/0/outcome/status",
            serde_json::to_value(ReplicaGoalStatus::Failed("failed role".into()))?,
        ),
        (
            "/status/replicaGoals/0/outcome/status",
            serde_json::to_value(ReplicaGoalStatus::Pending)?,
        ),
        (
            "/status/replicaGoals/0/digest",
            serde_json::to_value({
                let mut wrong = retained.clone();
                wrong.goal.revision += 1;
                wrong.goal.intent().input_digest
            })?,
        ),
        (
            "/status/publicV1Topology/sessions/1/replicatorCreationId",
            serde_json::json!("previous-creation"),
        ),
        (
            "/status/publicV1Topology/sessions/1/storageIncarnation/storageGeneration",
            serde_json::json!("other-storage"),
        ),
    ] {
        let mut pending = status.clone();
        *pending
            .pointer_mut(pointer)
            .with_context(|| format!("missing fixture {pointer}"))? = value;
        assert!(
            !secondary_reconstructed_authority(&pending, &before, &after, 2)?,
            "{pointer}"
        );
    }
    let mut pending = status.clone();
    pending["status"]["publicV1Topology"]["canonicalConfigurationJson"] = serde_json::json!("{}");
    assert!(secondary_reconstructed_authority(&pending, &before, &after, 2).is_err());
    before.process_session_id = ProcessSessionId::new("");
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
    let before = replica_ordinary_snapshot(&kubeconfig, &context, secondary)?;
    stop_replica_process(&kubeconfig, &context, secondary)?;
    let restart_deadline = Instant::now() + Duration::from_secs(90);
    loop {
        if let Ok(after) = replica_ordinary_snapshot(&kubeconfig, &context, secondary)
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

    let before_restart = replica_ordinary_snapshot(&kubeconfig, &context, 2)?;
    stop_replica_process(&kubeconfig, &context, 2)?;
    let restart_deadline = Instant::now() + Duration::from_secs(90);
    loop {
        if let Ok(restarted) = replica_ordinary_snapshot(&kubeconfig, &context, 2)
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
