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
        if self.restoring.load(Ordering::Acquire)
            && *self.role.lock().unwrap() == ReplicaRole::None
            && role == ReplicaRole::ActiveSecondary
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
    use kuberic_controller::test_util::TestCluster;
    use kuberic_runtime::protocol::public_operations::{
        ReplicaGoalKind, ReplicaGoalStatus, ReplicaPublicationStage,
    };
    use kuberic_runtime::protocol::types::AccessStatus;

    let mut cluster = TestCluster::builder(factory).start().await?;
    let result: Result<()> = async {
        let topology = cluster.settle().await?;
        let old_primary = topology.primary().clone();
        // Election chooses the smallest equally-current survivor. Fault the
        // other one so promotion cannot conceal a broken restored consumer.
        let target = topology.secondaries().max_by_key(|member| member.replica_id())
            .context("secondary missing")?.clone();
        let healthy = topology.secondaries().min_by_key(|member| member.replica_id())
            .context("other secondary missing")?.clone();
        ensure!(target != healthy);
        let writer = cluster.service(&old_primary).await?;
        let failed = cluster.service(&target).await?;
        let healthy_service = cluster.service(&healthy).await?;
        if let Some(request) = F::initialize_request() {
            F::write_lsn(cluster.probe(writer.request(request)).await?)?;
        }
        F::write_lsn(cluster.probe(writer.request(F::write_request(71))).await?)?;
        let initial_lsn = F::write_lsn(
            cluster.probe(writer.request(F::write_request(79))).await?,
        )?;
        cluster.probe(wait_for_value(&failed, 79, initial_lsn)).await?;
        cluster.probe(wait_for_value(&healthy_service, 79, initial_lsn)).await?;
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
                    && matches!(entry.goal.desired, ReplicaGoalKind::RestoreRetainedMember { .. })
            }).context("fresh process did not select canonical restoration")?.goal.clone();
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
        ensure!(native["production"]["retainedInitialized"] == true);
        ensure!(native["sessions"]["receiver"].is_null());
        ensure!(native["sessions"]["entries"].as_array().is_some_and(Vec::is_empty));
        ensure!(native["ackOwners"].as_array().is_some_and(Vec::is_empty));
        let pending_journal = journal(&root)?;
        let selected = operation(&pending_journal, restore_goal.operation_id.as_str())?;
        ensure!(selected["lifecycle"]["in_flight"] == "ApplicationRole");
        ensure!(selected["lifecycle"]["outcomes"].as_array().is_some_and(|outcomes| {
            outcomes.iter().any(|outcome| {
                outcome["RetainedNativeInitialized"] == serde_json::to_value(&restore_goal.scope).unwrap()
            })
        }));
        let receipt = &selected["ordinary"]["retained"];
        ensure!(receipt["predecessor_process_id"] == notice.process_id);
        ensure!(receipt["predecessor_process_lifetime"] == notice.process_lifetime);
        ensure!(receipt["owner_process_id"].as_u64().is_some_and(|pid| pid > 0 && pid != u64::from(notice.process_id)));
        ensure!(receipt["owner_process_lifetime"] != receipt["predecessor_process_lifetime"]);
        ensure!(receipt["successor_process"] == serde_json::to_value(fresh_ref.process_session_id())?);
        ensure!(receipt["successor_creation"] == serde_json::to_value(fresh_ref.replicator_creation_id())?);
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
                && publication.role == ReplicaRole::ActiveSecondary
        }));
        ensure!(achieved.progress.value() == Some(initial_lsn));
        ensure!(achieved.capability.value().is_some_and(|value| value >= 0));
        let before_adoption = cluster.observe().await?;
        ensure!(before_adoption.set().status.as_ref().unwrap().public_v1_topology.as_ref().unwrap()
            .sessions.iter().any(|session| session.process_session_id == *target.process_session_id()));
        loop {
            cluster.step().await?;
            let observation = cluster.observe().await?;
            let topology = observation.set().status.as_ref().unwrap().public_v1_topology.as_ref().unwrap();
            if topology.sessions.iter().any(|session| {
                session.identity == *fresh_ref.identity()
                    && session.process_session_id == *fresh_ref.process_session_id()
                    && session.replicator_creation_id == *fresh_ref.replicator_creation_id()
            }) {
                ensure!(topology.configuration().map_err(anyhow::Error::msg)? == original);
                break;
            }
        }
        let complete_journal = journal(&root)?;
        let complete = operation(&complete_journal, restore_goal.operation_id.as_str())?;
        successful_receipts(complete)?;
        ensure!(complete["ordinary"]["actions"] == json!([
            "ApplicationOpen", "ReplicatorOpen", "ReplicatorRole", "ApplicationRole", "ProgramEpoch", "Configuration",
        ]));
        let outcomes = &complete["lifecycle"]["outcomes"];
        ensure!(outcomes[0] == "Done");
        ensure!(outcomes[1]["BuiltinNativeEndpoint"]["process_id"] == receipt["owner_process_id"]);
        ensure!(outcomes[1]["BuiltinNativeEndpoint"]["process_lifetime"] == receipt["owner_process_lifetime"]);
        ensure!(outcomes[1]["BuiltinNativeEndpoint"]["storage_owner"]
            == serde_json::to_value(fresh_ref.process_session_id())?);
        ensure!(outcomes[2]["RetainedNativeInitialized"] == serde_json::to_value(&restore_goal.scope)?);
        ensure!(outcomes[3].get("ApplicationRole").is_some());
        ensure!(outcomes[4] == "Done" && outcomes[5] == "Done");
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
        let retained_fault = operation(&final_journal, notice.fault_operation_id.as_str())?;
        for field in ["lifecycle", "disposition", "containment", "stage"] {
            ensure!(retained_fault[field] == old_fault[field], "recovery rewrote old failure {field}");
        }
        let after_routes = cluster.native_route_counts().await;
        for kind in ["Build", "AfterContext", "BuildResult"] {
            ensure!(after_routes.get(kind) == routes.get(kind), "restoration used source copy: {kind}");
        }
        ensure!(initial_observation["witness"]["openModes"] != json!([OpenMode::Existing]));
        eprintln!("retained-witness secondary acknowledged suffix: {}", json!({
            "initialLsn": initial_lsn, "subsequentLsn": subsequent_lsn,
            "application": restored.observation()?, "native": cluster.native_transport_observation(secondary).await?,
            "oldFault": retained_fault, "routesBefore": routes, "routesAfter": after_routes,
        }));
        Ok(())
    }.await;
    cluster.finish(result).await?;
    Ok(())
}
