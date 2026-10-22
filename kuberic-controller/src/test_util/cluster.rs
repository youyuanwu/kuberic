use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use crate::cluster_api::{AgentApi, GrpcAgentApi, KubeClusterApi};
use crate::crd::{CONTROL_ADDRESS_ANNOTATION, KubericSet, KubericSetSpec, KubericSetStatus};
use crate::reconciler::Reconciler;
use anyhow::{Context, Result, anyhow, bail, ensure};
use k8s_openapi::api::core::v1::{PersistentVolumeClaim, Pod, PodStatus, Secret};
use kube::{Api, ResourceExt, api::PostParams};
use kuberic_runtime::host::{KubernetesDnsResolver, ReplicaHost, RunningReplica};
use kuberic_runtime::protocol::public_operations::{
    ProcessSessionRenewalRequired, PublicOperationIntent, PublicOperationProgram,
    PublicV1AcceptedOperationCompletion, PublicV1AgentReport, PublicV1OperationCompletion,
    PublicV1OperationResult,
};
use kuberic_runtime::protocol::types::{AccessStatus, ReplicaRole};
use tokio::sync::{Mutex, watch};
use tokio::task::JoinHandle;
use tokio::time::Instant;

use super::{
    application::{ApplicationFactory, ReplicaContext},
    kube::{ApiServer as TestApi, Kind},
    pod,
    transport::Network,
};

const NAMESPACE: &str = "in-process";
const NAME: &str = "sample";
const TIMEOUT: Duration = Duration::from_secs(150);

pub(super) fn storage_parent() -> Result<PathBuf> {
    let current = std::env::current_dir()?;
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .unwrap_or_else(|| {
            // Cargo runs unit tests in each package directory. Keep retained
            // failures under the workspace target, not untracked package directories.
            current
                .ancestors()
                .find(|path| path.join("Cargo.toml").is_file() && path.join("Cargo.lock").is_file())
                .unwrap_or(&current)
                .join("target")
        });
    Ok(target.join("kuberic-test-storage"))
}

#[derive(Clone)]
pub(super) struct ConfigurationEvidence {
    pub operations: BTreeMap<String, (PublicOperationIntent, PublicV1OperationCompletion)>,
    renewals: Vec<ProcessSessionRenewalRequired>,
}

struct Hosted<F: ApplicationFactory> {
    service: Arc<F::Service>,
    factory: Arc<F>,
    startup_shutdown: watch::Sender<bool>,
    startup: Option<JoinHandle<kuberic_runtime::host::Result<Option<RunningReplica>>>>,
    running: Option<RunningReplica>,
    data_root: PathBuf,
    _route: kuberic_runtime::replicator::TestingTcpRoute,
    network: Network,
    uid: String,
}

impl<F: ApplicationFactory> Hosted<F> {
    async fn stop(mut self) -> Result<()> {
        self.startup_shutdown.send_replace(true);
        let mut errors = Vec::new();
        if let Some(startup) = self.startup.take() {
            match startup.await {
                Ok(Ok(running)) => self.running = running,
                Ok(Err(error)) => errors.push(error.into()),
                Err(error) => errors.push(error.into()),
            }
        }
        if let Some(mut running) = self.running.take() {
            running.shutdown();
            if let Err(error) = running.wait().await {
                errors.push(error.into());
            }
        }
        if let Err(error) = self.factory.shutdown_and_drain(self.service.clone()).await {
            errors.push(anyhow::Error::new(error));
        }
        if let Err(error) = self.network.forget(&self.uid).await {
            errors.push(error);
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(super::TestError::combined(
                super::ErrorKind::Cleanup,
                "host cleanup failed",
                errors,
            )
            .into())
        }
    }
}

impl<F: ApplicationFactory> Drop for Hosted<F> {
    fn drop(&mut self) {
        self.startup_shutdown.send_replace(true);
        if let Some(running) = &self.running {
            running.shutdown();
        }
    }
}

struct SupervisorState<F: ApplicationFactory> {
    hosts: BTreeMap<String, Hosted<F>>,
    stopped: BTreeSet<String>,
    held_deletions: Arc<std::sync::Mutex<BTreeSet<String>>>,
    drains: Vec<JoinHandle<Result<()>>>,
}

impl<F: ApplicationFactory> Default for SupervisorState<F> {
    fn default() -> Self {
        Self {
            hosts: BTreeMap::new(),
            stopped: BTreeSet::new(),
            held_deletions: Arc::default(),
            drains: Vec::new(),
        }
    }
}

struct ControllerCall {
    future: Pin<Box<dyn Future<Output = crate::Result<crate::reconciler::ReconcileAction>> + Send>>,
    changed: Pin<Box<tokio::sync::futures::OwnedNotified>>,
}

type RequeueFuture = Pin<Box<dyn Future<Output = ()> + Send>>;

pub(super) struct Harness<F: ApplicationFactory> {
    pub api: TestApi,
    supervisor: Arc<Mutex<SupervisorState<F>>>,
    supervisor_shutdown: watch::Sender<bool>,
    supervisor_task: Option<JoinHandle<Result<()>>>,
    pub reconciler: Arc<Reconciler>,
    root: Option<tempfile::TempDir>,
    deadline: Instant,
    pub reconcile_errors: usize,
    last_error: Option<String>,
    fatal_failure: Option<super::error::RetainedFailure>,
    cleaned: bool,
    network: Network,
    renewals: std::sync::Mutex<BTreeMap<String, ProcessSessionRenewalRequired>>,
    last_reports: std::sync::Mutex<BTreeMap<String, PublicV1AgentReport>>,
    namespace: String,
    name: String,
    teardown_timeout: Duration,
    // Exclusive get_mut polling preserves cluster Sync without requiring Sync futures.
    controller_call: Mutex<Option<ControllerCall>>,
    requeue: Mutex<Option<RequeueFuture>>,
}

impl<F: ApplicationFactory> Harness<F> {
    pub(super) async fn new(
        factory: F,
        namespace: String,
        name: String,
        image: String,
        timeout: Duration,
        teardown_timeout: Duration,
        initial_replicas: u32,
    ) -> Result<Self> {
        let api = TestApi::new(&namespace);
        let reconciler = Self::reconciler(&api, &namespace)?;
        let storage_parent = storage_parent()?;
        std::fs::create_dir_all(&storage_parent)?;
        let root = tempfile::Builder::new()
            .prefix("scenario-")
            .tempdir_in(storage_parent)?;
        let supervisor = Arc::new(Mutex::new(SupervisorState::default()));
        let set = KubericSet::new(
            &name,
            KubericSetSpec {
                replicas: initial_replicas,
                image: image.clone(),
                failover_delay_seconds: 1,
                switchover: None,
            },
        );
        Api::<KubericSet>::namespaced(api.client(), &namespace)
            .create(&PostParams::default(), &set)
            .await?;
        let (supervisor_shutdown, mut shutdown) = watch::channel(false);
        let task_api = api.clone();
        let task_state = supervisor.clone();
        let task_root = root.path().to_owned();
        let network = Network::default();
        let task_network = network.clone();
        let task_namespace = namespace.clone();
        let factory = Arc::new(factory);
        let deadline = Instant::now() + timeout;
        let supervisor_task = tokio::spawn(async move {
            let result = loop {
                if *shutdown.borrow() || shutdown.has_changed().is_err() {
                    break Ok(());
                }
                if let Err(error) = supervise(
                    &task_api,
                    &task_state,
                    &task_root,
                    &task_network,
                    &factory,
                    &task_namespace,
                    &image,
                    deadline,
                )
                .await
                {
                    break Err(error);
                }
                tokio::select! {
                    _ = shutdown.changed() => {}
                    _ = tokio::time::sleep(Duration::from_millis(5)) => {}
                }
            };
            task_api.changed().notify_waiters();
            result
        });
        Ok(Self {
            api,
            supervisor,
            supervisor_shutdown,
            supervisor_task: Some(supervisor_task),
            reconciler,
            root: Some(root),
            deadline,
            reconcile_errors: 0,
            last_error: None,
            fatal_failure: None,
            cleaned: false,
            network,
            renewals: std::sync::Mutex::new(BTreeMap::new()),
            last_reports: std::sync::Mutex::new(BTreeMap::new()),
            namespace,
            name,
            teardown_timeout,
            controller_call: Mutex::new(None),
            requeue: Mutex::new(None),
        })
    }

    fn reconciler(api: &TestApi, namespace: &str) -> Result<Arc<Reconciler>> {
        let agents = Arc::new(GrpcAgentApi::new(pod::RPC_DEADLINE));
        let cluster = KubeClusterApi::new(api.client(), agents, "local-test-token", namespace)?;
        Ok(Arc::new(Reconciler::new(
            Arc::new(cluster),
            crate::production_evaluation_config(1, 1, 1),
        )))
    }

    pub fn recreate_controller(&mut self) -> Result<()> {
        self.cancel_controller_call();
        self.reconciler = Self::reconciler(&self.api, &self.namespace)?;
        Ok(())
    }

    fn cancel_controller_call(&mut self) {
        *self.controller_call.get_mut() = None;
        *self.requeue.get_mut() = None;
    }

    pub async fn set(&self) -> Result<KubericSet> {
        let set = Api::<KubericSet>::namespaced(self.api.client(), &self.namespace)
            .get(&self.name)
            .await?;
        if let Some(renewal) = set
            .status
            .as_ref()
            .and_then(|s| s.public_v1_session_renewal.as_ref())
        {
            let required = renewal.renewal().map_err(anyhow::Error::msg)?.required;
            self.renewals
                .lock()
                .unwrap()
                .insert(required.operation_id.to_string(), required);
        }
        Ok(set)
    }

    fn retain_failure(&mut self, source: anyhow::Error) -> anyhow::Error {
        let failure = super::error::RetainedFailure::new(source);
        self.fatal_failure = Some(failure.clone());
        failure.into()
    }

    async fn check(&mut self) -> Result<()> {
        if let Some(failure) = &self.fatal_failure {
            return Err(failure.clone().into());
        }
        self.api.check()?;
        tokio::time::timeout_at(self.deadline, self.network.check()).await??;
        let mut state = self.supervisor.lock().await;
        for index in (0..state.drains.len()).rev() {
            if state.drains[index].is_finished() {
                let result = match state.drains.swap_remove(index).await {
                    Ok(result) => result,
                    Err(error) => Err(error.into()),
                };
                if let Err(error) = result {
                    drop(state);
                    return Err(self.retain_failure(error));
                }
            }
        }
        drop(state);
        if self
            .supervisor_task
            .as_ref()
            .is_some_and(JoinHandle::is_finished)
        {
            let task = self.supervisor_task.take().unwrap();
            let failure = match task.await {
                Ok(Err(error)) => error.context("supervisor failed"),
                Err(error) => anyhow::Error::new(error).context("supervisor panicked"),
                Ok(Ok(())) => anyhow!("supervisor stopped unexpectedly"),
            };
            return Err(self.retain_failure(failure));
        }
        if Instant::now() >= self.deadline {
            return Err(super::TestError::new(
                super::ErrorKind::Deadline,
                "shared scenario deadline elapsed; operations are not rolled back",
            )
            .into());
        }
        Ok(())
    }

