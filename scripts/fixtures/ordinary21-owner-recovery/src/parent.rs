use std::net::TcpListener as PortReservation;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, bail, ensure};
use kuberic_runtime::control::{convert, proto};
use kuberic_runtime::protocol::public_operations::*;
use kuberic_runtime::protocol::types::*;
use rusqlite::{Connection, OpenFlags};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::TcpStream;
use tokio::process::{Child, Command};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::{Instant, sleep, timeout_at};

use crate::shared::*;

type Client = proto::agent_control_client::AgentControlClient<tonic::transport::Channel>;

const BUDGET: Duration = Duration::from_secs(240);
const POLL: Duration = Duration::from_millis(25);

struct Member {
    child: Child,
    pid: u32,
    os_start: String,
    ready: ChildReady,
    events: mpsc::UnboundedReceiver<ChildEvent>,
    reader: Option<JoinHandle<anyhow::Result<()>>>,
    session: ProcessSessionId,
    stopped: bool,
    reaped: bool,
}

struct Driver {
    token: String,
    members: Vec<Member>,
    peers: Vec<PeerEndpoint>,
    deadline: Instant,
}

fn emit(step: &str, evidence: Value) {
    println!("{}", json!({"step": step, "evidence": evidence}));
}

fn os_start(pid: u32) -> anyhow::Result<String> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat"))?;
    let fields = stat.rsplit_once(") ").context("invalid owned process stat")?.1;
    Ok(fields.split_whitespace().nth(19).context("missing OS start time")?.into())
}

fn journal(root: &Path) -> anyhow::Result<Value> {
    let connection = Connection::open_with_flags(
        root.join("agent/.kuberic/agent.sqlite3"),
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    let text: String = connection.query_row(
        "SELECT state_json FROM agent_state WHERE singleton = 1",
        [],
        |row| row.get(0),
    )?;
    Ok(serde_json::from_str(&text)?)
}

fn operations(state: &Value) -> anyhow::Result<&serde_json::Map<String, Value>> {
    state["publicV1OperationState"]["operations"]
        .as_object()
        .context("missing actual ordinary journal")
}

fn record<'a>(state: &'a Value, goal: &ReplicaGoal) -> anyhow::Result<&'a Value> {
    operations(state)?.get(goal.operation_id.as_str()).context("missing operation")
}

fn publication(snapshot: &ReplicaAgentSnapshot) -> anyhow::Result<&ReplicaPublication> {
    snapshot.publication.as_ref().context("missing actual publication")
}

fn sampled_peer(snapshot: &ReplicaAgentSnapshot, role: ReplicaRole) -> anyhow::Result<ReplicaPeerDescription> {
    let publication = publication(snapshot)?;
    ensure!(publication.stage == ReplicaPublicationStage::AchievedScope && publication.role == role,
        "peer lacks actual achieved role");
    let progress = snapshot.progress.value().context("native progress unavailable")?;
    let capability = snapshot.capability.value().context("native capability unavailable")?;
    ensure!(progress >= 0 && capability >= 0, "native progress not initialized");
    Ok(ReplicaPeerDescription {
        identity: snapshot.storage.replica.clone(),
        replication_address: publication.replication_address.clone(),
        process_session_id: snapshot.process_session_id.clone(),
        storage_incarnation: snapshot.storage.clone(),
        role,
        current_progress: progress,
        catch_up_capability: capability,
        is_up: true,
        is_ready: true,
        must_catchup: false,
    })
}

