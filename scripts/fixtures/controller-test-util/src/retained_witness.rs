//! Opt-in physical restoration schedule over an actual hosted application.
//! The wrapper adds a fallible owned worker, not an alternate provider or ledger.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, ensure};
use async_trait::async_trait;
use kuberic_controller::test_util::{ApplicationFactory, CreatedApplication, ReplicaContext};
use kuberic_runtime::application::{OpenContext, OpenMode, RoleChange};
use kuberic_runtime::host::ApplicationStorageState;
use kuberic_runtime::protocol::public_operations::RetainedRecoveryExitNotice;
use kuberic_runtime::protocol::types::{FaultType, ReplicaRole};
use kuberic_runtime::replicator::{Replicator, StatefulServicePartition};
use kuberic_runtime::{RuntimeError, StatefulServiceReplica};
use serde_json::{Value, json};
use tokio::sync::{Mutex as AsyncMutex, oneshot, watch};
use tokio::task::JoinHandle;

fn application(error: impl std::fmt::Display) -> RuntimeError {
    RuntimeError::Application(error.to_string())
}

pub struct WitnessFactory<F> {
    inner: F,
    executable: PathBuf,
}

impl<F> WitnessFactory<F> {
    pub fn new(inner: F, executable: PathBuf) -> Self {
        Self { inner, executable }
    }
}

pub struct WitnessService<S> {
    inner: Option<Arc<S>>,
    context: ReplicaContext,
    partition: Mutex<Option<StatefulServicePartition>>,
    role: Mutex<ReplicaRole>,
    open_modes: Mutex<Vec<OpenMode>>,
    restoring: AtomicBool,
    worker: AsyncMutex<Option<JoinHandle<kuberic_runtime::Result<()>>>>,
    worker_failure: Mutex<Option<String>>,
    fault_started: AtomicBool,
    exit_notice: Mutex<Option<RetainedRecoveryExitNotice>>,
    exit_release: watch::Sender<bool>,
    role_entered: AtomicBool,
    role_release: watch::Sender<bool>,
}

impl<S> WitnessService<S> {
    fn new(inner: Option<Arc<S>>, context: ReplicaContext) -> Self {
        Self {
            inner,
            context,
            partition: Mutex::new(None),
            role: Mutex::new(ReplicaRole::None),
            open_modes: Mutex::new(Vec::new()),
            restoring: AtomicBool::new(false),
            worker: AsyncMutex::new(None),
            worker_failure: Mutex::new(None),
            fault_started: AtomicBool::new(false),
            exit_notice: Mutex::new(None),
            exit_release: watch::channel(false).0,
            role_entered: AtomicBool::new(false),
            role_release: watch::channel(false).0,
        }
    }

    fn inner(&self) -> kuberic_runtime::Result<Arc<S>> {
        self.inner.clone().ok_or(RuntimeError::NotOpen)
    }

    /// Actual service data adapter in the exact child, never local provider apply.
    pub async fn request(&self, request: Value) -> Result<Value> {
        Ok(self
            .context
            .member_data(json!({"application": request}))
            .await?)
    }

    pub async fn command(&self, command: &str) -> Result<()> {
        if command == "release-exit" {
            self.context
                .member_exit_gate_reply(json!({"witness": command}))
                .await?;
        } else {
            self.context
                .member_data(json!({"witness": command}))
                .await?;
        }
        Ok(())
    }

    pub fn observation(&self) -> Result<Value> {
        Ok(self.context.member_observation()?["application"].clone())
    }

    fn local_observation(&self) -> Value {
        json!({
            "pid": std::process::id(),
            "role": *self.role.lock().unwrap(),
            "openModes": *self.open_modes.lock().unwrap(),
            "restoring": self.restoring.load(Ordering::Acquire),
            "workerFailure": *self.worker_failure.lock().unwrap(),
            "exitNotice": *self.exit_notice.lock().unwrap(),
            "roleEntered": self.role_entered.load(Ordering::Acquire),
        })
    }

    async fn fail_owned_worker(&self) -> Result<()> {
        ensure!(
            *self.role.lock().unwrap() == ReplicaRole::ActiveSecondary,
            "fault requires an actual admitted application secondary"
        );
        ensure!(
            !self.fault_started.swap(true, Ordering::AcqRel),
            "worker fault already injected"
        );
        let partition = self.partition.lock().unwrap().clone().context("not open")?;
        let missing = self
            .context
            .application_root()
            .join("absent-retained-worker-input");
        ensure!(
            !missing.exists(),
            "worker failure input unexpectedly exists"
        );
        let (entered, observed) = oneshot::channel();
        let worker = tokio::spawn(async move {
            let _ = entered.send(());
            // A real callback-owned task fails I/O. Close must actually join it;
            // neither its failure nor its drainage is synthesized in a journal.
            std::fs::read(missing)
                .map(|_| ())
                .map_err(|error| application(format!("retained witness worker: {error}")))
        });
        *self.worker.lock().await = Some(worker);
        observed.await?;
        // The actual owned task has failed and is joined before reporting it.
        // The runtime's subsequent successful report is not a Close receipt.
        ensure!(
            self.join_worker().await.is_err(),
            "owned worker did not actually fail"
        );
        partition.report_fault(FaultType::Transient).await?;
        Ok(())
    }

    async fn join_worker(&self) -> kuberic_runtime::Result<()> {
        if let Some(worker) = self.worker.lock().await.take() {
            let error = match worker.await {
                Ok(Ok(())) => None,
                Ok(Err(error)) => Some(error.to_string()),
                Err(error) => Some(error.to_string()),
            };
            if let Some(error) = error {
                *self.worker_failure.lock().unwrap() = Some(error);
            }
        }
        match self.worker_failure.lock().unwrap().clone() {
            Some(error) => Err(application(error)),
            None => Ok(()),
        }
    }
}