    pub async fn step(&mut self) -> Result<()> {
        self.check().await?;
        if let Some(requeue) = self.requeue.get_mut().as_mut() {
            requeue.await;
            *self.requeue.get_mut() = None;
            return self.check().await;
        }
        if self.controller_call.get_mut().is_none() {
            let mut changed = Box::pin(self.api.changed().notified_owned());
            changed.as_mut().enable();
            let reconciler = self.reconciler.clone();
            let namespace = self.namespace.clone();
            let name = self.name.clone();
            *self.controller_call.get_mut() = Some(ControllerCall {
                future: Box::pin(async move { reconciler.reconcile(&namespace, &name).await }),
                changed,
            });
        }
        self.finish_controller_call().await?;
        self.requeue.get_mut().as_mut().unwrap().await;
        *self.requeue.get_mut() = None;
        self.check().await
    }

    async fn finish_controller_call(&mut self) -> Result<()> {
        self.check().await?;
        if self.controller_call.get_mut().is_none() {
            return Ok(());
        }
        let outcome = tokio::time::timeout_at(
            self.deadline,
            self.controller_call
                .get_mut()
                .as_mut()
                .unwrap()
                .future
                .as_mut(),
        )
        .await
        .map_err(|error| {
            super::TestError::with_source(super::ErrorKind::Deadline, "reconcile deadline", error)
        })?;
        let call = self.controller_call.get_mut().take().unwrap();
        let (delay, wake_on_change) = match outcome {
            Ok(action) => (action.requeue_after, true),
            Err(error) => {
                self.reconcile_errors += 1;
                self.last_error = Some(error.to_string());
                (Duration::from_millis(20), false)
            }
        };
        let deadline = self.deadline;
        let requeue_at = Instant::now()
            .checked_add(delay)
            .unwrap_or(deadline)
            .min(deadline);
        *self.requeue.get_mut() = Some(Box::pin(async move {
            if delay.is_zero() {
                tokio::task::yield_now().await;
            } else {
                tokio::select! {
                    _ = call.changed, if wake_on_change => {}
                    _ = tokio::time::sleep_until(requeue_at) => {}
                }
            }
        }));
        self.check().await
    }

    fn diagnostic_snapshot(&self) -> serde_json::Value {
        let mut objects = snapshot(&self.api);
        objects.iter_mut().for_each(redact);
        objects.truncate(32);
        fn bound(value: &mut serde_json::Value) {
            match value {
                serde_json::Value::String(text) if text.len() > 4096 => {
                    *text = text.chars().take(4096).collect::<String>() + "[truncated]";
                }
                serde_json::Value::Object(values) => values.values_mut().for_each(bound),
                serde_json::Value::Array(values) => {
                    values.truncate(64);
                    values.iter_mut().for_each(bound);
                }
                _ => {}
            }
        }
        objects.iter_mut().for_each(bound);
        if serde_json::to_vec(&objects).map_or(true, |value| value.len() > 65536) {
            return serde_json::json!({"truncated":true,"objects":objects.into_iter().map(|object|serde_json::json!({"kind":object["kind"],"metadata":object["metadata"]})).collect::<Vec<_>>()});
        }
        serde_json::json!(objects)
    }

    #[cfg(test)]
    pub async fn ready(&mut self) -> Result<i64> {
        self.ready_members(3).await
    }

    pub async fn ready_members(&mut self, expected: usize) -> Result<i64> {
        self.ready_observed(expected, &[]).await
    }

    pub async fn ready_survivors(&mut self, expected: usize, isolated: &[String]) -> Result<i64> {
        for uid in isolated {
            ensure!(
                self.network.is_isolated(uid).await,
                "survivor observation requires acknowledged isolation"
            );
        }
        self.ready_observed(expected, isolated).await
    }

    async fn ready_observed(&mut self, expected: usize, isolated: &[String]) -> Result<i64> {
        loop {
            self.check().await?;
            let set = self.set().await?;
            if let Some(status) = set.status.as_ref()
                && status.public_v1_operation.is_none()
                && status.public_v1_transition.is_none()
                && status.public_v1_replacement.is_none()
                && status.public_v1_session_renewal.is_none()
                && let Some(topology) = status.public_v1_topology.as_ref()
            {
                let configuration = topology.configuration().map_err(anyhow::Error::msg)?;
                if configuration.members.len() == expected {
                    ensure!(
                        !configuration.members.iter().any(|member| isolated
                            .iter()
                            .any(|uid| member.identity.instance_id.as_str() == uid)),
                        "isolated host remains admitted"
                    );
                    let uids = self
                        .live_uids()
                        .await
                        .into_iter()
                        .filter(|uid| !isolated.contains(uid))
                        .collect::<Vec<_>>();
                    let mut reports = Vec::new();
                    let mut complete = true;
                    for uid in &uids {
                        match self.report_uid(uid).await {
                            Ok(report) => reports.push(report),
                            Err(error) => {
                                self.last_error = Some(format!("readiness observation: {error:#}"));
                                complete = false;
                            }
                        }
                    }
                    if complete
                        && Self::observations_complete(&uids, &reports)
                        && Self::settled(status, &reports, expected)
                        && Self::configurations_settled(
                            status,
                            &reports,
                            &self.configuration_evidence(&reports).await?,
                        )
                    {
                        return Ok(configuration.primary_id.value());
                    }
                }
            }
            self.step().await?;
        }
    }

    #[cfg(test)]
    pub async fn report(
        &self,
        replica_id: i64,
    ) -> Result<kuberic_runtime::protocol::public_operations::PublicV1AgentReport> {
        let set = self.set().await?;
        let configuration = set
            .status
            .as_ref()
            .and_then(|status| status.public_v1_topology.as_ref())
            .context("topology missing")?
            .configuration()
            .map_err(|error| anyhow!("{error}"))?;
        let member = configuration
            .members
            .iter()
            .find(|member| member.identity.replica_id.value() == replica_id)
            .context("member missing")?;
        self.report_uid(member.identity.instance_id.as_str()).await
    }

    pub async fn live_uids(&self) -> Vec<String> {
        self.supervisor
            .lock()
            .await
            .hosts
            .iter()
            .filter(|(_, host)| {
                host.running
                    .as_ref()
                    .is_some_and(|running| !running.is_finished())
            })
            .map(|(uid, _)| uid.clone())
            .collect()
    }

    pub async fn data_root(&self, uid: &str) -> Result<PathBuf> {
        self.supervisor
            .lock()
            .await
            .hosts
            .get(uid)
            .map(|host| host.data_root.clone())
            .context("host missing")
    }

    pub async fn service_uid(&self, uid: &str) -> Result<Arc<F::Service>> {
        self.supervisor
            .lock()
            .await
            .hosts
            .get(uid)
            .map(|host| host.service.clone())
            .context("host missing")
    }

    pub fn observations_complete(uids: &[String], reports: &[PublicV1AgentReport]) -> bool {
        uids.iter().all(|uid| {
            reports
                .iter()
                .any(|report| report.replica.instance_id.as_str() == uid)
        })
    }

    pub async fn configuration_evidence(
        &self,
        reports: &[PublicV1AgentReport],
    ) -> Result<ConfigurationEvidence> {
        let handles = self
            .supervisor
            .lock()
            .await
            .hosts
            .iter()
            .filter_map(|(uid, host)| {
                host.running
                    .as_ref()
                    .map(|running| (uid.clone(), running.handle()))
            })
            .collect::<BTreeMap<_, _>>();
        let mut evidence = BTreeMap::new();
        for report in reports {
            let Some(handle) = handles.get(report.replica.instance_id.as_str()) else {
                continue;
            };
            let mut completions = report
                .completed_operations
                .iter()
                .filter(|completion| {
                    completion.process_session_id == report.process_session_id
                        && completion.result == PublicV1OperationResult::Succeeded
                })
                .collect::<Vec<_>>();
            completions.sort_by_key(|completion| std::cmp::Reverse(completion.revision));
            for completion in completions {
                let Some(intent) = tokio::time::timeout_at(
                    self.deadline,
                    handle.public_v1_operation_intent(&completion.operation_id),
                )
                .await??
                else {
                    continue;
                };
                if matches!(
                    intent.program,
                    Some(
                        PublicOperationProgram::Configuration(_)
                            | PublicOperationProgram::CatchUp { .. }
                            | PublicOperationProgram::Swap { .. }
                    )
                ) {
                    evidence.insert(
                        report.replica.instance_id.to_string(),
                        (intent, completion.clone()),
                    );
                    break;
                }
            }
        }
        Ok(ConfigurationEvidence {
            operations: evidence,
            renewals: self.renewals.lock().unwrap().values().cloned().collect(),
        })
    }

    pub fn configurations_settled(
        status: &KubericSetStatus,
        reports: &[PublicV1AgentReport],
        evidence: &ConfigurationEvidence,
    ) -> bool {
        let Some(topology) = &status.public_v1_topology else {
            return false;
        };
        if !Self::settled(status, reports, topology.member_count as usize) {
            return false;
        }
        let Ok(configuration) = topology.configuration() else {
            return false;
        };
        let Some(authority) = &status.public_v1_authority else {
            return false;
        };
        let Ok(accepted) = authority.authority() else {
            return false;
        };
        topology.sessions.iter().all(|session| {
            let Some((intent, completion)) = evidence
                .operations
                .get(session.identity.instance_id.as_str())
            else {
                return false;
            };
            if intent.validate().is_err()
                || intent.process_session_id != session.process_session_id
                || intent.operation_id != completion.operation_id
                || intent.revision != completion.revision
                || intent.input_digest != completion.input_digest
                || completion.process_session_id != session.process_session_id
                || completion.history != accepted.history
                || completion.storage_incarnation != session.storage_incarnation
            {
                return false;
            }
            let public = match &intent.program {
                Some(
                    PublicOperationProgram::Configuration(public)
                    | PublicOperationProgram::CatchUp {
                        configuration: public,
                        ..
                    },
                ) => public,
                Some(PublicOperationProgram::Swap { refreshed, .. }) => refreshed,
                _ => return false,
            };
            let authorized = accepted.completed_operations.contains(
                &PublicV1AcceptedOperationCompletion::from_completion(completion),
            ) || evidence.renewals.iter().any(|required| {
                required.history == accepted.history
                    && required.fresh_configuration == configuration
                    && (intent.operation_id.to_string()
                        == format!(
                            "{}-configuration-{}",
                            required.operation_id, session.identity.replica_id
                        )
                        || (session.role == ReplicaRole::Primary
                            && matches!(
                                intent.program,
                                Some(PublicOperationProgram::CatchUp {
                                    mode: kuberic_runtime::protocol::public_operations::
                                        PublicCatchUpMode::All,
                                    ..
                                })
                            )
                            && intent.operation_id
                                == crate::public_v1_reconciliation::renewal_settlement_id(
                                    &required.operation_id,
                                    session.identity.replica_id,
                                    intent.revision,
                                    intent.program.as_ref().expect("CatchUp checked"),
                                )))
                    && required
                        .replacement
                        .iter()
                        .chain(required.surviving_sessions.iter())
                        .any(|endpoint| {
                            let mut authorized = endpoint.clone();
                            authorized.role = session.role;
                            authorized == *session
                        })
            });
            authorized
                && public.current == configuration
                && public.previous.is_none()
                && public.validate().is_ok()
                && public.replicas.iter().all(|peer| {
                    topology.sessions.iter().any(|admitted| {
                        peer.identity == admitted.identity
                            && peer.process_session_id == admitted.process_session_id
                            && peer.storage_incarnation.as_ref()
                                == Some(&admitted.storage_incarnation)
                            && peer.role == admitted.role
                    }) && reports.iter().any(|report| {
                        report.replica == peer.identity
                            && report.replication_address.as_ref().is_some_and(|address| {
                                address.trim_end_matches('/')
                                    == peer.replication_address.trim_end_matches('/')
                            })
                    })
                })
        })
    }