impl Driver {
    async fn spawn(&mut self, root: &Path, stable: StableReplica, hold: bool, contender: bool) -> anyhow::Result<usize> {
        let mut peers = self.peers.clone();
        if contender {
            let socket = PortReservation::bind("127.0.0.1:0")?;
            peers.iter_mut().find(|peer| peer.identity == stable.identity()).context("missing contender peer")?
                .control_endpoint = format!("http://{}", socket.local_addr()?);
        }
        let control = peers.iter().find(|peer| peer.identity == stable.identity())
            .context("missing owned peer")?.control_endpoint.trim_start_matches("http://").to_owned();
        let mut command = Command::new(std::env::current_exe()?);
        command.args(["child", "--root"]).arg(root)
            .args(["--resource", stable.resource_uid.as_str(), "--replica", &stable.replica_id.to_string(),
                "--pod", stable.pod_uid.as_str(), "--pvc", stable.pvc_uid.as_str(),
                "--control-bind", &control, "--replication-bind", "127.0.0.1:0",
                "--data-bind", "127.0.0.1:0", "--peers-json", &serde_json::to_string(&peers)?])
            .env(TOKEN_ENV, &self.token)
            .stdout(Stdio::piped()).stderr(Stdio::inherit());
        if hold { command.arg("--hold-first-open"); }
        let mut child = command.spawn()?;
        let pid = child.id().context("child has no PID")?;
        let start = os_start(pid).unwrap_or_default();
        let stdout = child.stdout.take().context("missing owned stdout")?;
        let (send, events) = mpsc::unbounded_channel();
        let reader = tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Some(line) = lines.next_line().await? {
                let event: ChildEvent = serde_json::from_str(&line).context("invalid child stdout")?;
                emit("child", json!({"ownedPid": pid, "event": event}));
                send.send(event).map_err(|_| anyhow::anyhow!("child event observer lost"))?;
            }
            Ok(())
        });
        // Retain every OS owner before awaiting any observation, including failed startup.
        let placeholder = ChildReady {
            process_id: pid, root: root.into(), agent_root: root.join("agent"),
            application_root: root.join("application"), identity: stable.identity(),
            initialization_id: stable.initialization_id(),
            expected_bootstrap_history: stable.bootstrap_history(),
            expected_initial_storage: stable.initial_storage(), stable: stable.clone(),
            control_endpoint: format!("http://{control}"), data_endpoint: "127.0.0.1:0".parse()?,
            requested_replication_bind: "127.0.0.1:0".parse()?, application_was_empty: false,
            hold_first_open: hold,
        };
        let index = self.members.len();
        self.members.push(Member { child, pid, os_start: start, ready: placeholder, events, reader: Some(reader),
            session: ProcessSessionId::new(""), stopped: false, reaped: false });
        ensure!(!self.members[index].os_start.is_empty(), "owned child exited before OS identity observation");
        let ready_event = timeout_at(self.deadline, self.members[index].events.recv()).await;
        let ChildEvent::IpcReady { ready } = ready_event?.context("child exited before IPC readiness")? else {
            bail!("child failed before IPC readiness");
        };
        ensure!(ready.process_id == pid && ready.root == root && ready.identity == stable.identity()
            && ready.agent_root == root.join("agent") && ready.application_root == root.join("application"),
            "child crossed owned root/process/identity");
        self.members[index].ready = ready;
        Ok(index)
    }

    async fn client(&self, index: usize) -> anyhow::Result<Client> {
        Ok(Client::connect(self.members[index].ready.control_endpoint.clone()).await?)
    }

    async fn initialize(&mut self, index: usize, bootstrap: &ConfigurationDescriptor) -> anyhow::Result<()> {
        let ready = self.members[index].ready.clone();
        let request = proto::GetAgentStatusRequest {
            protocol_version: kuberic_runtime::protocol::PROTOCOL_VERSION,
            resource_uid: ready.stable.resource_uid.to_string(), replica_id: ready.identity.replica_id.value(),
            expected_instance_id: ready.identity.instance_id.to_string(),
        };
        let status = loop {
            if let Ok(mut client) = self.client(index).await
                && let Ok(response) = client.get_status(authorized(request.clone(), &self.token)?).await {
                break response.into_inner();
            }
            sleep(POLL).await;
        };
        emit("actualGetStatus", json!({"pid": self.members[index].pid,
            "processSession": status.process_session_id, "storageState": status.storage_state}));
        if status.storage_state == proto::AgentStorageState::Uninitialized as i32 {
            let stable = &ready.stable;
            let policy = EffectivePolicy::fixed(2, 1).context("invalid original policy")?;
            self.client(index).await?.execute(authorized(proto::ExecuteCommandRequest {
                protocol_version: kuberic_runtime::protocol::PROTOCOL_VERSION,
                resource_uid: stable.resource_uid.to_string(), target: Some(ready.identity.clone().into()),
                expected_process_session_id: status.process_session_id,
                command: Some(proto::execute_command_request::Command::InitializeAgentStore(
                    proto::InitializeAgentStoreCommand {
                        initialization_id: ready.initialization_id.to_string(),
                        resource_uid: stable.resource_uid.to_string(), local_replica_id: stable.replica_id.value(),
                        expected_instance_id: ready.identity.instance_id.to_string(),
                        expected_pod_uid: stable.pod_uid.to_string(), expected_pvc_uid: stable.pvc_uid.to_string(),
                        assigned_agent_generation: ready.identity.agent_generation.to_string(),
                        effective_policy: Some(policy.into()), bootstrap_configuration: Some(bootstrap.clone().into()),
                        provisioning: None,
                    })),
            }, &self.token)?).await?;
            emit("actualInitializeAgentStore", json!({"pid": self.members[index].pid, "bootstrap": bootstrap}));
        } else {
            let report = convert::normalize_public_v1_agent_status_report(status)?;
            ensure!(report.replica == ready.identity && report.history == ready.expected_bootstrap_history
                && report.storage_incarnation == ready.expected_initial_storage,
                "reopen did not retain initialized public status history/storage");
        }
        loop {
            match self.members[index].events.recv().await.context("host event owner lost")? {
                ChildEvent::HostStarted { process_id, process_session_id } => {
                    ensure!(process_id == self.members[index].pid, "host PID differs");
                    self.members[index].session = process_session_id;
                    break;
                }
                ChildEvent::Failed { message } => bail!("child startup failed: {message}"),
                ChildEvent::Stopped { failures, .. } => bail!("child stopped during initialization: {failures:?}"),
                other => bail!("unexpected startup event: {other:?}"),
            }
        }
        let actual = self.snapshot(index, false).await?;
        ensure!(actual.history == ready.expected_bootstrap_history
            && actual.storage == ready.expected_initial_storage && actual.identity == ready.stable.binding().identity,
            "normal initialized history/storage differ from stable bootstrap");
        emit("actualInitializedSnapshot", serde_json::to_value(actual)?);
        Ok(())
    }

    async fn snapshot(&self, index: usize, sample: bool) -> anyhow::Result<ReplicaAgentSnapshot> {
        let member = &self.members[index];
        let response = self.client(index).await?.get_snapshot(authorized(proto::ReplicaSnapshotRequest {
            protocol_version: ORDINARY_ACTION_FORMAT, resource_uid: member.ready.stable.resource_uid.to_string(),
            target: Some(member.ready.identity.clone().into()), expected_process_session_id: member.session.to_string(),
            sample_native_progress: sample,
        }, &self.token)?).await?.into_inner();
        let ReplicaAgentQueryResult::Initialized(actual) = serde_json::from_slice(&response.canonical_json)? else {
            bail!("initialized host became uninitialized");
        };
        ensure!(actual.process_session_id == member.session, "snapshot process crossed");
        Ok(*actual)
    }

    async fn make_goal(&self, index: usize, desired: ReplicaGoalKind, configuration: ReplicaConfigurationScope)
        -> anyhow::Result<ReplicaGoal> {
        let snapshot = self.snapshot(index, false).await?;
        let revision = snapshot.accepted_revision_high_watermark.max(
            snapshot.goals.iter().map(|goal| goal.goal.revision).max().unwrap_or(0)) + 1;
        let goal = ReplicaGoal {
            format: ORDINARY_ACTION_FORMAT, identity: snapshot.identity,
            operation_id: OperationId::new(format!("normal-owner-{}-{}-{revision}",
                snapshot.storage.replica.replica_id, snapshot.process_session_id)),
            revision, scope: ReplicaScope { history: snapshot.history, storage: snapshot.storage,
                process_session_id: snapshot.process_session_id, creation_id: snapshot.creation_id,
                epoch: configuration.current.epoch, configuration }, desired,
        };
        goal.validate().map_err(anyhow::Error::msg)?;
        Ok(goal)
    }

    async fn submit(&self, index: usize, goal: &ReplicaGoal) -> anyhow::Result<()> {
        emit("actualSubmitGoal", serde_json::to_value(goal)?);
        self.client(index).await?.submit_goal(authorized(proto::ReplicaGoalRequest {
            protocol_version: ORDINARY_ACTION_FORMAT, canonical_json: serde_json::to_vec(goal)?,
        }, &self.token)?).await?;
        Ok(())
    }

    async fn acknowledge(&self, index: usize, goal: &ReplicaGoal) -> anyhow::Result<()> {
        self.client(index).await?.acknowledge_goal(authorized(proto::ReplicaGoalRequest {
            protocol_version: ORDINARY_ACTION_FORMAT,
            canonical_json: serde_json::to_vec(&ReplicaGoalAcknowledgement { goal: goal.clone(), logically_retired: false })?,
        }, &self.token)?).await?;
        emit("actualAcknowledgeGoal", json!({"operation": goal.operation_id}));
        Ok(())
    }

    async fn achieve(&self, index: usize, goal: &ReplicaGoal) -> anyhow::Result<ReplicaAgentSnapshot> {
        self.submit(index, goal).await?;
        loop {
            let snapshot = self.snapshot(index, false).await?;
            let observation = snapshot.goals.iter().find(|entry| entry.goal == *goal).context("accepted goal missing")?;
            match &observation.status {
                ReplicaGoalStatus::Achieved => {
                    self.acknowledge(index, goal).await?;
                    let settled = self.snapshot(index, false).await?;
                    ensure!(settled.goals.iter().any(|entry| entry.goal == *goal && entry.host_settled),
                        "accepted goal lacks actual host settlement");
                    emit("actualGoalAchieved", serde_json::to_value(&settled)?);
                    return Ok(settled);
                }
                ReplicaGoalStatus::Pending => {
                    if matches!(goal.desired, ReplicaGoalKind::RecoverContainedCreation { .. }) {
                        ensure!(snapshot.read_status != AccessStatus::Granted && snapshot.write_status != AccessStatus::Granted,
                            "contained reconstruction exposed application access before actual achievement");
                    }
                    sleep(POLL).await;
                }
                other => bail!("ordinary goal failed: {other:?}; snapshot={snapshot:?}"),
            }
        }
    }

    async fn goal(&self, index: usize, desired: ReplicaGoalKind, configuration: ReplicaConfigurationScope)
        -> anyhow::Result<ReplicaGoal> {
        let goal = self.make_goal(index, desired, configuration).await?;
        self.achieve(index, &goal).await?;
        Ok(goal)
    }

    async fn ipc(&self, index: usize, command: ChildCommand) -> anyhow::Result<Result<ChildReply, ChildError>> {
        let mut socket = TcpStream::connect(self.members[index].ready.data_endpoint).await?;
        write_frame(&mut socket, &ParentRequest { request_id: 1, bearer_token: self.token.clone(), command }).await?;
        let reply: ParentReply = read_frame(&mut socket).await?;
        ensure!(reply.request_id == 1, "crossed data response");
        Ok(reply.result)
    }

    async fn value(&self, index: usize, expected: i64) -> anyhow::Result<()> {
        loop {
            match self.ipc(index, ChildCommand::CommittedValueDiagnostic).await? {
                Ok(ChildReply::CommittedValueDiagnostic { value }) if value == expected => return Ok(()),
                Ok(ChildReply::CommittedValueDiagnostic { .. }) => sleep(POLL).await,
                other => bail!("real committed diagnostic failed: {other:?}"),
            }
        }
    }

    fn same_source(&mut self, original: &ReplicaAgentSnapshot) -> anyhow::Result<()> {
        let source = &mut self.members[0];
        ensure!(source.child.try_wait()?.is_none() && os_start(source.pid)? == source.os_start,
            "original source OS lifetime was lost");
        ensure!(source.session == original.process_session_id, "source process session changed");
        Ok(())
    }

    async fn shutdown(&mut self, index: usize) -> anyhow::Result<()> {
        ensure!(matches!(self.ipc(index, ChildCommand::Shutdown).await?,
            Ok(ChildReply::ShutdownRequested)), "shutdown IPC failed");
        let mut failures = Vec::new();
        while let Some(event) = self.members[index].events.recv().await {
            match event {
                ChildEvent::Stopped { process_id, failures: actual } => {
                    ensure!(process_id == self.members[index].pid, "cleanup PID crossed");
                    failures.extend(actual);
                    self.members[index].stopped = true;
                }
                ChildEvent::IpcError { message } | ChildEvent::Failed { message } => failures.push(message),
                other => failures.push(format!("unexpected cleanup event: {other:?}")),
            }
        }
        let status = self.members[index].child.wait().await?;
        self.members[index].reaped = true;
        if let Some(reader) = self.members[index].reader.take() { reader.await??; }
        ensure!(status.success() && self.members[index].stopped && failures.is_empty(),
            "genuine cleanup failed: status={status}, failures={failures:?}");
        emit("actualGracefulJoin", json!({"pid": self.members[index].pid, "status": status.to_string()}));
        Ok(())
    }

    async fn kill_held(&mut self, index: usize) -> anyhow::Result<()> {
        let member = &mut self.members[index];
        ensure!(member.child.try_wait()?.is_none() && os_start(member.pid)? == member.os_start,
            "held target not the exact live owned OS process");
        member.child.start_kill()?;
        let status = member.child.wait().await?;
        member.reaped = true;
        ensure!(!status.success(), "abrupt target kill unexpectedly successful");
        if let Some(reader) = member.reader.take() { reader.await??; }
        emit("actualOsKillAndReap", json!({"pid": member.pid, "osStart": member.os_start,
            "processSession": member.session, "waitStatus": status.to_string()}));
        Ok(())
    }

    async fn scenario(&mut self, root: &Path, stable: &[StableReplica], bootstrap: &ConfigurationDescriptor)
        -> anyhow::Result<()> {
        for (i, identity) in stable.iter().enumerate() {
            let member_root = root.join(if i == 0 { "source" } else { "target" });
            std::fs::create_dir(&member_root)?;
            let index = self.spawn(&member_root, identity.clone(), false, false).await?;
            self.initialize(index, bootstrap).await?;
        }
        let singleton = ConfigurationDescriptor::new(Epoch::new(0, 1), stable[0].replica_id,
            vec![ConfigurationMember { identity: stable[0].identity(), role: ReplicaRole::Primary }], 1);
        let initial = ReplicaConfigurationScope { current: singleton.clone(), previous: None, replicas: Vec::new() };
        self.goal(0, ReplicaGoalKind::Prepare { role: ReplicaRole::Primary, rebuild: None }, initial.clone()).await?;
        self.goal(1, ReplicaGoalKind::Prepare { role: ReplicaRole::IdleSecondary, rebuild: None }, initial.clone()).await?;
        let source = self.snapshot(0, true).await?;
        let target = self.snapshot(1, true).await?;
        let remote = RemoteReplicaSelection { source: publication(&source)?.clone(), target: publication(&target)?.clone(),
            original_member: None,
            target_control_address: self.members[1].ready.control_endpoint.clone(), generation: 1 };
        let built_goal = self.goal(0, ReplicaGoalKind::Build { remote: Box::new(remote.clone()) }, initial).await?;
        let source_peer = sampled_peer(&self.snapshot(0, true).await?, ReplicaRole::Primary)?;
        let built = self.snapshot(0, false).await?;
        ensure!(built.remote_operations.iter().any(|entry| entry.selection == remote
            && entry.accepted && entry.state == RemoteReplicaState::Ready),
            "initial admission lacks genuine accepted Build Ready");
        // Idle native samples are deliberately unavailable. Only the exact accepted
        // Build grants admission; unknown progress stays unknown.
        let target_peer = ReplicaPeerDescription {
            identity: target.storage.replica.clone(),
            replication_address: publication(&target)?.replication_address.clone(),
            process_session_id: target.process_session_id.clone(), storage_incarnation: target.storage.clone(),
            role: ReplicaRole::ActiveSecondary, current_progress: INVALID_LSN, catch_up_capability: INVALID_LSN,
            is_up: true, is_ready: true, must_catchup: false,
        };
        let current = bootstrap.clone();
        let joint_source = ReplicaConfigurationScope { current: current.clone(), previous: Some(singleton),
            replicas: vec![target_peer.clone()] };
        self.goal(0, ReplicaGoalKind::Reconfigure { role: ReplicaRole::Primary, catch_up: None }, joint_source.clone()).await?;
        let mut joint_target = joint_source;
        joint_target.replicas = vec![source_peer.clone()];
        self.goal(1, ReplicaGoalKind::Reconfigure { role: ReplicaRole::ActiveSecondary, catch_up: None }, joint_target).await?;
        let source_configuration = ReplicaConfigurationScope { current: current.clone(), previous: None,
            replicas: vec![target_peer] };
        self.goal(0, ReplicaGoalKind::Reconfigure { role: ReplicaRole::Primary, catch_up: Some(PublicCatchUpMode::All) },
            source_configuration.clone()).await?;
        let target_configuration = ReplicaConfigurationScope { current: current.clone(), previous: None, replicas: vec![source_peer] };
        let admitted = self.goal(1, ReplicaGoalKind::Reconfigure { role: ReplicaRole::ActiveSecondary, catch_up: None },
            target_configuration.clone()).await?;
        let source_original = self.snapshot(0, true).await?;
        let target_original = self.snapshot(1, true).await?;
        ensure!(source_original.write_status == AccessStatus::Granted && current.write_quorum == 2 && current.members.len() == 2,
            "original full quorum not admitted");
        let acknowledged = self.ipc(0, ChildCommand::Set { value: 41 }).await?;
        let Ok(ChildReply::SetAcknowledged { lsn: first_lsn }) = acknowledged else { bail!("real write failed: {acknowledged:?}"); };
        self.value(0, 41).await?;
        self.value(1, 41).await?;
        emit("originalFullQuorumAcknowledged", json!({"lsn": first_lsn, "value": 41,
            "source": source_original, "target": target_original, "oldReady": remote}));
        self.shutdown(1).await?;
        self.same_source(&source_original)?;

        let held = self.spawn(&root.join("target"), stable[1].clone(), true, false).await?;
        self.initialize(held, bootstrap).await?;
        let held_snapshot = self.snapshot(held, false).await?;
        ensure!(held_snapshot.open_mode == OpenMode::Existing && held_snapshot.publication.is_none()
            && held_snapshot.process_session_id != target_original.process_session_id
            && held_snapshot.creation_id != target_original.creation_id, "restart did not retain a fresh closed Existing creation");
        let discover_configuration = ReplicaConfigurationScope { current: current.clone(), previous: None, replicas: Vec::new() };
        let discover = self.make_goal(held, ReplicaGoalKind::Discover, discover_configuration).await?;
        self.submit(held, &discover).await?;
        let mut overrides_rejected = false;
        loop {
            match self.members[held].events.recv().await.context("held Open observer lost")? {
                ChildEvent::BoundFactoryOverridesRejected { process_id, callbacks, variants } => {
                    ensure!(process_id == self.members[held].pid && callbacks == 0 && variants.len() == 4,
                        "bound factory callback probe failed");
                    overrides_rejected = true;
                }
                ChildEvent::OpenHeld { process_id, identity, mode, process_session_id, replicator_creation_id } => {
                    ensure!(process_id == self.members[held].pid && identity == stable[1].identity()
                        && mode == OpenMode::Existing && process_session_id.as_ref() == Some(&discover.scope.process_session_id)
                        && replicator_creation_id.is_none(), "Open callback crossed actual host custody");
                    break;
                }
                other => bail!("unexpected held child event: {other:?}"),
            }
        }
        ensure!(overrides_rejected, "normal-feature bound override/wrapper probes not executed");
        let selected_state = journal(&root.join("target"))?;
        let selected_record = record(&selected_state, &discover)?.clone();
        let owner = &selected_record["ordinary"]["creation_owner"];
        ensure!(selected_record["lifecycle"]["in_flight"] == "ApplicationOpen"
            && selected_record["lifecycle"]["outcomes"].as_array().is_some_and(Vec::is_empty)
            && selected_record["ordinary"]["actions"] == json!(["ApplicationOpen", "ReplicatorOpen"])
            && owner["process_id"] == self.members[held].pid
            && owner["process_session_id"] == serde_json::to_value(&discover.scope.process_session_id)?
            && owner["creation_id"] == serde_json::to_value(&discover.scope.creation_id)?
            && owner["process_lifetime"].as_str().is_some_and(|s| !s.is_empty()),
            "actual pre-effect selected owner stamp missing: {selected_record}");
        let factory_id = owner["builtin_factory"].as_str().context("no retained builtin factory binding")?;
        let factory = operations(&selected_state)?.get(factory_id).context("factory provenance record absent")?;
        ensure!(factory["lifecycle"]["outcomes"].as_array().is_some_and(|outcomes|
            outcomes.iter().any(|outcome| outcome.get("BuiltinNativeEndpoint").is_some())),
            "bound record lacks genuine builtin endpoint receipt: {factory}");
        let admission_record = record(&selected_state, &admitted)?;
        ensure!(admission_record["ordinary"]["accepted"] == true && admission_record["ordinary"]["host_settled"] == true
            && admission_record["stage"] == "completed" && admission_record["disposition"]["kind"] == "succeeded",
            "historical admission was not accepted/joined");
        let paired = operations(&selected_state)?.values().filter(|entry|
            entry["intent"]["processSessionId"] == serde_json::to_value(&target_original.process_session_id).unwrap_or(Value::Null)
                && entry["ordinary"]["actions"].as_array().is_some_and(|actions|
                    actions.contains(&json!("ReplicatorRole")) && actions.contains(&json!("ApplicationRole"))))
            .collect::<Vec<_>>();
        ensure!(!paired.is_empty() && paired.iter().all(|entry|
            entry["ordinary"]["host_settled"] == true && entry["ordinary"]["accepted"] == true
                && entry["stage"] == "completed" && entry["disposition"]["kind"] == "succeeded"),
            "historical paired native/application role lacks real accepted joined receipts");
        emit("actualReadOnlySelectedApplicationOpen", json!({"osPid": self.members[held].pid,
            "osStart": self.members[held].os_start, "record": selected_record, "factory": factory,
            "admitted": admission_record, "pairedRoles": paired}));
        ensure!(matches!(self.ipc(held, ChildCommand::ServingRead).await?, Err(ChildError { kind: ChildErrorKind::NotOpen, .. })),
            "held application unexpectedly accessible");
        let contender = self.spawn(&root.join("target"), stable[1].clone(), false, true).await?;
        self.expect_refused(contender).await?;
        self.same_source(&source_original)?;
        self.kill_held(held).await?;
        self.same_source(&source_original)?;

        let fresh = self.spawn(&root.join("target"), stable[1].clone(), false, false).await?;
        self.initialize(fresh, bootstrap).await?;
        let reopened = self.snapshot(fresh, false).await?;
        ensure!(reopened.history == target_original.history && reopened.storage == target_original.storage
            && reopened.open_mode == OpenMode::Existing && reopened.publication.is_none()
            && reopened.read_status != AccessStatus::Granted && reopened.write_status != AccessStatus::Granted
            && reopened.process_session_id != discover.scope.process_session_id
            && reopened.creation_id != discover.scope.creation_id, "fresh reacquisition lost exact closed retained roots");
        let uncertain = reopened.goals.iter().find(|entry| entry.goal == discover).context("old Discover lost")?.clone();
        ensure!(matches!(uncertain.status, ReplicaGoalStatus::Uncertain(_)), "old selected work not preserved uncertain: {uncertain:?}");
        let dependencies = reopened.reconstruction_dependencies.iter().find(|group|
            group.process_session_id == discover.scope.process_session_id && group.creation_id == discover.scope.creation_id)
            .context("actual Open-only predecessor dependencies unavailable")?;
        let old_before = record(&journal(&root.join("target"))?, &discover)?.clone();
        let mut recovery_configuration = target_configuration.clone();
        for peer in &mut recovery_configuration.replicas {
            peer.current_progress = INVALID_LSN; peer.catch_up_capability = INVALID_LSN;
            peer.is_up = false; peer.is_ready = false; peer.must_catchup = false;
        }
        let recovery = self.make_goal(fresh, ReplicaGoalKind::RecoverContainedCreation {
            recovery: Box::new(ContainedCreationRecovery {
                predecessor_process_session_id: discover.scope.process_session_id.clone(),
                predecessor_creation_id: discover.scope.creation_id.clone(), predecessor_operations: dependencies.operations.clone(),
                receiver_settlements: Vec::new(), role: ReplicaRole::ActiveSecondary, catch_up: None,
            }) }, recovery_configuration).await?;
        self.achieve(fresh, &recovery).await?;
        let recovered = self.snapshot(fresh, true).await?;
        ensure!(recovered.read_status != AccessStatus::Granted && recovered.write_status != AccessStatus::Granted
            && publication(&recovered)?.stage == ReplicaPublicationStage::AchievedScope,
            "local initialized secondary published application access");
        let recovery_state = journal(&root.join("target"))?;
        let recovery_record = record(&recovery_state, &recovery)?;
        let receipt = &recovery_record["ordinary"]["reconstruction"];
        ensure!(receipt["predecessor_process_id"] == self.members[held].pid
            && receipt["owner_process_id"] == self.members[fresh].pid
            && receipt["predecessor_process_lifetime"] == owner["process_lifetime"]
            && receipt["owner_process_lifetime"] != owner["process_lifetime"]
            && receipt["exclusion"] == "ExclusiveProcessReacquisition", "no exact OS-exclusion receipt: {receipt}");
        let old_after = record(&recovery_state, &discover)?;
        for key in ["stage", "disposition", "lifecycle", "attemptResults", "retryFailure", "acceptedHistory", "acceptedStorage"] {
            ensure!(old_before[key] == old_after[key], "recovery rewrote old uncertain fact: {key}");
        }
        ensure!(recovered.goals.iter().any(|entry| entry.goal == discover && entry.status == uncertain.status),
            "old uncertain outcome changed");
        emit("actualContainedCreationRecovery", json!({"record": recovery_record, "oldUncertainty": old_after}));
        self.value(fresh, 41).await?;
        self.deny_stale(fresh, &admitted).await?;
        self.deny_stale(fresh, &discover).await?;
        self.deny_stale(0, &built_goal).await?;

        let mut higher = current.clone();
        higher.epoch = Epoch::new(current.epoch.data_loss_number, current.epoch.configuration_number + 1);
        higher = ConfigurationDescriptor::new(higher.epoch, higher.primary_id, higher.members, higher.write_quorum);
        let target_joint = ReplicaConfigurationScope { current: higher.clone(), previous: Some(current.clone()),
            replicas: vec![sampled_peer(&self.snapshot(0, true).await?, ReplicaRole::Primary)?] };
        self.goal(fresh, ReplicaGoalKind::Reconfigure { role: ReplicaRole::ActiveSecondary, catch_up: None },
            target_joint).await?;
        let prepared = self.snapshot(fresh, true).await?;
        ensure!(prepared.read_status != AccessStatus::Granted && prepared.write_status != AccessStatus::Granted,
            "higher joint target preparation opened serving access");
        let target_fresh_peer = sampled_peer(&self.snapshot(fresh, true).await?, ReplicaRole::ActiveSecondary)?;
        let source_joint = ReplicaConfigurationScope { current: higher.clone(), previous: Some(current.clone()),
            replicas: vec![target_fresh_peer.clone()] };
        self.goal(0, ReplicaGoalKind::Reconfigure { role: ReplicaRole::Primary, catch_up: Some(PublicCatchUpMode::All) },
            source_joint).await?;
        let source_current = ReplicaConfigurationScope { current: higher.clone(), previous: None, replicas: vec![target_fresh_peer] };
        self.goal(0, ReplicaGoalKind::Reconfigure { role: ReplicaRole::Primary, catch_up: Some(PublicCatchUpMode::All) },
            source_current).await?;
        let source_fresh_peer = sampled_peer(&self.snapshot(0, true).await?, ReplicaRole::Primary)?;
        let final_target = ReplicaConfigurationScope { current: higher.clone(), previous: None, replicas: vec![source_fresh_peer] };
        self.goal(fresh, ReplicaGoalKind::Reconfigure { role: ReplicaRole::ActiveSecondary, catch_up: None }, final_target).await?;
        self.same_source(&source_original)?;
        let final_source = self.snapshot(0, true).await?;
        let final_target = self.snapshot(fresh, true).await?;
        ensure!(final_source.history == source_original.history && final_source.storage == source_original.storage
            && final_source.process_session_id == source_original.process_session_id && final_source.creation_id == source_original.creation_id
            && publication(&final_source)?.role == ReplicaRole::Primary && final_source.write_status == AccessStatus::Granted
            && final_target.history == target_original.history && final_target.storage == target_original.storage
            && higher.members == current.members && higher.write_quorum == current.write_quorum
            && [publication(&final_source)?, publication(&final_target)?].iter().all(|p|
                p.stage == ReplicaPublicationStage::AchievedScope && p.scope.configuration.current == higher
                    && p.scope.configuration.previous.is_none()), "final original full quorum/source identity changed");
        let acknowledged = self.ipc(0, ChildCommand::Set { value: 42 }).await?;
        let Ok(ChildReply::SetAcknowledged { lsn }) = acknowledged else { bail!("fresh ACK write failed: {acknowledged:?}"); };
        ensure!(lsn > first_lsn, "native acknowledged prefix did not advance");
        self.value(0, 42).await?;
        self.value(fresh, 42).await?;
        ensure!(matches!(self.ipc(0, ChildCommand::ServingRead).await?, Ok(ChildReply::ServingRead { value: 42 })),
            "actual Primary serving read failed");
        emit("positiveBodyComplete", json!({"acknowledgedLsn": lsn, "valueOnEveryOriginalMember": 42,
            "source": final_source, "target": final_target, "higherOriginalFullQuorum": higher}));
        Ok(())
    }

    async fn deny_stale(&self, index: usize, goal: &ReplicaGoal) -> anyhow::Result<()> {
        let before = self.snapshot(index, false).await?;
        let result = self.client(index).await?.submit_goal(authorized(proto::ReplicaGoalRequest {
            protocol_version: ORDINARY_ACTION_FORMAT, canonical_json: serde_json::to_vec(goal)?,
        }, &self.token)?).await;
        let error = result.err().context("old process/Ready goal unexpectedly admitted")?;
        let after = self.snapshot(index, false).await?;
        ensure!(before == after, "denied stale goal mutated normal host");
        emit("actualOldGoalDenied", json!({"operation": goal.operation_id, "code": format!("{:?}", error.code()),
            "message": error.message()}));
        Ok(())
    }

    async fn expect_refused(&mut self, index: usize) -> anyhow::Result<()> {
        let mut evidence = Vec::new();
        while let Some(event) = self.members[index].events.recv().await { evidence.push(event); }
        let status = self.members[index].child.wait().await?;
        self.members[index].reaped = true;
        if let Some(reader) = self.members[index].reader.take() { reader.await??; }
        let expected = format!("command rejected: storage lifetime is already owned at {}:",
            self.members[index].ready.agent_root.join(".kuberic/agent.sqlite3").display());
        ensure!(!status.success() && evidence.iter().filter(|event| matches!(event, ChildEvent::Stopped { .. })).count() == 1
            && evidence.iter().any(|event| matches!(event,
                ChildEvent::Stopped { process_id, failures } if *process_id == self.members[index].pid
                    && failures.len() == 1 && failures[0].starts_with(&expected)))
            && evidence.iter().all(|event| match event {
                ChildEvent::Stopped { failures, .. } => failures.len() == 1 && failures[0].starts_with(&expected),
                ChildEvent::Failed { message } => message.starts_with(&expected),
                _ => false,
            }),
            "concurrent owner was not refused by actual storage custody: {evidence:?}; {status}");
        emit("actualConcurrentOwnerRefusal", json!({"pid": self.members[index].pid, "events": evidence}));
        Ok(())
    }
}