#[async_trait]
impl<S: StatefulServiceReplica + 'static> StatefulServiceReplica for WitnessService<S> {
    async fn open(
        self: Arc<Self>,
        context: OpenContext,
    ) -> kuberic_runtime::Result<Arc<dyn Replicator>> {
        self.open_modes.lock().unwrap().push(context.mode);
        self.restoring
            .store(context.restoring_retained_member(), Ordering::Release);
        *self.partition.lock().unwrap() = Some(context.partition.clone());
        self.inner()?.open(context).await
    }

    async fn change_role(&self, role: ReplicaRole) -> kuberic_runtime::Result<RoleChange> {
        if self.open_modes.lock().unwrap().last() == Some(&OpenMode::Existing)
            && *self.role.lock().unwrap() == ReplicaRole::None
            && role == ReplicaRole::IdleSecondary
        {
            self.role_entered.store(true, Ordering::Release);
            self.role_release
                .subscribe()
                .wait_for(|released| *released)
                .await
                .map_err(application)?;
        }
        let result = self.inner()?.change_role(role).await?;
        *self.role.lock().unwrap() = role;
        Ok(result)
    }

    async fn close(&self) -> kuberic_runtime::Result<()> {
        let inner = self.inner()?.close().await;
        let worker = self.join_worker().await;
        match (inner, worker) {
            (Err(inner), Err(worker)) => Err(application(format!("{inner}; {worker}"))),
            (Err(error), _) | (_, Err(error)) => Err(error),
            _ => Ok(()),
        }
    }

    fn abort(&self) {
        self.role_release.send_replace(true);
        if let Some(inner) = &self.inner {
            inner.abort();
        }
    }
}

#[async_trait]
impl<F: ApplicationFactory> ApplicationFactory for WitnessFactory<F> {
    type Service = WitnessService<F::Service>;

    async fn create(
        &self,
        context: ReplicaContext,
    ) -> kuberic_controller::test_util::Result<CreatedApplication<Self::Service>> {
        if context.is_process_proxy() {
            return Ok(CreatedApplication::new(
                Arc::new(WitnessService::new(None, context)),
                ApplicationStorageState::Established,
            ));
        }
        let application = self.inner.create(context.clone()).await?;
        Ok(
            application
                .map_service(|service| Arc::new(WitnessService::new(Some(service), context))),
        )
    }

    async fn shutdown_and_drain(
        &self,
        service: Arc<Self::Service>,
    ) -> kuberic_controller::test_util::Result<()> {
        let worker = service.join_worker().await;
        let inner = self
            .inner
            .shutdown_and_drain(service.inner().map_err(anyhow::Error::new)?)
            .await;
        match (inner, worker) {
            (Err(inner), Err(worker)) => Err(anyhow::anyhow!("{inner}; {worker}").into()),
            (Err(error), _) => Err(error),
            (_, Err(error)) => Err(anyhow::Error::new(error).into()),
            _ => Ok(()),
        }
    }

    fn member_executable(&self) -> Option<PathBuf> {
        Some(self.executable.clone())
    }

    fn recoverable_member_exit(
        &self,
        context: &ReplicaContext,
        pid: u32,
        status: std::process::ExitStatus,
    ) -> kuberic_controller::test_util::Result<bool> {
        if status.success() {
            return Err(
                anyhow::anyhow!("restoration owner unexpectedly exited successfully").into(),
            );
        }
        context.validate_member_recovery_exit_cause(pid)?;
        Ok(true)
    }

    async fn before_member_recovery_exit(
        &self,
        service: Arc<Self::Service>,
        notice: RetainedRecoveryExitNotice,
    ) -> kuberic_controller::test_util::Result<()> {
        if notice.native_rebuild.is_some() {
            return Ok(());
        }
        *service.exit_notice.lock().unwrap() = Some(notice);
        let mut release = service.exit_release.subscribe();
        release
            .wait_for(|released| *released)
            .await
            .map_err(anyhow::Error::new)?;
        Ok(())
    }

    async fn member_data(
        &self,
        service: Arc<Self::Service>,
        request: Value,
    ) -> kuberic_controller::test_util::Result<Value> {
        let result: Result<Value> = async {
            ensure!(
                request.as_object().is_some_and(|object| object.len() == 1),
                "invalid witness request"
            );
            if let Some(command) = request.get("witness").and_then(Value::as_str) {
                match command {
                    "fail-worker" => service.fail_owned_worker().await?,
                    "release-exit" => {
                        ensure!(
                            service.exit_notice.lock().unwrap().is_some(),
                            "no actual runtime exit notice"
                        );
                    }
                    "release-role" => {
                        service.role_release.send_replace(true);
                    }
                    _ => anyhow::bail!("unknown witness scheduling command"),
                }
                return Ok(Value::Null);
            }
            Ok(self
                .inner
                .member_data(
                    service.inner()?,
                    request
                        .get("application")
                        .context("missing application request")?
                        .clone(),
                )
                .await?)
        }
        .await;
        result.map_err(Into::into)
    }

    fn member_data_reply_sent(&self, service: &Self::Service, request: &Value) {
        if request["witness"] == "release-exit" {
            service.exit_release.send_replace(true);
        }
    }

    fn member_observation(
        &self,
        service: &Self::Service,
    ) -> kuberic_controller::test_util::Result<Value> {
        let inner = service.inner().map_err(anyhow::Error::new)?;
        let application = self
            .inner
            .member_observation(&inner)
            .unwrap_or_else(|error| json!({"error": format!("{:#}", anyhow::Error::new(error))}));
        Ok(json!({"witness": service.local_observation(), "application": application}))
    }
}

pub fn helper_path(variable: &str, name: &str) -> PathBuf {
    if let Some(path) = std::env::var_os(variable) {
        return path.into();
    }
    let executable = std::env::current_exe().expect("current test executable");
    let directory = executable.parent().expect("test executable directory");
    let directory = if directory.file_name().is_some_and(|name| name == "deps") {
        directory.parent().expect("Cargo binary directory")
    } else {
        directory
    };
    directory.join(name)
}