    pub async fn report_uid(&self, uid: &str) -> Result<PublicV1AgentReport> {
        let set = self.set().await?;
        let pod = self
            .api
            .snapshot(Kind::Pod)
            .into_iter()
            .find(|pod| pod["metadata"]["uid"].as_str() == Some(uid))
            .with_context(|| {
                format!(
                    "member Pod missing: {uid}; current Pods: {:?}",
                    self.api
                        .snapshot(Kind::Pod)
                        .iter()
                        .map(|pod| (
                            pod["metadata"]["name"].clone(),
                            pod["metadata"]["uid"].clone()
                        ))
                        .collect::<Vec<_>>()
                )
            })?;
        let endpoint = pod["metadata"]["annotations"][CONTROL_ADDRESS_ANNOTATION]
            .as_str()
            .context("control address missing")?;
        let replica_id = pod["metadata"]["labels"][crate::crd::REPLICA_ID_LABEL]
            .as_str()
            .context("replica label missing")?
            .parse()?;
        let report = tokio::time::timeout_at(
            self.deadline,
            GrpcAgentApi::new(pod::RPC_DEADLINE).get_status(
                endpoint,
                "local-test-token",
                kuberic_runtime::control::proto::GetAgentStatusRequest {
                    protocol_version: kuberic_runtime::protocol::PROTOCOL_VERSION,
                    resource_uid: set.uid().context("set UID missing")?,
                    replica_id,
                    expected_instance_id: uid.to_owned(),
                },
            ),
        )
        .await?
        .map_err(|error| anyhow!("agent report: {error:?}"))?;
        let report = kuberic_runtime::control::normalize_public_v1_agent_status_report(report)
            .map_err(|error| anyhow!("{error}"))?;
        let mut reports = self.last_reports.lock().unwrap();
        reports.insert(uid.to_owned(), report.clone());
        while reports.len() > 16 {
            reports.pop_first();
        }
        Ok(report)
    }

    pub(super) fn settled(
        status: &KubericSetStatus,
        reports: &[PublicV1AgentReport],
        expected: usize,
    ) -> bool {
        if status.public_v1_operation.is_some()
            || status.public_v1_transition.is_some()
            || status.public_v1_replacement.is_some()
            || status.public_v1_session_renewal.is_some()
        {
            return false;
        }
        let Some(topology) = &status.public_v1_topology else {
            return false;
        };
        if topology.pending_switchover.is_some() {
            return false;
        }
        let Ok(configuration) = topology.configuration() else {
            return false;
        };
        let Some(authority) = &status.public_v1_authority else {
            return false;
        };
        let Ok(accepted) = authority.authority() else {
            return false;
        };
        if configuration.members.len() != expected {
            return false;
        }
        for session in &topology.sessions {
            let Some(report) = reports
                .iter()
                .find(|report| report.replica == session.identity)
            else {
                return false;
            };
            if report.identity != accepted.identity
                || report.history != accepted.history
                || report.resource_uid != accepted.history.resource_uid
                || report.storage_incarnation != session.storage_incarnation
                || report.process_session_id != session.process_session_id
                || report.replicator_creation_id != session.replicator_creation_id
                || !report.has_current_open_endpoint()
                || report.role != session.role
                || (session.role == ReplicaRole::Primary
                    && report.read_status != AccessStatus::Granted)
                || (session.role == ReplicaRole::ActiveSecondary
                    && report.read_status != AccessStatus::NotPrimary)
                || (report.write_status == AccessStatus::Granted)
                    != (session.role == ReplicaRole::Primary)
                || report.progress.current().is_none()
                || !report.completed_operations.iter().any(|completion| {
                    completion.process_session_id == session.process_session_id
                        && completion.storage_incarnation == session.storage_incarnation
                        && completion.history == accepted.history
                        && completion.current_configuration_id.as_ref()
                            == Some(&configuration.configuration_id)
                        && completion.result == PublicV1OperationResult::Succeeded
                })
            {
                return false;
            }
        }
        !reports.iter().any(|report| {
            !topology
                .sessions
                .iter()
                .any(|session| session.identity == report.replica)
                && (report.role != ReplicaRole::None
                    || report.read_status == AccessStatus::Granted
                    || report.write_status == AccessStatus::Granted)
        })
    }

    pub async fn stop(&self, uid: &str) -> Result<()> {
        let mut state = self.supervisor.lock().await;
        state.stopped.insert(uid.to_owned());
        let host = state.hosts.remove(uid).context("host missing")?;
        state.drains.push(tokio::spawn(host.stop()));
        let index = state.drains.len() - 1;
        let outcome = tokio::time::timeout_at(self.deadline, &mut state.drains[index]).await;
        match outcome {
            Ok(result) => {
                state.drains.swap_remove(index);
                result??;
            }
            Err(error) => return Err(error.into()),
        }
        Ok(())
    }

    pub async fn finish(mut self, result: Result<()>) -> Result<()> {
        self.cancel_controller_call();
        let initial_transport = self.network.diagnostics();
        let scenario_kind = result
            .as_ref()
            .err()
            .and_then(|error| error.downcast_ref::<super::TestError>())
            .map_or(super::ErrorKind::Application, |error| error.kind());
        let mut errors = result.err().into_iter().collect::<Vec<_>>();
        let scenario_errors = errors.len();
        if let Err(error) = self.check().await {
            errors.push(error);
        }
        let teardown = Instant::now() + self.teardown_timeout;
        self.api.release_barriers();
        self.supervisor_shutdown.send_replace(true);
        if let Some(task) = self.supervisor_task.take() {
            let mut task = task;
            match tokio::time::timeout_at(teardown, &mut task).await {
                Ok(Ok(Ok(()))) => {}
                Ok(Ok(Err(error))) => errors.push(error),
                Ok(Err(error)) => errors.push(error.into()),
                Err(error) => {
                    task.abort();
                    let _ = task.await;
                    errors.push(error.into());
                }
            }
        }
        let network = self.network.clone();
        let mut transport_drain = tokio::spawn(async move { network.finish().await });
        match tokio::time::timeout_at(teardown, &mut transport_drain).await {
            Ok(Ok(Ok(()))) => {}
            Ok(Ok(Err(error))) => errors.push(error),
            Ok(Err(error)) => errors.push(error.into()),
            Err(error) => errors.push(error.into()),
        }
        let mut drains = {
            let mut state = self.supervisor.lock().await;
            state.held_deletions.lock().unwrap().clear();
            let hosts = std::mem::take(&mut state.hosts);
            state
                .drains
                .extend(hosts.into_values().map(|host| tokio::spawn(host.stop())));
            std::mem::take(&mut state.drains)
        };
        for drain in &mut drains {
            match tokio::time::timeout_at(teardown, drain).await {
                Ok(Ok(Ok(()))) => {}
                Ok(Ok(Err(error))) => errors.push(error),
                Ok(Err(error)) => errors.push(error.into()),
                Err(error) => errors.push(error.into()),
            }
            match tokio::time::timeout_at(teardown, self.network.assert_drained()).await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => errors.push(error),
                Err(error) => errors.push(error.into()),
            }
        }
        if let Err(error) = self.api.check() {
            errors.push(error);
        }
        self.cleaned = true;
        if errors.is_empty() {
            Ok(())
        } else {
            let path = self.root.take().unwrap().keep();
            let kind = if errors.len() > scenario_errors {
                super::ErrorKind::Cleanup
            } else {
                scenario_kind
            };
            let agents=self.last_reports.lock().unwrap().values().map(|report|serde_json::json!({"identity":report.replica,"session":report.process_session_id,"creation":report.replicator_creation_id,"storage":report.storage_incarnation,"role":report.role,"read":report.read_status,"write":report.write_status,"progress":report.progress})).collect::<Vec<_>>();
            let diagnostics = serde_json::json!({"resources":self.diagnostic_snapshot(),"agents":agents,"transportBeforeCleanup":initial_transport,"transport":self.network.diagnostics(),"reconcileErrors":self.reconcile_errors,"lastReconcileError":self.last_error.as_ref().map(|error|error.replace("local-test-token","[redacted]").chars().take(512).collect::<String>()),"retainedStorage":path,"pendingTaskOwners":drains.iter().filter(|task|!task.is_finished()).count()});
            Err(super::TestError::combined(
                kind,
                format!(
                    "scenario/cleanup failed; retained storage: {}",
                    path.display()
                ),
                errors,
            )
            .with_diagnostics(diagnostics)
            .into())
        }
    }
}