pub async fn run(args: Vec<String>) -> anyhow::Result<()> {
    ensure!(args.len() == 3 && args[0] == "parent" && args[1] == "--root", "usage: parent --root <new absolute evidence root>");
    let root = PathBuf::from(&args[2]);
    ensure!(root.is_absolute() && !root.starts_with("/tmp") && !root.starts_with("/var/tmp")
        && !root.exists(), "proof root must be a new absolute non-temporary directory");
    std::fs::create_dir_all(&root)?;
    let root = root.canonicalize()?;
    let unique = root.file_name().context("missing root name")?.to_string_lossy();
    let resource = ResourceUid::new(format!("normal-owner-{unique}"));
    let stable = (1..=2).map(|id| StableReplica { resource_uid: resource.clone(), replica_id: ReplicaId::new(id),
        pod_uid: PodUid::new(format!("owner-pod-{id}")), pvc_uid: PvcUid::new(format!("owner-pvc-{id}")),
        generation: 1, spec_generation: 1 }).collect::<Vec<_>>();
    let reservations = (0..2).map(|_| PortReservation::bind("127.0.0.1:0")).collect::<std::io::Result<Vec<_>>>()?;
    let peers = reservations.iter().zip(&stable).map(|(socket, stable)| Ok(PeerEndpoint {
        identity: stable.identity(), control_endpoint: format!("http://{}", socket.local_addr()?),
        replication_endpoint: String::new(),
    })).collect::<std::io::Result<Vec<_>>>()?;
    drop(reservations);
    let bootstrap = ConfigurationDescriptor::new(Epoch::new(0, 1), stable[0].replica_id,
        stable.iter().enumerate().map(|(i, s)| ConfigurationMember { identity: s.identity(),
            role: if i == 0 { ReplicaRole::Primary } else { ReplicaRole::ActiveSecondary } }).collect(), 2);
    // The credential is fixture-private and never included in argv or evidence.
    let token = format!("owner-{}-{}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_nanos());
    let deadline = Instant::now() + BUDGET;
    let mut driver = Driver { token, members: Vec::new(), peers, deadline };
    emit("custody", json!({"parentPid": std::process::id(), "root": root, "budgetSeconds": BUDGET.as_secs(),
        "binary": std::env::current_exe()?, "workspaceCfgEnvironmentPresent": std::env::var_os("KUBERIC_WORKSPACE_TESTS").is_some()}));
    let body = timeout_at(deadline, driver.scenario(&root, &stable, &bootstrap)).await;
    let mut failures = Vec::new();
    match body { Ok(Ok(())) => {}, Ok(Err(error)) => failures.push(format!("{error:#}")), Err(error) => failures.push(error.to_string()) }
    for index in (0..driver.members.len()).rev() {
        if driver.members[index].reaped { continue; }
        match timeout_at(deadline, driver.shutdown(index)).await {
            Ok(Ok(())) => {},
            Ok(Err(error)) => failures.push(format!("cleanup PID {}: {error:#}", driver.members[index].pid)),
            Err(error) => failures.push(format!("cleanup PID {}: {error}", driver.members[index].pid)),
        }
        if !driver.members[index].reaped {
            let member = &mut driver.members[index];
            match member.child.start_kill() { Ok(()) => {}, Err(error) => failures.push(format!("emergency kill {}: {error}", member.pid)) }
            match member.child.wait().await { Ok(status) => emit("failureEmergencyReap", json!({"pid": member.pid, "status": status.to_string()})),
                Err(error) => failures.push(format!("emergency reap {}: {error}", member.pid)) }
            member.reaped = true;
            if let Some(reader) = member.reader.take() {
                match reader.await { Ok(Ok(())) => {}, other => failures.push(format!("reader join: {other:?}")) }
            }
        }
    }
    ensure!(failures.is_empty(), "normal-feature owner proof failed; retained root {}: {}",
        root.display(), failures.join("; "));
    emit("PASS", json!({"root": root, "allOwnedChildrenWaitedAndReaped": true,
        "normalControlOnly": true, "testingFeature": false}));
    Ok(())
}