/// Read the existing authority journal without opening a writer or a host.
/// This is observation of the source-owned format, not a parallel authority.
pub fn journal(application_root: &Path) -> Result<Value> {
    let path = application_root
        .parent()
        .context("application parent missing")?
        .join(".kuberic/agent.sqlite3");
    let connection =
        rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let state: String = connection.query_row(
        "SELECT state_json FROM agent_state WHERE singleton = 1",
        [],
        |row| row.get(0),
    )?;
    let state: Value = serde_json::from_str(&state)?;
    ensure!(
        state["identity"]["schemaVersion"]
            == kuberic_runtime::protocol::public_operations::ORDINARY_ACTION_FORMAT,
        "witness cannot inspect an incompatible retained journal"
    );
    Ok(state)
}

pub trait WitnessApplication: ApplicationFactory {
    fn initialize_request() -> Option<Value> {
        None
    }
    fn write_request(value: i64) -> Value;
    fn read_request() -> Value;
    fn write_lsn(response: Value) -> Result<i64>;
    fn read_value(response: Value) -> Result<i64>;
}

fn operation<'a>(journal: &'a Value, id: &str) -> Result<&'a Value> {
    journal["publicV1OperationState"]["operations"]
        .get(id)
        .context("journal operation missing")
}

fn successful_receipts(record: &Value) -> Result<()> {
    ensure!(record["stage"] == "completed");
    ensure!(record["disposition"]["kind"] == "succeeded");
    ensure!(record["ordinary"]["accepted"] == true);
    ensure!(record["lifecycle"]["in_flight"].is_null());
    ensure!(
        record["ordinary"]["actions"]
            .as_array()
            .context("missing instructions")?
            .len()
            == record["lifecycle"]["outcomes"]
                .as_array()
                .context("missing receipts")?
                .len()
    );
    Ok(())
}

fn application_progress(observation: &Value) -> Result<(i64, i64)> {
    let committed = observation["application"]["committedLsn"]
        .as_i64()
        .with_context(|| {
            format!("missing durable LSN in actual application observation: {observation}")
        })?;
    ensure!(
        observation["application"]["appliedLsn"]
            .as_i64()
            .context("missing applied LSN")?
            >= committed,
        "application commitment exceeds actual provider application"
    );
    Ok((
        observation["application"]["committedValue"]
            .as_i64()
            .context("missing durable value")?,
        committed,
    ))
}