impl<F: ApplicationFactory> Drop for Harness<F> {
    fn drop(&mut self) {
        self.cancel_controller_call();
        self.supervisor_shutdown.send_replace(true);
        if !self.cleaned
            && let Some(root) = self.root.take()
        {
            eprintln!(
                "in-process harness dropped before cleanup; retained storage: {}",
                root.keep().display()
            );
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                let task = self.supervisor_task.take();
                let state = self.supervisor.clone();
                let network = self.network.clone();
                let api = self.api.clone();
                let deadline = Instant::now() + self.teardown_timeout;
                runtime.spawn(async move {
                    api.release_barriers();
                    if let Some(mut task) = task
                        && tokio::time::timeout_at(deadline, &mut task).await.is_err()
                    {
                        task.abort();
                        let _ = task.await;
                    }
                    let mut transport = tokio::spawn(async move { network.finish().await });
                    let _ = tokio::time::timeout_at(deadline, &mut transport).await;
                    let drains = {
                        let mut state = state.lock().await;
                        state.held_deletions.lock().unwrap().clear();
                        let hosts = std::mem::take(&mut state.hosts);
                        state
                            .drains
                            .extend(hosts.into_values().map(|host| tokio::spawn(host.stop())));
                        std::mem::take(&mut state.drains)
                    };
                    for mut drain in drains {
                        // Timeout detaches the still-owning drain task; retained
                        // storage is never certified reclaimed on this path.
                        let _ = tokio::time::timeout_at(deadline, &mut drain).await;
                    }
                });
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn supervise<F: ApplicationFactory>(
    api: &TestApi,
    state: &Mutex<SupervisorState<F>>,
    root: &std::path::Path,
    network: &Network,
    factory: &Arc<F>,
    namespace: &str,
    image: &str,
    deadline: Instant,
) -> Result<()> {
    let mut state = state.lock().await;
    let objects = snapshot(api);
    let mut pods: Vec<Pod> = Vec::new();
    let mut pvcs: Vec<PersistentVolumeClaim> = Vec::new();
    let mut secrets: Vec<Secret> = Vec::new();
    for object in objects {
        match object["kind"].as_str() {
            Some("Pod") => pods.push(serde_json::from_value(object)?),
            Some("PersistentVolumeClaim") => pvcs.push(serde_json::from_value(object)?),
            Some("Secret") => secrets.push(serde_json::from_value(object)?),
            _ => {}
        }
    }
    for pod in &mut pods {
        let uid = pod.uid().context("Pod UID missing")?;
        if pod.metadata.deletion_timestamp.is_some() {
            if let Some(host) = state.hosts.remove(&uid) {
                state.drains.push(tokio::spawn(host.stop()));
            }
            if state.drains.iter().any(|drain| !drain.is_finished()) {
                continue;
            }
            if !state.held_deletions.lock().unwrap().contains(&uid) {
                api.finalize_deletion(Kind::Pod, &pod.name_any(), &uid)?;
            }
            continue;
        }
        if state.stopped.contains(&uid) {
            continue;
        }
        if let Some(host) = state.hosts.get_mut(&uid) {
            if host.startup.as_ref().is_some_and(JoinHandle::is_finished) {
                host.running = Some(
                    host.startup
                        .take()
                        .unwrap()
                        .await??
                        .context("host startup cancelled unexpectedly")?,
                );
                api.changed().notify_waiters();
            }
            if host
                .running
                .as_ref()
                .is_some_and(RunningReplica::is_finished)
            {
                host.running.take().unwrap().wait().await?;
                bail!("replica host stopped unexpectedly");
            }
            continue;
        }
        let status = pod.status.get_or_insert_with(PodStatus::default);
        status.pod_ip = Some("127.0.0.1".into());
        let (config, environment) = pod::resolve(pod, &pvcs, &secrets, root, image)?;
        let control_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let backend = format!("http://{}", control_listener.local_addr()?);
        let control = network.route("controller", &uid, &backend).await?;
        let app_path = config.data_root.join("application");
        std::fs::create_dir_all(&app_path)?;
        use kuberic_runtime::protocol::types::{
            ReplicaIdentity, ReplicaInstanceId, derive_agent_generation, derive_initialization_id,
        };
        let identity = ReplicaIdentity {
            replica_id: config.replica_id,
            instance_id: ReplicaInstanceId::new(uid.clone()),
            agent_generation: derive_agent_generation(&derive_initialization_id(
                &config.resource_uid,
                config.replica_id,
                &config.pod_uid,
                &config.pvc_uid,
            )),
        };
        let application = tokio::time::timeout_at(
            deadline,
            factory.create(ReplicaContext {
                identity,
                root: app_path.clone(),
                environment,
            }),
        )
        .await?
        .map_err(|error| {
            super::TestError::with_source(
                super::ErrorKind::Application,
                "application factory failed",
                error,
            )
        })?;
        application.validate(&app_path)?;
        let service = application.service;
        let resolver = Arc::new(KubernetesDnsResolver::new(
            config.resource_uid.clone(),
            namespace,
        ));
        let data_root = config.data_root.clone();
        let (startup_shutdown, receiver) = watch::channel(false);
        let routed_uid = uid.clone();
        let routed_network = network.clone();
        let route = kuberic_runtime::replicator::TestingTcpRoute::register_with_kind(
            uid.clone(),
            move |kind, destination, endpoint| {
                let network = routed_network.clone();
                let uid = routed_uid.clone();
                Box::pin(async move {
                    network
                        .route_data(kind, &uid, &destination, &endpoint)
                        .await
                        .map_err(std::io::Error::other)
                })
            },
        )?;
        let mut host = ReplicaHost::new(config, service.clone(), application.storage, resolver);
        if !application.paths.is_empty() {
            host = host.with_application_storage_paths(
                application
                    .paths
                    .into_iter()
                    .enumerate()
                    .map(|(index, path)| (format!("application-{index}"), path))
                    .collect(),
            );
        }
        pod.annotations_mut()
            .insert(CONTROL_ADDRESS_ANNOTATION.into(), control);
        let infrastructure = serde_json::to_value(&*pod)?;
        let mut updated = false;
        for _ in 0..10 {
            let current = api
                .get(Kind::Pod, &pod.name_any())
                .context("Pod disappeared during startup")?;
            let rv = current["metadata"]["resourceVersion"]
                .as_str()
                .context("Pod version missing")?;
            match api.update_infrastructure(Kind::Pod, &pod.name_any(), &uid, rv, |next| {
                next["status"] = infrastructure["status"].clone();
                let annotations = next["metadata"]
                    .as_object_mut()
                    .unwrap()
                    .entry("annotations")
                    .or_insert_with(|| serde_json::json!({}));
                annotations[CONTROL_ADDRESS_ANNOTATION] =
                    infrastructure["metadata"]["annotations"][CONTROL_ADDRESS_ANNOTATION].clone();
            }) {
                Ok(_) => {
                    updated = true;
                    break;
                }
                Err(error) if error.to_string() == "infrastructure resourceVersion changed" => {
                    continue;
                }
                Err(error) => return Err(error),
            }
        }
        ensure!(updated, "Pod infrastructure updates repeatedly conflicted");
        let startup = tokio::spawn(host.start_with_shutdown(control_listener, receiver));
        state.hosts.insert(
            uid.clone(),
            Hosted {
                service,
                factory: factory.clone(),
                startup_shutdown,
                startup: Some(startup),
                running: None,
                data_root,
                _route: route,
                network: network.clone(),
                uid: uid.clone(),
            },
        );
    }
    for pvc in &pvcs {
        if pvc.metadata.deletion_timestamp.is_some() {
            let uid = pvc.uid().context("PVC UID missing")?;
            let directory = root.join(&uid);
            if state.hosts.values().any(|host| host.data_root == directory) {
                continue;
            }
            if state.held_deletions.lock().unwrap().contains(&uid) {
                continue;
            }
            if directory.exists() {
                std::fs::remove_dir_all(&directory)?;
            }
            api.finalize_deletion(Kind::Pvc, &pvc.name_any(), &uid)?;
        }
    }
    for object in snapshot(api) {
        if object["metadata"]["deletionTimestamp"].is_string()
            && object["kind"] != "Pod"
            && object["kind"] != "PersistentVolumeClaim"
        {
            let kind = match object["kind"].as_str() {
                Some("Service") => Kind::Service,
                Some("Secret") => Kind::Secret,
                _ => bail!("unsupported resource deletion"),
            };
            let uid = object["metadata"]["uid"].as_str().context("UID missing")?;
            if !state.held_deletions.lock().unwrap().contains(uid) {
                api.finalize_deletion(
                    kind,
                    object["metadata"]["name"]
                        .as_str()
                        .context("name missing")?,
                    uid,
                )?;
            }
        }
    }
    Ok(())
}

fn snapshot(api: &TestApi) -> Vec<serde_json::Value> {
    [
        Kind::KubericSet,
        Kind::Pod,
        Kind::Pvc,
        Kind::Service,
        Kind::Secret,
    ]
    .into_iter()
    .flat_map(|kind| api.snapshot(kind))
    .collect()
}

use super::{
    ErrorKind, TestError,
    faults::{Barrier, DeletionHold, OperationEvidence, Pause, SwitchoverRequest},
    transport::Isolation,
};
use kuberic_runtime::protocol::types::{
    ProcessSessionId, ReplicaIdentity, ReplicaStorageIncarnationContext, ReplicatorCreationId,
};

/// Exact cluster, Pod/PVC, process and replicator incarnation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReplicaRef {
    cluster: uuid::Uuid,
    resource_uid: String,
    identity: ReplicaIdentity,
    storage: ReplicaStorageIncarnationContext,
    session: ProcessSessionId,
    creation: ReplicatorCreationId,
    replication_address: Option<String>,
}
impl ReplicaRef {
    pub fn identity(&self) -> &ReplicaIdentity {
        &self.identity
    }
    pub fn replica_id(&self) -> i64 {
        self.identity.replica_id.value()
    }
    pub fn instance_id(&self) -> &str {
        self.identity.instance_id.as_str()
    }
    pub fn process_session_id(&self) -> &ProcessSessionId {
        &self.session
    }
    pub fn replicator_creation_id(&self) -> &ReplicatorCreationId {
        &self.creation
    }
    pub fn storage_incarnation(&self) -> &ReplicaStorageIncarnationContext {
        &self.storage
    }
    pub fn replication_address(&self) -> Option<&str> {
        self.replication_address.as_deref()
    }
    pub(super) fn matches_command(
        &self,
        identity: &ReplicaIdentity,
        session: &ProcessSessionId,
    ) -> bool {
        self.identity == *identity && self.session == *session
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum ResourceKind {
    KubericSet,
    Pod,
    Pvc,
    Service,
    Secret,
}
impl ResourceKind {
    fn internal(self) -> Kind {
        match self {
            Self::KubericSet => Kind::KubericSet,
            Self::Pod => Kind::Pod,
            Self::Pvc => Kind::Pvc,
            Self::Service => Kind::Service,
            Self::Secret => Kind::Secret,
        }
    }
    fn from_internal(kind: Kind) -> Self {
        match kind {
            Kind::KubericSet => Self::KubericSet,
            Kind::Pod => Self::Pod,
            Kind::Pvc => Self::Pvc,
            Kind::Service => Self::Service,
            Kind::Secret => Self::Secret,
        }
    }
}
/// An exact infrastructure incarnation, not admission or service access.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResourceRef {
    cluster: uuid::Uuid,
    kind: ResourceKind,
    name: String,
    uid: String,
}
impl ResourceRef {
    pub fn kind(&self) -> ResourceKind {
        self.kind
    }
    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn uid(&self) -> &str {
        &self.uid
    }
}
#[derive(Clone, Debug)]
pub struct ResourceObservation {
    reference: ResourceRef,
    value: serde_json::Value,
}
impl ResourceObservation {
    pub fn reference(&self) -> &ResourceRef {
        &self.reference
    }
    /// Detached and redacted resource data.
    pub fn value(&self) -> &serde_json::Value {
        &self.value
    }
}
/// Detached observations; none of these values can alter host/controller state.
#[derive(Clone, Debug)]
pub struct Observation {
    set: KubericSet,
    resources: Vec<ResourceObservation>,
    reports: Vec<(ReplicaRef, PublicV1AgentReport)>,
    reconcile_errors: usize,
    credentials_consistent: bool,
    failures: Vec<ObservationFailure>,
}
impl Observation {
    pub fn set(&self) -> &KubericSet {
        &self.set
    }
    pub fn resources(&self) -> &[ResourceObservation] {
        &self.resources
    }
    pub fn replicas(&self) -> impl Iterator<Item = &ReplicaRef> {
        self.reports.iter().map(|(reference, _)| reference)
    }
    pub fn reports(&self) -> impl Iterator<Item = (&ReplicaRef, &PublicV1AgentReport)> {
        self.reports
            .iter()
            .map(|(reference, report)| (reference, report))
    }
    pub fn report(&self, reference: &ReplicaRef) -> Option<&PublicV1AgentReport> {
        self.reports
            .iter()
            .find(|(candidate, _)| candidate == reference)
            .map(|(_, report)| report)
    }
    pub fn reconcile_errors(&self) -> usize {
        self.reconcile_errors
    }
    /// Startup credentials still match the owned controller/host contract.
    pub fn credentials_consistent(&self) -> bool {
        self.credentials_consistent
    }
    pub fn failures(&self) -> &[ObservationFailure] {
        &self.failures
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ObservationFailureKind {
    Isolated,
    PendingStartup,
    Unavailable,
}
#[derive(Clone, Debug)]
pub struct ObservationFailure {
    instance_id: String,
    kind: ObservationFailureKind,
    error: Option<Arc<TestError>>,
}
impl ObservationFailure {
    pub fn instance_id(&self) -> &str {
        &self.instance_id
    }
    pub fn kind(&self) -> ObservationFailureKind {
        self.kind
    }
    pub fn error(&self) -> Option<&TestError> {
        self.error.as_deref()
    }
}
#[derive(Clone, Debug)]
pub struct Topology {
    primary: ReplicaRef,
    members: Vec<ReplicaRef>,
}
impl Topology {
    pub fn primary(&self) -> &ReplicaRef {
        &self.primary
    }
    pub fn members(&self) -> impl Iterator<Item = &ReplicaRef> {
        self.members.iter()
    }
    pub fn secondaries(&self) -> impl Iterator<Item = &ReplicaRef> {
        self.members
            .iter()
            .filter(|member| *member != &self.primary)
    }
}
#[derive(Clone, Copy, Debug)]
pub enum SupportResource {
    PeerService,
    CredentialsSecret,
}

/// Configuration for one persisted application (three replicas by default).
pub struct ClusterBuilder<F> {
    factory: F,
    namespace: String,
    name: String,
    image: String,
    timeout: Duration,
    teardown_timeout: Duration,
    initial_replicas: u32,
}
impl<F: ApplicationFactory> ClusterBuilder<F> {
    pub fn initial_replicas(mut self, replicas: u32) -> Self {
        self.initial_replicas = replicas;
        self
    }
    pub fn namespace(mut self, value: impl Into<String>) -> Self {
        self.namespace = value.into();
        self
    }
    pub fn name(mut self, value: impl Into<String>) -> Self {
        self.name = value.into();
        self
    }
    pub fn image(mut self, value: impl Into<String>) -> Self {
        self.image = value.into();
        self
    }
    pub fn scenario_timeout(mut self, value: Duration) -> Self {
        self.timeout = value;
        self
    }
    pub fn teardown_timeout(mut self, value: Duration) -> Self {
        self.teardown_timeout = value;
        self
    }
    pub async fn start(self) -> super::Result<TestCluster<F>> {
        for value in [&self.namespace, &self.name] {
            crate::validate_installation_namespace(value).map_err(|error| {
                TestError::with_source(
                    ErrorKind::InvalidConfiguration,
                    "namespace/name must be a DNS label",
                    error,
                )
            })?;
        }
        let now = Instant::now();
        if self.image.trim().is_empty()
            || !(1..=3).contains(&self.initial_replicas)
            || self.timeout.is_zero()
            || self.teardown_timeout.is_zero()
            || now.checked_add(self.timeout).is_none()
            || now.checked_add(self.teardown_timeout).is_none()
        {
            return Err(TestError::new(
                ErrorKind::InvalidConfiguration,
                "image and positive, representable scenario/teardown timeouts are required",
            ));
        }
        let core = Harness::new(
            self.factory,
            self.namespace,
            self.name,
            self.image,
            self.timeout,
            self.teardown_timeout,
            self.initial_replicas,
        )
        .await?;
        Ok(TestCluster {
            core,
            nonce: uuid::Uuid::new_v4(),
            paused: None,
        })
    }
}

/// Owns the real controller, hosts, applications, private Kubernetes store and TCP routes.
pub struct TestCluster<F: ApplicationFactory> {
    core: Harness<F>,
    nonce: uuid::Uuid,
    paused: Option<uuid::Uuid>,
}

/// Detached results of an exact persisted command dispatched by the production API.
#[derive(Clone, Debug)]
pub struct OperationDispatch {
    pub evidence: OperationEvidence,
    pub dispatch_result: std::result::Result<(), String>,
    pub report: PublicV1AgentReport,
    pub contained: bool,
}

impl<F: ApplicationFactory> TestCluster<F> {
    pub fn builder(factory: F) -> ClusterBuilder<F> {
        ClusterBuilder {
            factory,
            namespace: NAMESPACE.into(),
            name: NAME.into(),
            image: "in-process/application".into(),
            timeout: TIMEOUT,
            teardown_timeout: Duration::from_secs(45),
            initial_replicas: 3,
        }
    }
    fn reference(&self, report: &PublicV1AgentReport) -> ReplicaRef {
        ReplicaRef {
            cluster: self.nonce,
            resource_uid: report.resource_uid.to_string(),
            identity: report.replica.clone(),
            storage: report.storage_incarnation.clone(),
            session: report.process_session_id.clone(),
            creation: report.replicator_creation_id.clone(),
            replication_address: report.replication_address.clone(),
        }
    }
    async fn validate(&self, reference: &ReplicaRef) -> super::Result<PublicV1AgentReport> {
        if reference.cluster != self.nonce {
            return Err(TestError::new(
                ErrorKind::InvalidReference,
                "reference belongs to another cluster",
            ));
        }
        if !self
            .core
            .live_uids()
            .await
            .iter()
            .any(|uid| uid == reference.instance_id())
        {
            return Err(TestError::new(
                ErrorKind::InvalidReference,
                "replica is no longer live",
            ));
        }
        let handle = {
            let state = self.core.supervisor.lock().await;
            state
                .hosts
                .get(reference.instance_id())
                .and_then(|host| host.running.as_ref())
                .map(RunningReplica::handle)
                .ok_or_else(|| {
                    TestError::new(
                        ErrorKind::InvalidReference,
                        "replica startup/session is not live",
                    )
                })?
        };
        // Exact-reference validation is a local identity read, not healthy
        // transport evidence. Readiness still observes every agent over RPC.
        let report = tokio::time::timeout_at(self.core.deadline, handle.public_v1_report())
            .await
            .map_err(anyhow::Error::from)?
            .map_err(anyhow::Error::from)?;
        if self.reference(&report) != *reference {
            return Err(TestError::new(
                ErrorKind::InvalidReference,
                "replica incarnation/session is obsolete",
            ));
        }
        Ok(report)
    }
    fn driving(&mut self) -> super::Result<()> {
        if self.paused.is_some() {
            return Err(TestError::new(
                ErrorKind::InvalidConfiguration,
                "controller is paused; use probe or resume",
            ));
        }
        Ok(())
    }
    async fn check(&mut self) -> super::Result<()> {
        if Instant::now() >= self.core.deadline {
            return Err(TestError::new(
                ErrorKind::Deadline,
                "shared scenario deadline elapsed; an application operation may have durable effects",
            ));
        }
        self.core.check().await.map_err(TestError::from)
    }
    async fn checked_application_result<T>(
        &mut self,
        result: super::Result<T>,
    ) -> super::Result<T> {
        match (result, self.check().await) {
            (result, Ok(())) => result,
            (Ok(_), Err(error)) => Err(error),
            (Err(application), Err(harness)) => Err(TestError::combined(
                harness.kind(),
                "application and harness checks failed",
                vec![application.into(), harness.into()],
            )),
        }
    }
    pub async fn settle(&mut self) -> super::Result<Topology> {
        self.settle_members(3).await
    }
    pub async fn settle_members(&mut self, expected: usize) -> super::Result<Topology> {
        self.driving()?;
        if !(1..=3).contains(&expected) {
            return Err(TestError::new(
                ErrorKind::InvalidConfiguration,
                "one through three admitted members are supported",
            ));
        }
        let deadline = self.core.deadline;
        let primary = tokio::time::timeout_at(deadline, self.core.ready_members(expected))
            .await
            .map_err(anyhow::Error::from)??;
        self.topology(primary).await
    }
    pub async fn settle_survivors(
        &mut self,
        isolation: &Isolation,
        expected: usize,
    ) -> super::Result<Topology> {
        self.driving()?;
        if !(2..=3).contains(&expected) {
            return Err(TestError::new(
                ErrorKind::InvalidConfiguration,
                "only two or three admitted members are supported",
            ));
        }
        let members = self
            .core
            .network
            .isolated_members(isolation)
            .await
            .map_err(Self::network_error)?;
        let deadline = self.core.deadline;
        let primary =
            tokio::time::timeout_at(deadline, self.core.ready_survivors(expected, &members))
                .await
                .map_err(anyhow::Error::from)??;
        self.topology(primary).await
    }
    async fn topology(&mut self, primary: i64) -> super::Result<Topology> {
        let observation = self.observe().await?;
        let status = observation
            .set
            .status
            .as_ref()
            .context("topology missing")
            .map_err(TestError::from)?;
        let topology = status
            .public_v1_topology
            .as_ref()
            .context("topology missing")
            .map_err(TestError::from)?;
        let reports = observation
            .reports
            .iter()
            .map(|(_, report)| report.clone())
            .collect::<Vec<_>>();
        let expected = topology.sessions.len();
        if !Harness::<F>::settled(status, &reports, expected)
            || !Harness::<F>::configurations_settled(
                status,
                &reports,
                &self.core.configuration_evidence(&reports).await?,
            )
            || topology.sessions.iter().any(|session| {
                !observation.reports.iter().any(|(_, report)| {
                    report.replica == session.identity
                        && report.process_session_id == session.process_session_id
                        && report.replicator_creation_id == session.replicator_creation_id
                        && report.storage_incarnation == session.storage_incarnation
                })
            })
        {
            return Err(TestError::new(
                ErrorKind::Fatal,
                "ready topology changed or lost exact admitted observations",
            ));
        }
        let members = observation
            .reports
            .iter()
            .filter(|(_, report)| {
                topology
                    .sessions
                    .iter()
                    .any(|session| session.identity == report.replica)
            })
            .map(|(reference, _)| reference.clone())
            .collect::<Vec<_>>();
        let primary = members
            .iter()
            .find(|member| member.replica_id() == primary)
            .cloned()
            .ok_or_else(|| TestError::new(ErrorKind::Fatal, "ready primary observation missing"))?;
        self.check().await?;
        Ok(Topology { primary, members })
    }
    pub async fn service(&mut self, reference: &ReplicaRef) -> super::Result<Arc<F::Service>> {
        self.check().await?;
        self.validate(reference).await?;
        let service = self.core.service_uid(reference.instance_id()).await?;
        self.check().await?;
        Ok(service)
    }
    pub async fn application_root(&self, reference: &ReplicaRef) -> super::Result<PathBuf> {
        self.validate(reference).await?;
        Ok(self
            .core
            .data_root(reference.instance_id())
            .await?
            .join("application"))
    }
    pub async fn physical_route(
        &self,
        source: &ReplicaRef,
        destination: &ReplicaRef,
    ) -> super::Result<Option<String>> {
        self.validate(source).await?;
        self.validate(destination).await?;
        let endpoint = destination.replication_address().ok_or_else(|| {
            TestError::new(
                ErrorKind::InvalidReference,
                "destination has no Open endpoint",
            )
        })?;
        Ok(self
            .core
            .network
            .physical_route(source.instance_id(), endpoint)
            .await)
    }
    pub async fn observe(&mut self) -> super::Result<Observation> {
        self.check().await?;
        let set = self.core.set().await?;
        let mut reports = Vec::new();
        let mut failures = Vec::new();
        let live = self.core.live_uids().await;
        let all = self
            .core
            .supervisor
            .lock()
            .await
            .hosts
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        for uid in all {
            // Isolated hosts are deliberately not healthy evidence.
            if self.core.network.is_isolated(&uid).await {
                failures.push(ObservationFailure {
                    instance_id: uid,
                    kind: ObservationFailureKind::Isolated,
                    error: None,
                });
                continue;
            }
            if !live.contains(&uid) {
                failures.push(ObservationFailure {
                    instance_id: uid,
                    kind: ObservationFailureKind::PendingStartup,
                    error: None,
                });
                continue;
            }
            match self.core.report_uid(&uid).await {
                Ok(report) => reports.push((self.reference(&report), report)),
                Err(error) => failures.push(ObservationFailure {
                    instance_id: uid,
                    kind: ObservationFailureKind::Unavailable,
                    error: Some(Arc::new(error.into())),
                }),
            }
        }
        let credentials_consistent = self.core.api.snapshot(Kind::Secret).iter().all(|value| {
            serde_json::from_value::<Secret>(value.clone())
                .ok()
                .and_then(|secret| secret.data)
                .and_then(|mut data| data.remove("bearer-token"))
                .is_some_and(|bytes| bytes.0 == b"local-test-token")
        });
        let resources = [
            Kind::KubericSet,
            Kind::Pod,
            Kind::Pvc,
            Kind::Service,
            Kind::Secret,
        ]
        .into_iter()
        .flat_map(|kind| {
            self.core
                .api
                .snapshot(kind)
                .into_iter()
                .map(move |value| (kind, value))
        })
        .map(|(kind, mut value)| {
            let reference = ResourceRef {
                cluster: self.nonce,
                kind: ResourceKind::from_internal(kind),
                name: value["metadata"]["name"].as_str().unwrap().into(),
                uid: value["metadata"]["uid"].as_str().unwrap().into(),
            };
            redact(&mut value);
            ResourceObservation { reference, value }
        })
        .collect();
        self.check().await?;
        Ok(Observation {
            set,
            resources,
            reports,
            failures,
            reconcile_errors: self.core.reconcile_errors,
            credentials_consistent,
        })
    }
    /// Reconcile once, honoring requeue and retry policy.
    pub async fn step(&mut self) -> super::Result<()> {
        self.driving()?;
        self.core.step().await?;
        self.check().await
    }
    pub async fn drive<T, E>(
        &mut self,
        future: impl std::future::Future<Output = std::result::Result<T, E>>,
    ) -> super::Result<T>
    where
        E: Into<anyhow::Error>,
    {
        self.driving()?;
        tokio::pin!(future);
        loop {
            self.check().await?;
            let deadline = self.core.deadline;
            tokio::select! {
                biased;
                result = &mut future => {
                    let result = result.map_err(|source| TestError::with_source(ErrorKind::Application, "application/probe failed", source.into()));
                    return self.checked_application_result(result).await;
                }
                result = self.core.step() => result?,
                _ = tokio::time::sleep_until(deadline) => return Err(TestError::new(ErrorKind::Deadline, "shared scenario deadline elapsed; operation is not rolled back")),
            }
        }
    }
    pub async fn probe<T, E>(
        &mut self,
        future: impl std::future::Future<Output = std::result::Result<T, E>>,
    ) -> super::Result<T>
    where
        E: Into<anyhow::Error>,
    {
        tokio::pin!(future);
        loop {
            self.check().await?;
            tokio::select! {
                result = &mut future => {
                    let result = result.map_err(|source| TestError::with_source(ErrorKind::Application, "application/probe failed", source.into()));
                    return self.checked_application_result(result).await;
                }
                _ = tokio::time::sleep(Duration::from_millis(10)) => {},
            }
        }
    }
    pub async fn stop(&mut self, reference: &ReplicaRef) -> super::Result<()> {
        self.driving()?;
        self.core.finish_controller_call().await?;
        self.validate(reference).await?;
        if self.core.network.is_isolated(reference.instance_id()).await {
            return Err(TestError::new(
                ErrorKind::InvalidReference,
                "heal isolation before stopping/restarting its host",
            ));
        }
        self.core.stop(reference.instance_id()).await?;
        self.check().await
    }
    pub async fn restart(&mut self, reference: &ReplicaRef) -> super::Result<ReplicaRef> {
        self.stop(reference).await?;
        self.core
            .supervisor
            .lock()
            .await
            .stopped
            .remove(reference.instance_id());
        let topology = self.settle().await?;
        topology
            .members
            .into_iter()
            .find(|member| member.replica_id() == reference.replica_id())
            .ok_or_else(|| TestError::new(ErrorKind::Fatal, "restarted member missing"))
    }
    fn validate_resource(&self, reference: &ResourceRef) -> super::Result<serde_json::Value> {
        if reference.cluster != self.nonce {
            return Err(TestError::new(
                ErrorKind::InvalidReference,
                "resource belongs to another cluster",
            ));
        }
        self.core
            .api
            .get(reference.kind.internal(), &reference.name)
            .filter(|value| value["metadata"]["uid"].as_str() == Some(reference.uid()))
            .ok_or_else(|| {
                TestError::new(
                    ErrorKind::InvalidReference,
                    "resource incarnation is obsolete",
                )
            })
    }
    pub async fn delete_replica(&mut self, reference: &ReplicaRef) -> super::Result<()> {
        self.driving()?;
        self.core.finish_controller_call().await?;
        self.check().await?;
        self.validate(reference).await?;
        let pod = self
            .core
            .api
            .snapshot(Kind::Pod)
            .into_iter()
            .find(|value| value["metadata"]["uid"].as_str() == Some(reference.instance_id()))
            .context("Pod missing")
            .map_err(TestError::from)?;
        self.delete_exact(Kind::Pod, pod).await
    }
    async fn delete_exact(&self, kind: Kind, value: serde_json::Value) -> super::Result<()> {
        use ::kube::api::{DeleteParams, Preconditions};
        let params = DeleteParams {
            preconditions: Some(Preconditions {
                uid: value["metadata"]["uid"].as_str().map(str::to_owned),
                resource_version: value["metadata"]["resourceVersion"]
                    .as_str()
                    .map(str::to_owned),
            }),
            ..Default::default()
        };
        let name = value["metadata"]["name"].as_str().unwrap();
        let future = async {
            match kind {
                Kind::Pod => {
                    Api::<Pod>::namespaced(self.core.api.client(), &self.core.namespace)
                        .delete(name, &params)
                        .await?;
                }
                Kind::Service => {
                    Api::<k8s_openapi::api::core::v1::Service>::namespaced(
                        self.core.api.client(),
                        self.core.namespace.as_str(),
                    )
                    .delete(name, &params)
                    .await?;
                }
                Kind::Secret => {
                    Api::<Secret>::namespaced(self.core.api.client(), &self.core.namespace)
                        .delete(name, &params)
                        .await?;
                }
                _ => return Err(anyhow!("unsupported resource deletion")),
            }
            Ok::<_, anyhow::Error>(())
        };
        tokio::time::timeout_at(self.core.deadline, future)
            .await
            .map_err(|source| {
                TestError::with_source(ErrorKind::Deadline, "resource deletion deadline", source)
            })??;
        Ok(())
    }
    pub async fn delete_support_resource(
        &mut self,
        kind: SupportResource,
    ) -> super::Result<ResourceRef> {
        self.driving()?;
        self.check().await?;
        let (kind, name) = match kind {
            SupportResource::PeerService => (Kind::Service, format!("{}-peer", self.core.name)),
            SupportResource::CredentialsSecret => (
                Kind::Secret,
                format!("{}-agent-credentials", self.core.name),
            ),
        };
        let value = self
            .core
            .api
            .get(kind, &name)
            .context("support resource missing")
            .map_err(TestError::from)?;
        let reference = ResourceRef {
            cluster: self.nonce,
            kind: ResourceKind::from_internal(kind),
            name,
            uid: value["metadata"]["uid"].as_str().unwrap().into(),
        };
        self.delete_exact(kind, value).await?;
        Ok(reference)
    }
    pub async fn hold_deletion(&mut self, reference: &ResourceRef) -> super::Result<DeletionHold> {
        self.check().await?;
        self.validate_resource(reference)?;
        if !matches!(reference.kind, ResourceKind::Pod | ResourceKind::Pvc) {
            return Err(TestError::new(
                ErrorKind::InvalidReference,
                "deletion holds support exact Pod/PVC incarnations only",
            ));
        }
        let held = self.core.supervisor.lock().await.held_deletions.clone();
        if !held.lock().unwrap().insert(reference.uid.clone()) {
            return Err(TestError::new(
                ErrorKind::FaultAlreadyActive,
                "a deletion hold already owns this resource",
            ));
        }
        Ok(DeletionHold {
            resource: reference.clone(),
            held,
        })
    }
    pub async fn submit_switchover(
        &mut self,
        target: &ReplicaRef,
    ) -> super::Result<SwitchoverRequest> {
        self.check().await?;
        self.validate(target).await?;
        let request = SwitchoverRequest {
            cluster: self.nonce,
            id: uuid::Uuid::new_v4().to_string(),
            target: target.clone(),
        };
        let mut set = self.core.set().await?;
        set.spec.switchover = Some(crate::crd::PlannedSwitchoverRequestSpec {
            request_id: request.id.clone(),
            target_replica_id: target.replica_id() as u32,
        });
        self.replace_spec(set).await?;
        Ok(request)
    }
    /// Changes only desired spec cardinality; the real controller performs
    /// initialization, Build and separate configuration admission.
    pub async fn scale_to(&mut self, replicas: u32) -> super::Result<()> {
        self.driving()?;
        if !(1..=3).contains(&replicas) {
            return Err(TestError::new(
                ErrorKind::InvalidConfiguration,
                "one through three replicas are supported",
            ));
        }
        self.check().await?;
        let mut set = self.core.set().await?;
        set.spec.replicas = replicas;
        self.replace_spec(set).await
    }
    async fn replace_spec(&self, set: KubericSet) -> super::Result<()> {
        tokio::time::timeout_at(
            self.core.deadline,
            Api::<KubericSet>::namespaced(self.core.api.client(), &self.core.namespace).replace(
                &self.core.name,
                &PostParams::default(),
                &set,
            ),
        )
        .await
        .map_err(|source| {
            TestError::with_source(ErrorKind::Deadline, "spec update deadline", source)
        })?
        .map_err(|source| TestError::with_source(ErrorKind::Fatal, "spec update failed", source))?;
        Ok(())
    }
    pub async fn clear_switchover(&mut self, request: &SwitchoverRequest) -> super::Result<()> {
        self.check().await?;
        let mut set = self.core.set().await?;
        if request.cluster != self.nonce
            || set
                .spec
                .switchover
                .as_ref()
                .is_none_or(|current| current.request_id != request.id)
        {
            return Err(TestError::new(
                ErrorKind::InvalidReference,
                "switchover request is not current",
            ));
        }
        set.spec.switchover = None;
        self.replace_spec(set).await
    }
    pub async fn switchover(&mut self, target: &ReplicaRef) -> super::Result<Topology> {
        let request = self.submit_switchover(target).await?;
        loop {
            let set = self.core.set().await?;
            if set
                .status
                .as_ref()
                .and_then(|status| status.public_v1_topology.as_ref())
                .is_some_and(|topology| {
                    topology.completed_switchover_request_id.as_deref() == Some(request.id())
                })
            {
                return self.settle().await;
            }
            self.step().await?;
        }
    }
    pub async fn pause_at(&mut self, barrier: Barrier) -> super::Result<Pause> {
        self.driving()?;
        if let Some(participant) = barrier.participant() {
            self.validate(participant).await?;
        }
        if barrier
            .request()
            .is_some_and(|request| request.cluster != self.nonce)
        {
            return Err(TestError::new(
                ErrorKind::InvalidReference,
                "barrier request belongs to another cluster",
            ));
        }
        let mut held = self.core.api.arm_typed(&self.core.name, barrier)?;
        let value = loop {
            self.check().await?;
            tokio::select! {
                value = held.entered() => break value?,
                result = self.core.step() => result?,
            }
        };
        // Only an explicit pause cancels the held call. Ordinary application
        // completion leaves controller/requeue ownership with the driver.
        self.core.cancel_controller_call();
        let owner = uuid::Uuid::new_v4();
        self.paused = Some(owner);
        Ok(Pause {
            cluster: self.nonce,
            owner,
            barrier: Some(held),
            evidence: OperationEvidence::new(
                serde_json::from_value(value).map_err(anyhow::Error::from)?,
            ),
        })
    }
    pub fn completion_status_conflict(&mut self) -> super::Result<super::CompletionConflict> {
        self.driving()?;
        Ok(super::CompletionConflict {
            barrier: self.core.api.arm_completion_conflict(&self.core.name)?,
            deadline: self.core.deadline,
        })
    }
    pub async fn resume(&mut self, mut pause: Pause) -> super::Result<OperationEvidence> {
        if pause.cluster != self.nonce || self.paused != Some(pause.owner) {
            return Err(TestError::new(
                ErrorKind::InvalidReference,
                "pause owner is not current",
            ));
        }
        if let Some(barrier) = pause.barrier.take() {
            barrier.release();
        }
        self.paused = None;
        self.check().await?;
        Ok(pause.evidence.clone())
    }
    pub async fn recreate_controller(&mut self) -> super::Result<()> {
        self.check().await?;
        self.core.recreate_controller()?;
        Ok(())
    }

    /// Read the actual target grant without creating or changing native authority.
    pub async fn inbound_build_authority(
        &mut self,
        replica: &ReplicaRef,
    ) -> super::Result<
        Option<kuberic_runtime::protocol::public_operations::PublicInboundBuildAuthority>,
    > {
        self.check().await?;
        if replica.cluster != self.nonce {
            return Err(TestError::new(
                ErrorKind::InvalidReference,
                "replica belongs to another cluster",
            ));
        }
        let handle = self
            .core
            .supervisor
            .lock()
            .await
            .hosts
            .get(replica.instance_id())
            .and_then(|host| host.running.as_ref())
            .map(RunningReplica::handle)
            .ok_or_else(|| TestError::new(ErrorKind::InvalidReference, "host missing"))?;
        if handle.process_session_id() != replica.session {
            return Err(TestError::new(
                ErrorKind::InvalidReference,
                "replica process session changed",
            ));
        }
        tokio::time::timeout_at(
            self.core.deadline,
            handle.public_v1_inbound_build_authority(),
        )
        .await
        .map_err(anyhow::Error::from)?
        .map_err(anyhow::Error::from)
        .map_err(Into::into)
    }

    /// Observe only this exact replica over the production control RPC.
    /// Does not wait for unrelated replicas whose native copy may be held.
    pub async fn report_replica(
        &mut self,
        replica: &ReplicaRef,
    ) -> super::Result<PublicV1AgentReport> {
        self.check().await?;
        if replica.cluster != self.nonce {
            return Err(TestError::new(
                ErrorKind::InvalidReference,
                "replica belongs to another cluster",
            ));
        }
        let report = self.core.report_uid(replica.instance_id()).await?;
        if self.reference(&report) != *replica {
            return Err(TestError::new(
                ErrorKind::InvalidReference,
                "replica scope changed",
            ));
        }
        Ok(report)
    }

    /// Persist a frozen terminal intent, then use production controller dispatch.
    /// Ordinary topology reconciliation does not originate Close/Abort commands.
    pub async fn dispatch_terminal(
        &mut self,
        replica: &ReplicaRef,
        kind: kuberic_runtime::protocol::public_operations::PublicV1CallKind,
    ) -> super::Result<OperationDispatch> {
        use crate::cluster_api::{PublicV1OperationApi, PublicV1Scope};
        use crate::evaluator::public_v1_evidence::{
            PublicV1AcceptedTransitionIntent, PublicV1AcceptedTransitionStatus,
            PublicV1EvidenceConsumer, PublicV1ResourceAuthority,
        };
        use kuberic_runtime::protocol::command::PublicV1OperationCommand;
        use kuberic_runtime::protocol::public_operations::PublicV1CallKind;
        use kuberic_runtime::protocol::types::{OperationId, ResourceUid};

        self.driving()?;
        self.check().await?;
        let program = match kind {
            PublicV1CallKind::Close => PublicOperationProgram::Close,
            PublicV1CallKind::Abort => PublicOperationProgram::Abort,
            _ => {
                return Err(TestError::new(
                    ErrorKind::InvalidConfiguration,
                    "terminal dispatch requires Close or Abort",
                ));
            }
        };
        let report = self.validate(replica).await?;
        self.core.cancel_controller_call();
        let set = self.core.set().await?;
        let authority = set
            .status
            .as_ref()
            .and_then(|status| status.public_v1_authority.as_ref())
            .context("accepted authority missing")?
            .authority()
            .map_err(anyhow::Error::msg)?;
        let pod: Pod = self
            .core
            .api
            .snapshot(Kind::Pod)
            .into_iter()
            .map(serde_json::from_value)
            .collect::<std::result::Result<Vec<Pod>, _>>()
            .map_err(anyhow::Error::from)?
            .into_iter()
            .find(|pod| pod.uid().as_deref() == Some(replica.instance_id()))
            .context("terminal target Pod missing")?;
        let handle = self
            .core
            .supervisor
            .lock()
            .await
            .hosts
            .get(replica.instance_id())
            .and_then(|host| host.running.as_ref())
            .map(RunningReplica::handle)
            .context("terminal target host missing")?;
        let revision = handle
            .public_v1_next_operation_revision()
            .await
            .map_err(anyhow::Error::from)?;
        let intent = PublicOperationIntent {
            identity: report.identity,
            operation_id: OperationId::new(format!("terminal-{}", uuid::Uuid::new_v4())),
            revision,
            process_session_id: report.process_session_id.clone(),
            class: if kind == PublicV1CallKind::Close {
                kuberic_runtime::protocol::public_operations::PublicOperationClass::Close
            } else {
                kuberic_runtime::protocol::public_operations::PublicOperationClass::Abort
            },
            input_digest: program.digest(),
            replay_disposition: program.replay_disposition(),
            lifecycle: None,
            program: Some(program),
        };
        let transition = PublicV1AcceptedTransitionStatus {
            resource_version: set
                .resource_version()
                .context("set resource version missing")?,
            evidence: authority.clone(),
            intent: PublicV1AcceptedTransitionIntent {
                consumer: PublicV1EvidenceConsumer::Removal,
                operation_id: intent.operation_id.clone(),
                revision,
                call_kind: kind,
                history: report.history,
                source: PublicV1ResourceAuthority {
                    storage_incarnation: report.storage_incarnation,
                    resource_version: pod
                        .resource_version()
                        .context("Pod resource version missing")?,
                },
                target: None,
                process_session_id: report.process_session_id,
                input_digest: intent.input_digest.clone(),
                replay_disposition: intent.replay_disposition,
                previous_configuration_id: None,
                current_configuration_id: None,
                catch_up_mode: None,
                catch_up_specific_quorum: None,
            },
        };
        let command = PublicV1OperationCommand {
            target: replica.identity.clone(),
            intent,
        };
        let api = KubeClusterApi::new(
            self.core.api.client(),
            Arc::new(GrpcAgentApi::new(pod::RPC_DEADLINE)),
            "local-test-token",
            self.core.namespace.as_str(),
        )
        .map_err(anyhow::Error::from)?;
        let scope = PublicV1Scope {
            namespace: self.core.namespace.clone(),
            name: self.core.name.clone(),
            resource_uid: ResourceUid::new(replica.resource_uid.as_str()),
        };
        tokio::time::timeout_at(
            self.core.deadline,
            api.begin_public_v1_operation(&scope, &authority, &transition, &command),
        )
        .await
        .map_err(anyhow::Error::from)?
        .map_err(anyhow::Error::msg)?;
        let evidence = OperationEvidence::new(self.core.set().await?);
        self.redispatch_operation(&evidence).await
    }

    /// Redispatch only the same persisted program; never call an engine directly.
    pub async fn redispatch_operation(
        &mut self,
        evidence: &OperationEvidence,
    ) -> super::Result<OperationDispatch> {
        use crate::cluster_api::{PublicV1OperationApi, PublicV1Scope};
        use kuberic_runtime::protocol::command::ProtocolCommand;

        self.driving()?;
        self.check().await?;
        self.core.cancel_controller_call();
        let mut expected = evidence
            .transition()
            .transition()
            .map_err(anyhow::Error::msg)?;
        let command = evidence.operation().command().map_err(anyhow::Error::msg)?;
        if evidence.set().uid() != self.core.set().await?.uid() {
            return Err(TestError::new(
                ErrorKind::InvalidReference,
                "operation evidence is not for this cluster",
            ));
        }
        let api = KubeClusterApi::new(
            self.core.api.client(),
            Arc::new(GrpcAgentApi::new(pod::RPC_DEADLINE)),
            "local-test-token",
            self.core.namespace.as_str(),
        )
        .map_err(anyhow::Error::from)?;
        let scope = PublicV1Scope {
            namespace: self.core.namespace.clone(),
            name: self.core.name.clone(),
            resource_uid: expected.intent.history.resource_uid.clone(),
        };
        let current = api
            .observe_public_v1_operation(&scope)
            .await
            .map_err(anyhow::Error::msg)?
            .context("persisted operation disappeared")?;
        if current.intent != expected.intent || current.evidence != expected.evidence {
            return Err(TestError::new(
                ErrorKind::InvalidReference,
                "persisted operation authority changed before exact redispatch",
            ));
        }
        // Status persistence advances resourceVersion; retain frozen authority
        // but use the same fresh CAS observation as the production reconciler.
        expected.resource_version = current.resource_version;
        let dispatch_result = tokio::time::timeout_at(
            self.core.deadline,
            api.dispatch_public_v1_operation(
                &scope,
                &expected,
                &ProtocolCommand::PublicV1Operation(Box::new(command.clone())),
            ),
        )
        .await
        .map_err(anyhow::Error::from)?;
        let report = self
            .core
            .report_uid(command.target.instance_id.as_str())
            .await?;
        let handle = self
            .core
            .supervisor
            .lock()
            .await
            .hosts
            .get(command.target.instance_id.as_str())
            .and_then(|host| host.running.as_ref())
            .map(RunningReplica::handle)
            .context("terminal host missing")?;
        let contained = handle
            .public_v1_operation_is_contained(&command.intent.operation_id)
            .await
            .map_err(anyhow::Error::from)?;
        Ok(OperationDispatch {
            evidence: evidence.clone(),
            dispatch_result,
            report,
            contained,
        })
    }

    pub async fn recreate_controller_at(
        &mut self,
        barrier: Barrier,
    ) -> super::Result<OperationEvidence> {
        let pause = self.pause_at(barrier).await?;
        self.recreate_controller().await?;
        self.resume(pause).await
    }
    fn network_error(error: anyhow::Error) -> TestError {
        let kind = if error.is::<super::transport::FaultAlreadyActive>() {
            ErrorKind::FaultAlreadyActive
        } else if error.is::<tokio::time::error::Elapsed>() {
            ErrorKind::Deadline
        } else if error.is::<super::transport::InvalidIsolation>() {
            ErrorKind::InvalidReference
        } else {
            ErrorKind::Fatal
        };
        TestError::with_source(kind, "transport fault operation failed", error)
    }
    pub async fn isolate(&mut self, replicas: &[ReplicaRef]) -> super::Result<Isolation> {
        self.check().await?;
        if replicas.is_empty() {
            return Err(TestError::new(
                ErrorKind::InvalidReference,
                "isolation group is empty",
            ));
        }
        for reference in replicas {
            self.validate(reference).await?;
            if !kuberic_runtime::replicator::TestingTcpRoute::supports(
                &reference.session,
                &reference.creation,
            ) {
                return Err(TestError::new(
                    ErrorKind::UnsupportedCapability,
                    "selected engine did not acknowledge built-in routed transport",
                ));
            }
        }
        let members = replicas
            .iter()
            .map(|reference| reference.instance_id().to_owned())
            .collect::<Vec<_>>();
        tokio::time::timeout_at(self.core.deadline, self.core.network.isolate(&members))
            .await
            .map_err(anyhow::Error::from)?
            .map(|isolation| isolation.with_deadline(self.core.deadline))
            .map_err(Self::network_error)
    }
    pub async fn arm_next_build_dial(
        &mut self,
        resource: &ResourceRef,
    ) -> super::Result<Isolation> {
        self.check().await?;
        let value = self.validate_resource(resource)?;
        if resource.kind != ResourceKind::Pod || !value["metadata"]["deletionTimestamp"].is_null() {
            return Err(TestError::new(
                ErrorKind::InvalidReference,
                "Build fault requires a current non-deleting Pod",
            ));
        }
        let report = self
            .core
            .report_uid(resource.uid())
            .await
            .map_err(|source| {
                TestError::with_source(
                    ErrorKind::UnsupportedCapability,
                    "Build participant has no acknowledged live routed engine",
                    source,
                )
            })?;
        if !kuberic_runtime::replicator::TestingTcpRoute::supports(
            &report.process_session_id,
            &report.replicator_creation_id,
        ) {
            return Err(TestError::new(
                ErrorKind::UnsupportedCapability,
                "Build participant did not acknowledge built-in routed transport",
            ));
        }
        tokio::time::timeout_at(
            self.core.deadline,
            self.core.network.arm_next_build_dial(resource.uid()),
        )
        .await
        .map_err(anyhow::Error::from)?
        .map(|isolation| isolation.with_deadline(self.core.deadline))
        .map_err(Self::network_error)
    }
    pub async fn heal(&mut self, isolation: Isolation) -> super::Result<()> {
        tokio::time::timeout_at(self.core.deadline, self.core.network.heal(isolation))
            .await
            .map_err(anyhow::Error::from)?
            .map_err(Self::network_error)
    }
    pub async fn finish<E: Into<anyhow::Error>>(
        self,
        result: std::result::Result<(), E>,
    ) -> super::Result<()> {
        self.core
            .finish(result.map_err(Into::into))
            .await
            .map_err(|error| match error.downcast::<TestError>() {
                Ok(error) => error,
                Err(error) => {
                    TestError::with_source(ErrorKind::Cleanup, "finalization failed", error)
                }
            })
    }
}

pub(super) fn redact(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(object) => {
            if object.get("kind").and_then(serde_json::Value::as_str) == Some("Secret") {
                object.remove("data");
                object.remove("stringData");
            }

            if object
                .get("name")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|name| {
                    name.contains("TOKEN") || name.contains("SECRET") || name.contains("CREDENTIAL")
                })
            {
                object.remove("value");
            }
            for (key, value) in object {
                if (key.to_ascii_lowercase().contains("token") || key == "stringData")
                    && value.is_string()
                {
                    *value = serde_json::Value::String("[redacted]".into());
                } else {
                    redact(value);
                }
            }
        }
        serde_json::Value::Array(values) => values.iter_mut().for_each(redact),
        _ => {}
    }
}

#[cfg(test)]
mod readiness_lifecycle_tests {
    use super::super::tests::EmptyFactory;
    use super::*;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn lifecycle_joins_the_owned_controller_call_before_stopping_its_host() -> Result<()> {
        let mut cluster = TestCluster::builder(EmptyFactory)
            .scenario_timeout(Duration::from_secs(30))
            .start()
            .await?;
        let result: Result<()> = async {
            let topology = cluster.settle().await?;
            let primary = topology.primary().clone();
            let supervisor = cluster.core.supervisor.clone();
            let (started_tx, started) = tokio::sync::oneshot::channel();
            let (release, released) = tokio::sync::oneshot::channel();
            let mut changed = Box::pin(cluster.core.api.changed().notified_owned());
            changed.as_mut().enable();
            *cluster.core.controller_call.get_mut() = Some(ControllerCall {
                future: Box::pin(async move {
                    started_tx.send(()).unwrap();
                    released.await.unwrap();
                    Ok(crate::reconciler::ReconcileAction {
                        requeue_after: Duration::from_secs(60),
                        kind: crate::reconciler::ReconcileKind::Stable,
                    })
                }),
                changed,
            });
            cluster
                .drive(async {
                    started.await?;
                    Ok::<_, anyhow::Error>(())
                })
                .await?;
            {
                let stopping = cluster.stop(&primary);
                tokio::pin!(stopping);
                assert!(futures::poll!(stopping.as_mut()).is_pending());
                let state = supervisor.lock().await;
                let host = state
                    .hosts
                    .get(primary.instance_id())
                    .context("host stopped before its owned controller call completed")?;
                assert!(!*host.running.as_ref().unwrap().shutdown_signal().borrow());
                drop(state);
                release.send(()).unwrap();
                stopping.await?;
            }
            assert!(cluster.core.controller_call.get_mut().is_none());
            assert!(
                cluster.core.requeue.get_mut().is_some(),
                "lifecycle completion discarded the existing requeue policy"
            );
            Ok(())
        }
        .await;
        cluster.finish(result).await?;
        Ok(())
    }

    #[tokio::test]
    async fn fatal_task_failure_is_latched_and_keeps_an_observed_application_error() -> Result<()> {
        let mut cluster = TestCluster::builder(EmptyFactory).start().await?;
        let supervisor = cluster.core.supervisor_task.as_ref().unwrap();
        supervisor.abort();
        while !supervisor.is_finished() {
            tokio::task::yield_now().await;
        }
        let application = TestError::with_source(
            ErrorKind::Application,
            "simultaneous application failure",
            anyhow!("application-source"),
        );
        let error = cluster
            .checked_application_result(Err::<(), _>(application))
            .await
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Fatal);
        let sources = error.sources().collect::<Vec<_>>();
        assert_eq!(sources.len(), 2);
        assert!(
            sources[0]
                .source()
                .unwrap()
                .to_string()
                .contains("application-source")
        );
        assert!(
            sources[1]
                .source()
                .unwrap()
                .to_string()
                .contains("supervisor")
        );
        assert_eq!(
            cluster
                .probe(async { Ok::<_, anyhow::Error>(()) })
                .await
                .unwrap_err()
                .kind(),
            ErrorKind::Fatal
        );
        assert!(cluster.finish(Ok::<_, anyhow::Error>(())).await.is_err());
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn drive_retains_the_committed_controller_reply_until_the_next_drive() -> Result<()> {
        let mut cluster = TestCluster::builder(EmptyFactory)
            .scenario_timeout(Duration::from_secs(30))
            .start()
            .await?;
        let result: Result<()> = async {
            let topology = cluster.settle().await?;
            cluster
                .submit_switchover(topology.secondaries().next().unwrap())
                .await?;
            let mut barrier = cluster.core.api.arm_operation_barrier(NAME)?;
            let persisted = cluster.drive(barrier.entered()).await?;
            let operation: crate::crd::PublicV1OperationStatus =
                serde_json::from_value(persisted["status"]["publicV1Operation"].clone())?;
            let command = operation.command().map_err(anyhow::Error::msg)?;
            assert!(
                cluster.core.controller_call.get_mut().is_some(),
                "application completion discarded the in-flight controller call"
            );
            barrier.release();
            cluster.step().await?;
            assert!(cluster.core.controller_call.get_mut().is_none());
            let report = cluster
                .core
                .report_uid(command.target.instance_id.as_str())
                .await?;
            assert!(
                !report
                    .completed_operations
                    .iter()
                    .any(|completion| completion.operation_id == command.intent.operation_id),
                "resuming a held status reply started a second reconcile instead"
            );
            Ok(())
        }
        .await;
        cluster.finish(result).await?;
        Ok(())
    }

    #[tokio::test]
    async fn drive_retains_requeue_ownership_without_starting_another_reconcile() -> Result<()> {
        fn require_send_sync<T: Send + Sync>() {}
        fn require_send<T: Send>(_: &T) {}
        require_send_sync::<TestCluster<EmptyFactory>>();
        struct Guard(Arc<std::sync::atomic::AtomicBool>);
        impl Drop for Guard {
            fn drop(&mut self) {
                self.0.store(true, std::sync::atomic::Ordering::Release);
            }
        }
        let mut cluster = TestCluster::builder(EmptyFactory)
            .scenario_timeout(Duration::from_secs(5))
            .start()
            .await?;
        let unpolled = cluster.settle();
        require_send(&unpolled);
        drop(unpolled);
        let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let guard = Guard(dropped.clone());
        let (started_tx, started) = tokio::sync::oneshot::channel();
        let (release, released) = tokio::sync::oneshot::channel();
        *cluster.core.requeue.get_mut() = Some(Box::pin(async move {
            let _guard = guard;
            started_tx.send(()).unwrap();
            released.await.unwrap();
        }));
        let result: Result<()> = async {
            cluster
                .drive(async {
                    started.await?;
                    Ok::<_, anyhow::Error>(())
                })
                .await?;
            assert!(
                !dropped.load(std::sync::atomic::Ordering::Acquire),
                "application completion cancelled the requeue owner"
            );
            release.send(()).unwrap();
            cluster.step().await?;
            assert!(dropped.load(std::sync::atomic::Ordering::Acquire));
            assert!(
                cluster.core.set().await?.status.is_none(),
                "resuming the requeue started an unintended reconciliation"
            );
            Ok(())
        }
        .await;
        cluster.finish(result).await?;
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn readiness_rejects_real_pending_replacement_and_renewal_after_convergence() -> Result<()>
    {
        let mut core = Harness::new(
            EmptyFactory,
            NAMESPACE.into(),
            NAME.into(),
            "in-process/empty".into(),
            Duration::from_secs(90),
            Duration::from_secs(15),
            3,
        )
        .await?;
        let result = async {
            core.ready()
                .await
                .context("initial readiness convergence")?;
            let predecessor = core.report(2).await?;
            let pod = core
                .api
                .snapshot(Kind::Pod)
                .into_iter()
                .find(|pod| {
                    pod["metadata"]["uid"].as_str()
                        == Some(predecessor.replica.instance_id.as_str())
                })
                .context("Pod missing")?;
            Api::<Pod>::namespaced(core.api.client(), NAMESPACE)
                .delete(
                    pod["metadata"]["name"].as_str().unwrap(),
                    &::kube::api::DeleteParams {
                        preconditions: Some(::kube::api::Preconditions {
                            uid: Some(predecessor.replica.instance_id.to_string()),
                            resource_version: None,
                        }),
                        ..Default::default()
                    },
                )
                .await?;
            let pending_replacement = loop {
                if let Some(replacement) = core
                    .set()
                    .await?
                    .status
                    .and_then(|status| status.public_v1_replacement)
                {
                    break replacement;
                }
                core.step().await?;
            };
            core.ready()
                .await
                .context("replacement readiness convergence")?;
            let replacement = core.report(2).await?;
            core.stop(replacement.replica.instance_id.as_str()).await?;
            core.supervisor
                .lock()
                .await
                .stopped
                .remove(replacement.replica.instance_id.as_str());
            let pending_renewal = loop {
                if let Some(renewal) = core
                    .set()
                    .await?
                    .status
                    .and_then(|status| status.public_v1_session_renewal)
                {
                    break renewal;
                }
                core.step().await?;
            };
            core.ready()
                .await
                .context("renewed-session readiness convergence")?;
            let status = core.set().await?.status.context("status missing")?;
            let mut reports = Vec::new();
            for id in 1..=3 {
                reports.push(core.report(id).await?);
            }
            assert!(Harness::<EmptyFactory>::settled(&status, &reports, 3));
            let mut altered = status.clone();
            altered.public_v1_replacement = Some(pending_replacement);
            assert!(!Harness::<EmptyFactory>::settled(&altered, &reports, 3));
            altered = status;
            altered.public_v1_session_renewal = Some(pending_renewal);
            assert!(!Harness::<EmptyFactory>::settled(&altered, &reports, 3));
            Ok(())
        }
        .await;
        core.finish(result).await
    }
}