async fn wait_for_value<S>(service: &WitnessService<S>, value: i64, lsn: i64) -> Result<()> {
    loop {
        if application_progress(&service.observation()?)? == (value, lsn) {
            return Ok(());
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

/// Separately named from every original positive; all waits use TestCluster's
/// existing scenario budget. There is no test-directed restart or data reset.
pub async fn qualify_retained_secondary<F: WitnessApplication>(
    factory: WitnessFactory<F>,
) -> Result<()> {
    qualify_retained_prefix(factory, false).await
}

pub async fn qualify_unequal_retained_prefix<F: WitnessApplication>(
    factory: WitnessFactory<F>,
) -> Result<()> {
    qualify_retained_prefix(factory, true).await
}

async fn qualify_retained_prefix<F: WitnessApplication>(
    factory: WitnessFactory<F>,
    opened_winner: bool,
) -> Result<()> {
    use kuberic_controller::test_util::TestCluster;
    use kuberic_runtime::protocol::public_operations::{
        ReplicaGoalKind, ReplicaGoalStatus, ReplicaPublicationStage,
    };
    use kuberic_runtime::protocol::types::AccessStatus;

    let mut cluster = TestCluster::builder(factory).start().await?;
    let result: Result<()> = async {
        let topology = cluster.settle().await?;
        let old_primary = topology.primary().clone();
        // Keep a genuinely warm eligible nominee; the separately reopened
        // original member must later consume as a copied Secondary.
        let target = topology.secondaries().max_by_key(|member| member.replica_id())
            .context("secondary missing")?.clone();
        let healthy = topology.secondaries().min_by_key(|member| member.replica_id())
            .context("other secondary missing")?.clone();
        ensure!(target != healthy);
        let writer = cluster.service(&old_primary).await?;
        let failed = cluster.service(&target).await?;
        let healthy_service = cluster.service(&healthy).await?;
        let healthy_root = cluster.application_root(&healthy).await?;
        if let Some(request) = F::initialize_request() {
            F::write_lsn(cluster.probe(writer.request(request)).await?)?;
        }
        F::write_lsn(cluster.probe(writer.request(F::write_request(71))).await?)?;
        let delivery = if opened_winner {
            let first = application_progress(&writer.observation()?)?.1;
            cluster.probe(wait_for_value(&failed, 71, first)).await?;
            cluster.probe(wait_for_value(&healthy_service, 71, first)).await?;
            Some(cluster.hold_native_ingress(&healthy).await?)
        } else {
            None
        };
        let initial_lsn = F::write_lsn(
            cluster.probe(writer.request(F::write_request(79))).await?,
        )?;
        cluster.probe(wait_for_value(&failed, 79, initial_lsn)).await?;
        if !opened_winner {
            cluster.probe(wait_for_value(&healthy_service, 79, initial_lsn)).await?;
        }
        ensure!(F::read_value(cluster.probe(writer.request(F::read_request())).await?)? == 79);
        let root = cluster.application_root(&target).await?;
        let initial_observation = failed.observation()?;
        let failed_generation = failed.context.member_generation().context("missing child generation")?.to_owned();
        let observed = cluster.observe().await?;
        let historical = observed.snapshot(&target).context("historical member missing")?;
        let publication = historical.publication.as_ref().context("historical publication missing")?;
        ensure!(publication.stage == ReplicaPublicationStage::AchievedScope);
        ensure!(publication.role == ReplicaRole::ActiveSecondary);
        ensure!(publication.scope.configuration.previous.is_none());
        let original = publication.scope.configuration.current.clone();
        let admitted = historical.goals.iter()
            .find(|entry| entry.goal.revision == publication.generation)
            .context("historical accepted goal missing")?;
        ensure!(admitted.status == ReplicaGoalStatus::Achieved);
        let history_journal = journal(&root)?;
        successful_receipts(operation(&history_journal, admitted.goal.operation_id.as_str())?)?;
        let records = history_journal["publicV1OperationState"]["operations"]
            .as_object().context("missing historical journal")?;
        for instruction in ["ReplicatorRole", "ApplicationRole"] {
            ensure!(records.values().any(|record| {
                record["intent"]["processSessionId"] == serde_json::to_value(target.process_session_id()).unwrap()
                    && record["intent"]["program"]["AgentGoal"]["desired"]["role"]
                        == json!(ReplicaRole::ActiveSecondary)
                    && record["stage"] == "completed"
                    && record["disposition"]["kind"] == "succeeded"
                    && record["ordinary"]["actions"].as_array().is_some_and(|actions| {
                        actions.iter().enumerate().any(|(index, action)| {
                            action == instruction
                                && record["lifecycle"]["outcomes"].get(index).is_some()
                        })
                    })
            }), "historical {instruction} lacks actual completed callback receipts");
        }
        let routes = cluster.native_route_counts().await;
        cluster.stop(&old_primary).await?;
        if opened_winner {
            ensure!(application_progress(&healthy_service.observation()?)?.1 < initial_lsn,
                "unequal-prefix fixture lost its actual lagging survivor");
        }
        failed.command("fail-worker").await?;
        let fault_observation = cluster.probe(async {
            loop {
                let observation = failed.observation()?;
                if !observation["witness"]["exitNotice"].is_null() {
                    break Ok::<_, anyhow::Error>(observation);
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        }).await?;
        let notice: RetainedRecoveryExitNotice =
            serde_json::from_value(fault_observation["witness"]["exitNotice"].clone())?;
        ensure!(notice.qualification.admitted_operation_id == admitted.goal.operation_id);
        ensure!(notice.qualification.predecessor_process_session_id == *target.process_session_id());
        ensure!(notice.qualification.predecessor_creation_id == *target.replicator_creation_id());
        ensure!(notice.storage == *target.storage_incarnation());
        ensure!(notice.qualification.configuration == original);
        ensure!(notice.process_id != std::process::id());
        ensure!(initial_observation["witness"]["pid"] == notice.process_id);
        ensure!(fault_observation["witness"]["workerFailure"].as_str()
            .is_some_and(|error| error.contains("retained witness worker")));
        let failed_journal = cluster.probe(async {
            loop {
                let state = journal(&root)?;
                let fault = operation(&state, notice.fault_operation_id.as_str())?;
                if fault["stage"] == "completed" {
                    break Ok::<_, anyhow::Error>(state);
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        }).await?;
        let old_fault = operation(&failed_journal, notice.fault_operation_id.as_str())?.clone();
        ensure!(old_fault["intent"]["class"]["kind"] == "transientFault");
        ensure!(old_fault["containment"] == "pending");
        ensure!(old_fault["stage"] == "completed");
        ensure!(old_fault["disposition"]["kind"] == "succeeded",
            "the fault report itself must have its actual successful result");
        ensure!(old_fault["lifecycle"]["outcomes"].as_array().is_some_and(|outcomes| {
            outcomes.iter().any(|outcome| {
                outcome["RecoveryExitRequired"] == serde_json::to_value(&notice).unwrap()
            })
        }), "no actual durable exit notice; report success cannot certify unknown Cleanup");

        // These are the actual existing host/provider lifetime lock files, not
        // locks created by the test. The live child is still answering its exact
        // generation while both independent ownership attempts are refused.
        for path in [
            root.parent().unwrap().join(".kuberic/agent.owner"),
            root.with_file_name(".application.kuberic-owner"),
        ] {
            let file = std::fs::OpenOptions::new().read(true).write(true).open(&path)?;
            ensure!(
                matches!(file.try_lock(), Err(std::fs::TryLockError::WouldBlock)),
                "live failed owner did not exclude overlap at {}", path.display()
            );
        }
        ensure!(failed.observation()?["witness"]["exitNotice"] == serde_json::to_value(&notice)?);
        ensure!(cluster.reaped_member_observations().await.is_empty());
        eprintln!("retained-witness live failed owner: {}", json!({
            "notice": notice, "fault": old_fault,
            "application": fault_observation, "historical": admitted.goal,
        }));
        failed.command("release-exit").await?;

        if opened_winner {
            let (fresh_ref, recovered, configured) = loop {
                cluster.step().await?;
                let observed = cluster.observe().await?;
                let Some((reference, snapshot)) = observed.snapshots().find(|(reference, _)| {
                    reference.identity() == target.identity()
                        && reference.process_session_id() != target.process_session_id()
                }) else { continue };
                let Some(configured) = snapshot.goals.iter().find(|entry| {
                    entry.goal.scope.process_session_id == *reference.process_session_id()
                        && entry.status == ReplicaGoalStatus::Achieved && entry.host_settled
                        && matches!(entry.goal.desired, ReplicaGoalKind::Reconfigure {
                            role: ReplicaRole::Primary, catch_up: None
                        })
                }) else { continue };
                ensure!(snapshot.native_lifecycle.as_ref().is_some_and(|facts| {
                    facts.state == kuberic_runtime::protocol::public_operations::NativeReplicaState::Ready
                        && facts.native_role == ReplicaRole::Primary
                        && facts.application_role == Some(ReplicaRole::Primary)
                        && facts.applied_primary_fence == Some(configured.goal.scope.epoch)
                }));
                ensure!(snapshot.read_status != AccessStatus::Granted && snapshot.write_status != AccessStatus::Granted);
                let reference = reference.clone();
                let goal = configured.goal.clone();
                let service = cluster.service(&reference).await?;
                break (reference, service, goal);
            };
            ensure!(fresh_ref.storage_incarnation() == target.storage_incarnation());
            ensure!(cluster.application_root(&fresh_ref).await? == root);
            let selected_journal = journal(&root)?;
            let selected = operation(&selected_journal, configured.operation_id.as_str())?;
            successful_receipts(selected)?;
            ensure!(selected["ordinary"]["native_input"]["lifecycle"]["state"] == "StandBy");
            ensure!(selected["ordinary"]["native_input"]["promotion"]["progress"]["detail"] == initial_lsn);
            ensure!(selected["ordinary"]["native_input"]["promotion"]["capability"]["detail"] == 0);
            ensure!(selected["ordinary"]["actions"] == json!([
                "ReplicatorRole", "ProgramEpoch", "ApplicationRole", "NativeDataLossDecision",
                "Configuration", "NativeComplete"
            ]));
            let source_native = cluster.native_transport_observation(&fresh_ref).await?;
            ensure!(source_native["production"]["nativeConfiguration"]["current"]["writeQuorum"] == original.write_quorum);
            ensure!(source_native["production"]["nativeConfiguration"]["previous"]["writeQuorum"] == original.write_quorum);
            for kind in ["current", "previous"] {
                ensure!(source_native["production"]["nativeConfiguration"][kind]["members"]
                    .as_array().is_some_and(Vec::is_empty));
            }
            ensure!(operation(&selected_journal, notice.fault_operation_id.as_str())?["supersededBy"].is_null());
            eprintln!("retained-witness unequal Opened configured source: {}", json!({
                "goal": configured, "record": selected, "native": source_native,
                "warm": cluster.sample_replica(&healthy).await?,
                "oldFault": operation(&selected_journal, notice.fault_operation_id.as_str())?,
            }));
            delivery.context("unequal-prefix native delivery hold missing")?.release();
            let mut successor_logged = false;
            loop {
                cluster.step().await?;
                let observed = cluster.observe().await?;
                let Some((reference, snapshot)) = observed.snapshots().find(|(reference, _)| {
                    reference.identity() == healthy.identity()
                        && reference.process_session_id() != healthy.process_session_id()
                }) else { continue };
                if !successor_logged {
                    successor_logged = true;
                    let status = observed.set().status.as_ref().unwrap();
                    let close = status.replica_goals.iter().rev().find(|entry| {
                        matches!(entry.goal.desired, ReplicaGoalKind::Close)
                            && entry.goal.scope.storage == *healthy.storage_incarnation()
                    }).context("native restart lost its actual selected Close")?;
                    eprintln!("retained-witness unequal warm successor: {}", json!({
                        "snapshot": snapshot, "reaps": cluster.reaped_member_observations().await,
                        "close": close, "topology": status.public_v1_topology,
                        "selectedCurrent": configured.scope.configuration.current,
                        "source": observed.snapshot(&fresh_ref),
                    }));
                    ensure!(close.goal.scope.configuration.current == configured.scope.configuration.current);
                    ensure!(close.goal.scope.configuration.previous.as_ref() == Some(&original));
                    ensure!(snapshot.open_mode == OpenMode::Existing && !snapshot.withdrawn);
                }
                let reference = reference.clone();
                let service = cluster.service(&reference).await?;
                if service.observation()?["witness"]["roleEntered"] == true {
                    ensure!(service.observation()?["witness"]["openModes"] == json!([OpenMode::Existing]));
                    ensure!(service.observation()?["witness"]["workerFailure"].is_null());
                    ensure!(application_progress(&service.observation()?)?.0 == 71);
                    ensure!(application_progress(&service.observation()?)?.1 < initial_lsn);
                    let native = cluster.native_transport_observation(&reference).await?;
                    ensure!(native["production"]["retainedInitialized"] != true);
                    ensure!(native["sessions"]["receiver"].is_null());
                    ensure!(native["ackOwners"].as_array().is_some_and(Vec::is_empty));
                    service.command("release-role").await?;
                    break;
                }
            }
            let elected = cluster.settle_members(2).await?;
            ensure!(elected.primary().identity() == target.identity(),
                "lower warm prefix replaced the actual higher Opened winner");
            let follower = elected.secondaries().next().context("rebuilt original follower absent")?;
            ensure!(follower.identity() == healthy.identity());
            ensure!(follower.process_session_id() != healthy.process_session_id(),
                "out-of-range warm owner was not actually recreated");
            ensure!(cluster.application_root(follower).await? == healthy_root);
            ensure!(follower.storage_incarnation() == healthy.storage_incarnation());
            let scope = cluster.sample_replica(elected.primary()).await?.publication
                .context("final unequal-prefix source publication missing")?.scope;
            ensure!(scope.configuration.current.write_quorum == original.write_quorum);
            ensure!(scope.configuration.previous.is_none());
            ensure!(F::read_value(cluster.probe(recovered.request(F::read_request())).await?)? == 79);
            let subsequent_lsn = F::write_lsn(cluster.probe(recovered.request(F::write_request(97))).await?)?;
            ensure!(subsequent_lsn > initial_lsn);
            let follower_service = cluster.service(follower).await?;
            cluster.probe(wait_for_value(&follower_service, 97, subsequent_lsn)).await?;
            ensure!(writer.observation().is_err() && failed.observation().is_err());
            ensure!(cluster.reaped_member_observations().await.iter().any(|reap|
                reap["pid"] == notice.process_id && reap["generation"] == failed_generation));
            let final_journal = journal(&root)?;
            ensure!(final_journal["reportedFault"] == json!(FaultType::Transient));
            let retained_fault = operation(&final_journal, notice.fault_operation_id.as_str())?;
            for field in ["lifecycle", "disposition", "containment", "stage"] {
                ensure!(retained_fault[field] == old_fault[field], "recovery rewrote old failure {field}");
            }
            let source_snapshot = cluster.sample_replica(elected.primary()).await?;
            let target_snapshot = cluster.sample_replica(follower).await?;
            let source_final = source_snapshot.goals.iter().find(|entry| {
                entry.goal.revision == source_snapshot.publication.as_ref().unwrap().generation
            }).context("source final receipt absent")?;
            let target_final = target_snapshot.goals.iter().find(|entry| {
                entry.goal.revision == target_snapshot.publication.as_ref().unwrap().generation
            }).context("target final receipt absent")?;
            let source_record = operation(&final_journal, source_final.goal.operation_id.as_str())?;
            successful_receipts(source_record)?;
            ensure!(source_record["ordinary"]["host_settled"] == true);
            ensure!(source_record["ordinary"]["actions"].as_array().is_some_and(|actions|
                actions.iter().any(|action| action == "CurrentConfiguration")));
            ensure!(retained_fault["supersededBy"] == json!(source_final.goal.operation_id));
            let target_journal = journal(&healthy_root)?;
            ensure!(target_journal["reportedFault"].is_null(),
                "native rebuild fabricated an application fault");
            let target_record = operation(&target_journal, target_final.goal.operation_id.as_str())?;
            successful_receipts(target_record)?;
            ensure!(target_record["ordinary"]["host_settled"] == true);
            let native_close = target_journal["publicV1OperationState"]["operations"]
                .as_object().unwrap().values().find(|record|
                    record["intent"]["processSessionId"] == json!(healthy.process_session_id())
                        && record["intent"]["program"]["AgentGoal"]["desired"]["kind"] == "close")
                .context("actual native restart Close absent")?;
            ensure!(native_close["stage"] == "completed" && native_close["disposition"]["kind"] == "succeeded");
            ensure!(native_close["intent"]["class"]["kind"] == "close");
            ensure!(native_close["containment"] == "pending");
            let reason = &native_close["lifecycle"]["outcomes"][1]["RecoveryExitRequired"]["nativeRebuild"];
            ensure!(reason["sourceLast"] == initial_lsn && reason["sourceFirst"] == 0);
            ensure!(reason["targetLast"].as_i64().is_some_and(|last| last < initial_lsn));
            ensure!(native_close["supersededBy"] == json!(target_final.goal.operation_id));
            let status = cluster.observe().await?;
            ensure!(!status.replicas().any(|reference|
                reference.instance_id() == old_primary.instance_id()),
                "original Primary was restarted");
            let old_close = status.set().status.as_ref().unwrap().replica_goals.iter()
                .find(|entry| json!(entry.goal.operation_id) == native_close["intent"]["operationId"])
                .context("old native Close controller result missing")?;
            if native_close["ordinary"]["accepted"] != true {
                ensure!(matches!(old_close.outcome.as_ref().map(|outcome| &outcome.status),
                    Some(ReplicaGoalStatus::Uncertain(_))), "lost old result was rewritten as success");
            }
            ensure!(source_snapshot.native_lifecycle.as_ref().unwrap().deactivation
                == target_snapshot.native_lifecycle.as_ref().unwrap().deactivation);
            let source_native = cluster.native_transport_observation(elected.primary()).await?;
            let target_native = cluster.native_transport_observation(follower).await?;
            ensure!(source_native["production"]["nativeConfiguration"]["previous"].is_null());
            ensure!(source_native["production"]["nativeConfiguration"]["current"]["writeQuorum"] == original.write_quorum);
            ensure!(target_native["ackOwners"].as_array().is_some_and(|owners| !owners.is_empty()));
            let after_routes = cluster.native_route_counts().await;
            for kind in ["Build", "AfterContext", "BuildResult"] {
                ensure!(after_routes.get(kind).copied().unwrap_or(0) > routes.get(kind).copied().unwrap_or(0));
            }
            eprintln!("retained-witness unequal original quorum acknowledged suffix: {}", json!({
                "sourceFinal": source_final.goal, "sourceRecord": source_record,
                "targetFinal": target_final.goal, "targetRecord": target_record,
                "nativeClose": native_close, "oldControllerClose": old_close,
                "oldFault": retained_fault, "sourceNative": source_native, "targetNative": target_native,
                "initialLsn": initial_lsn, "subsequentLsn": subsequent_lsn,
                "sourceRoot": root, "targetRoot": healthy_root,
                "reaps": cluster.reaped_member_observations().await,
            }));
            return Ok(());
        }

        let (fresh_ref, restored, restore_goal, pending) = loop {
            cluster.drive(async {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                Ok::<_, anyhow::Error>(())
            }).await?;
            let observation = cluster.observe().await?;
            let Some((reference, _)) = observation.snapshots().find(|(reference, _)| {
                reference.instance_id() == target.instance_id()
                    && reference.process_session_id() != target.process_session_id()
            }) else { continue };
            let reference = reference.clone();
            let service = cluster.service(&reference).await?;
            if !service.observation()?["witness"]["roleEntered"].as_bool().unwrap_or(false) {
                continue;
            }
            // The service was awaited after this aggregate observation; its
            // selected recipe can have advanced in between. Read the exact
            // still-held actor now rather than attributing its older snapshot.
            let snapshot = cluster.sample_replica(&reference).await?;
            let goal = snapshot.goals.iter().find(|entry| {
                entry.goal.scope.process_session_id == *reference.process_session_id()
                    && matches!(entry.goal.desired, ReplicaGoalKind::Prepare {
                        role: ReplicaRole::IdleSecondary, rebuild: Some(_)
                    })
            }).context("fresh process did not select canonical original-member Idle")?.goal.clone();
            break (reference, service, goal, snapshot);
        };
        ensure!(fresh_ref.storage_incarnation() == target.storage_incarnation());
        ensure!(fresh_ref.replicator_creation_id() != target.replicator_creation_id());
        ensure!(cluster.application_root(&fresh_ref).await? == root);
        let reaps = cluster.reaped_member_observations().await;
        let exact_reap = reaps.iter().find(|reap| reap["pid"] == notice.process_id)
            .context("no exact owned-child reap")?;
        ensure!(exact_reap["success"] == false);
        ensure!(exact_reap["code"] == 1, "helper did not return failure through its executable");
        ensure!(exact_reap["generation"] == failed_generation);
        ensure!(failed.observation().is_err(), "old data proxy reached the successor");
        ensure!(writer.observation().is_err(), "old Primary was restarted");
        let pending_publication = pending.publication.as_ref().context("pending publication missing")?;
        ensure!(pending_publication.stage == ReplicaPublicationStage::DiscoveryPending);
        ensure!(pending_publication.role == ReplicaRole::None);
        ensure!(pending.read_status != AccessStatus::Granted && pending.write_status != AccessStatus::Granted);
        let sample = cluster.sample_replica(&fresh_ref).await?;
        ensure!(sample.publication == pending.publication);
        ensure!(sample.progress.value().is_none() && sample.capability.value().is_none());
        ensure!(F::read_value(restored.request(F::read_request()).await.unwrap_or(Value::Null)).is_err());
        ensure!(F::write_lsn(restored.request(F::write_request(83)).await.unwrap_or(Value::Null)).is_err());
        let native = cluster.native_transport_observation(&fresh_ref).await?;
        ensure!(native["production"]["retainedInitialized"] != true);
        ensure!(native["sessions"]["receiver"].is_null());
        ensure!(native["sessions"]["entries"].as_array().is_some_and(Vec::is_empty));
        ensure!(native["ackOwners"].as_array().is_some_and(Vec::is_empty));
        let pending_journal = journal(&root)?;
        let selected = operation(&pending_journal, restore_goal.operation_id.as_str())?;
        ensure!(selected["lifecycle"]["in_flight"] == "ApplicationRole");
        let ReplicaGoalKind::Prepare { rebuild: Some(rebuild), .. } = &restore_goal.desired else {
            anyhow::bail!("missing canonical original-member provenance");
        };
        let inspection = operation(&pending_journal, rebuild.inspection.operation_id.as_str())?;
        successful_receipts(inspection)?;
        ensure!(inspection["ordinary"]["host_settled"] == true);
        let status = inspection["lifecycle"]["outcomes"].as_array()
            .context("missing actual inspection outcomes")?.iter()
            .find_map(|outcome| outcome.get("NativeStatus"))
            .context("missing actual native StandBy inspection")?;
        ensure!(status["input"]["state"] == "StandBy");
        ensure!(status["input"]["nativeRole"] == "none");
        ensure!(status["epoch"] == "StandBy");
        ensure!(status["progress"]["detail"] == initial_lsn);
        ensure!(status["capability"]["detail"] == 0);
        ensure!(!status["input"]["deactivation"].is_null());
        let opened = pending_journal["publicV1OperationState"]["operations"]
            .as_object().context("missing actual Open journal")?.values()
            .find(|record| {
                record["intent"]["processSessionId"] == json!(fresh_ref.process_session_id())
                    && record["lifecycle"]["outcomes"].as_array().is_some_and(|outcomes|
                        outcomes.iter().any(|outcome| outcome.get("BuiltinNativeEndpoint").is_some()))
            }).context("missing actual fresh native Open")?;
        successful_receipts(opened)?;
        let endpoint = opened["lifecycle"]["outcomes"].as_array().unwrap().iter()
            .find_map(|outcome| outcome.get("BuiltinNativeEndpoint")).unwrap();
        ensure!(endpoint["process_id"].as_u64().is_some_and(|pid| pid > 0 && pid != u64::from(notice.process_id)));
        ensure!(endpoint["process_lifetime"] != json!(notice.process_lifetime));
        ensure!(endpoint["storage_owner"] == serde_json::to_value(fresh_ref.process_session_id())?);
        ensure!(selected["ordinary"]["retained"].is_null());
        ensure!(!selected["lifecycle"]["outcomes"].as_array().unwrap().iter().any(|outcome|
            outcome.get("RetainedNativeInitialized").is_some()));
        ensure!(operation(&pending_journal, notice.fault_operation_id.as_str())?["supersededBy"].is_null());
        let configured_source = cluster.observe().await?;
        let (source_ref, source_snapshot) = configured_source.snapshots().find(|(_, snapshot)|
            snapshot.publication.as_ref() == Some(&rebuild.source))
            .context("actual configured source disappeared before Idle")?;
        ensure!(source_snapshot.read_status != AccessStatus::Granted && source_snapshot.write_status != AccessStatus::Granted);
        ensure!(rebuild.source.scope.configuration.previous.as_ref() == Some(&original));
        ensure!(rebuild.source.scope.configuration.current.write_quorum == original.write_quorum);
        ensure!(source_snapshot.native_lifecycle.as_ref().is_some_and(|facts|
            facts.state == kuberic_runtime::protocol::public_operations::NativeReplicaState::Ready
                && facts.native_role == ReplicaRole::Primary && facts.application_role == Some(ReplicaRole::Primary)));
        let source_native = cluster.native_transport_observation(source_ref).await?;
        for kind in ["current", "previous"] {
            ensure!(source_native["production"]["nativeConfiguration"][kind]["writeQuorum"] == original.write_quorum);
            ensure!(source_native["production"]["nativeConfiguration"][kind]["members"].as_array().is_some_and(Vec::is_empty));
        }
        ensure!(restored.observation()?["witness"]["openModes"] == json!([OpenMode::Existing]));
        ensure!(application_progress(&restored.observation()?)? == (79, initial_lsn));
        if let Some(image) = initial_observation["application"].get("imageSha256") {
            ensure!(restored.observation()?["application"]["imageSha256"] == *image,
                "retained SQL image changed before application restoration");
        }
        eprintln!("retained-witness pending new owner: {}", json!({
            "reap": exact_reap, "record": selected, "sample": sample, "native": native,
        }));

        restored.command("release-role").await?;
        // No reconcile until the actual callbacks have returned and their local
        // receipts/publication have been observed. Adoption is a later owner.
        cluster.probe(async {
            loop {
                let state = journal(&root)?;
                let record = operation(&state, restore_goal.operation_id.as_str())?;
                if record["stage"] == "completed" {
                    ensure!(record["disposition"]["kind"] == "succeeded");
                    break Ok::<_, anyhow::Error>(());
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        }).await?;
        let achieved = cluster.sample_replica(&fresh_ref).await?;
        ensure!(achieved.publication.as_ref().is_some_and(|publication| {
            publication.stage == ReplicaPublicationStage::AchievedScope
                && publication.scope == restore_goal.scope
                && publication.role == ReplicaRole::IdleSecondary
        }));
        ensure!(achieved.read_status != AccessStatus::Granted && achieved.write_status != AccessStatus::Granted);
        let before_adoption = cluster.observe().await?;
        ensure!(before_adoption.set().status.as_ref().unwrap().public_v1_topology.as_ref().unwrap()
            .sessions.iter().any(|session| session.process_session_id == *target.process_session_id()));
        loop {
            cluster.step().await?;
            let state = journal(&root)?;
            let record = operation(&state, restore_goal.operation_id.as_str())?;
            if record["ordinary"]["accepted"] == true
                && record["ordinary"]["host_settled"] == true
            {
                break;
            }
        }
        let complete_journal = journal(&root)?;
        let complete = operation(&complete_journal, restore_goal.operation_id.as_str())?;
        successful_receipts(complete)?;
        ensure!(complete["ordinary"]["actions"] == json!([
            "ReplicatorRole", "ApplicationRole", "NativeComplete",
        ]));
        let outcomes = &complete["lifecycle"]["outcomes"];
        ensure!(outcomes[0] == "Done");
        ensure!(outcomes[1].get("ApplicationRole").is_some());
        ensure!(outcomes[2]["NativeLifecycle"]["state"] == "InBuild");
        ensure!(outcomes[2]["NativeLifecycle"]["nativeRole"] == "idleSecondary");
        eprintln!("retained-witness achieved sampled adoption: {}", json!({
            "goal": restore_goal, "record": complete, "sample": achieved,
        }));
        let elected = cluster.settle_members(2).await?;
        ensure!(elected.primary().identity() == healthy.identity(), "restored member was promoted and hid its consumer");
        let secondary = elected.secondaries().next().context("restored secondary absent")?;
        ensure!(secondary.identity() == target.identity());
        ensure!(secondary.process_session_id() == fresh_ref.process_session_id());
        let observation = cluster.observe().await?;
        ensure!(!observation.replicas().any(|reference| {
            reference.instance_id() == old_primary.instance_id()
        }), "the original Primary acquired a successor process during election");
        let elected_scope = &observation.snapshot(secondary).context("secondary snapshot missing")?
            .publication.as_ref().context("secondary publication missing")?.scope;
        ensure!(elected_scope.configuration.current.write_quorum == original.write_quorum);
        ensure!(elected_scope.configuration.current.epoch > original.epoch);
        ensure!(elected_scope.configuration.current.members.iter().all(|member| {
            original.members.iter().any(|original| original.identity == member.identity)
                && member.identity != *old_primary.identity()
        }));
        let current_writer = cluster.service(elected.primary()).await?;
        ensure!(F::read_value(cluster.probe(current_writer.request(F::read_request())).await?)? == 79);
        let subsequent_lsn = F::write_lsn(
            cluster.probe(current_writer.request(F::write_request(97))).await?,
        )?;
        ensure!(subsequent_lsn > initial_lsn);
        cluster.probe(wait_for_value(&restored, 97, subsequent_lsn)).await?;
        ensure!(restored.observation()?["witness"]["role"] == json!(ReplicaRole::ActiveSecondary));
        ensure!(F::read_value(cluster.probe(current_writer.request(F::read_request())).await?)? == 97);
        ensure!(writer.observation().is_err() && failed.observation().is_err());
        let final_journal = journal(&root)?;
        ensure!(final_journal["reportedFault"] == json!(FaultType::Transient));
        let retained_fault = operation(&final_journal, notice.fault_operation_id.as_str())?;
        for field in ["lifecycle", "disposition", "containment", "stage"] {
            ensure!(retained_fault[field] == old_fault[field], "recovery rewrote old failure {field}");
        }
        let final_snapshot = cluster.sample_replica(secondary).await?;
        let final_goal = final_snapshot.goals.iter().find(|entry|
            entry.goal.revision == final_snapshot.publication.as_ref().unwrap().generation)
            .context("final copied-member goal missing")?;
        successful_receipts(operation(&final_journal, final_goal.goal.operation_id.as_str())?)?;
        ensure!(retained_fault["supersededBy"] == json!(final_goal.goal.operation_id));
        let source_journal = journal(&cluster.application_root(elected.primary()).await?)?;
        let source_snapshot = cluster.sample_replica(elected.primary()).await?;
        let source_final = source_snapshot.goals.iter().find(|entry|
            entry.goal.revision == source_snapshot.publication.as_ref().unwrap().generation)
            .context("final copy source goal missing")?;
        let source_record = operation(&source_journal, source_final.goal.operation_id.as_str())?;
        successful_receipts(source_record)?;
        ensure!(source_record["ordinary"]["actions"].as_array().is_some_and(|actions|
            actions.iter().any(|action| action == "CurrentConfiguration")));
        ensure!(source_snapshot.native_lifecycle.as_ref().unwrap().deactivation
            == final_snapshot.native_lifecycle.as_ref().unwrap().deactivation);
        let final_native = cluster.native_transport_observation(elected.primary()).await?;
        ensure!(final_native["production"]["nativeConfiguration"]["previous"].is_null());
        ensure!(final_native["production"]["nativeConfiguration"]["current"]["writeQuorum"] == original.write_quorum);
        let after_routes = cluster.native_route_counts().await;
        for kind in ["Build", "AfterContext", "BuildResult"] {
            ensure!(after_routes.get(kind).copied().unwrap_or(0) > routes.get(kind).copied().unwrap_or(0),
                "original-member recovery lacks actual copy boundary: {kind}");
        }
        ensure!(initial_observation["witness"]["openModes"] != json!([OpenMode::Existing]));
        eprintln!("retained-witness secondary acknowledged suffix: {}", json!({
            "initialLsn": initial_lsn, "subsequentLsn": subsequent_lsn,
            "application": restored.observation()?, "native": cluster.native_transport_observation(secondary).await?,
            "oldFault": retained_fault, "routesBefore": routes, "routesAfter": after_routes,
            "sourceRoot": cluster.application_root(elected.primary()).await?, "targetRoot": root,
        }));
        Ok(())
    }.await;
    cluster.finish(result).await?;
    Ok(())
}
