use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail, ensure};
use k8s_openapi::api::core::v1::{PersistentVolumeClaim, Pod, PodStatus, Secret};
use kube::{Api, ResourceExt, api::PostParams};
use kuberic_controller::cluster_api::{AgentApi, ClusterApi, GrpcAgentApi, KubeClusterApi};
use kuberic_controller::crd::{
    CONTROL_ADDRESS_ANNOTATION, KubericSet, KubericSetSpec, KubericSetStatus,
};
use kuberic_controller::reconciler::Reconciler;
use kuberic_runtime::host::{
    ApplicationStorageState, KubernetesDnsResolver, ReplicaHost, RunningReplica,
};
use kuberic_runtime::protocol::public_operations::{
    ProcessSessionRenewalRequired, PublicOperationIntent, PublicOperationProgram,
    PublicV1AcceptedOperationCompletion, PublicV1AgentReport, PublicV1OperationCompletion,
    PublicV1OperationResult,
};
use kuberic_runtime::protocol::types::{AccessStatus, ReplicaRole};
use kvstore2::{persistence::KvPersistence, service::KvService, state::CopyGate};
use tokio::sync::{Mutex, watch};
use tokio::task::JoinHandle;
use tokio::time::Instant;

use super::{
    kube::{ApiServer as TestApi, Kind},
    pod,
    transport::Network,
};

const NAMESPACE: &str = "in-process";
const NAME: &str = "sample";
const TIMEOUT: Duration = Duration::from_secs(150);
#[derive(Clone)]
pub(super) struct ConfigurationEvidence {
    pub operations: BTreeMap<String, (PublicOperationIntent, PublicV1OperationCompletion)>,
    renewals: Vec<ProcessSessionRenewalRequired>,
}

struct Hosted {
    service: Arc<KvService>,
    startup_shutdown: watch::Sender<bool>,
    startup: Option<JoinHandle<kuberic_runtime::host::Result<Option<RunningReplica>>>>,
    running: Option<RunningReplica>,
    data_root: PathBuf,
    _route: kuberic_runtime::replicator::stateless::TestingTcpRoute,
    network: Network,
    uid: String,
}

impl Hosted {
    async fn stop(mut self) -> Result<()> {
        self.startup_shutdown.send_replace(true);
        let mut errors = Vec::new();
        if let Some(startup) = self.startup.take() {
            match startup.await {
                Ok(Ok(running)) => self.running = running,
                Ok(Err(error)) => errors.push(error.to_string()),
                Err(error) => errors.push(error.to_string()),
            }
        }
        if let Some(mut running) = self.running.take() {
            running.shutdown();
            if let Err(error) = running.wait().await {
                errors.push(error.to_string());
            }
        }
        if let Err(error) = self.service.shutdown_and_drain().await {
            errors.push(error.to_string());
        }
        if let Err(error) = self.network.forget(&self.uid).await {
            errors.push(error.to_string());
        }
        ensure!(errors.is_empty(), "host cleanup: {}", errors.join("; "));
        Ok(())
    }
}

impl Drop for Hosted {
    fn drop(&mut self) {
        self.startup_shutdown.send_replace(true);
        if let Some(running) = &self.running {
            running.shutdown();
        }
    }
}

#[derive(Default)]
struct SupervisorState {
    hosts: BTreeMap<String, Hosted>,
    stopped: BTreeSet<String>,
    held_deletions: BTreeSet<String>,
}

pub(super) struct Harness {
    pub api: TestApi,
    supervisor: Arc<Mutex<SupervisorState>>,
    supervisor_shutdown: watch::Sender<bool>,
    supervisor_task: Option<JoinHandle<Result<()>>>,
    pub reconciler: Arc<Reconciler>,
    root: Option<tempfile::TempDir>,
    deadline: Instant,
    pub reconcile_errors: usize,
    last_error: Option<String>,
    cleaned: bool,
    network: Network,
    renewals: std::sync::Mutex<BTreeMap<String, ProcessSessionRenewalRequired>>,
}

impl Harness {
    pub async fn new() -> Result<Self> {
        let api = TestApi::new(NAMESPACE);
        let reconciler = Self::reconciler(&api)?;
        let root = tempfile::tempdir()?;
        let supervisor = Arc::new(Mutex::new(SupervisorState::default()));
        let set = KubericSet::new(
            NAME,
            KubericSetSpec {
                replicas: 3,
                image: pod::IMAGE.into(),
                failover_delay_seconds: 1,
                switchover: None,
            },
        );
        Api::<KubericSet>::namespaced(api.client(), NAMESPACE)
            .create(&PostParams::default(), &set)
            .await?;
        let (supervisor_shutdown, mut shutdown) = watch::channel(false);
        let task_api = api.clone();
        let task_state = supervisor.clone();
        let task_root = root.path().to_owned();
        let network = Network::default();
        let task_network = network.clone();
        let supervisor_task = tokio::spawn(async move {
            let result = loop {
                if *shutdown.borrow() || shutdown.has_changed().is_err() {
                    break Ok(());
                }
                if let Err(error) =
                    supervise(&task_api, &task_state, &task_root, &task_network).await
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
            deadline: Instant::now() + TIMEOUT,
            reconcile_errors: 0,
            last_error: None,
            cleaned: false,
            network,
            renewals: std::sync::Mutex::new(BTreeMap::new()),
        })
    }

    fn reconciler(api: &TestApi) -> Result<Arc<Reconciler>> {
        let agents = Arc::new(GrpcAgentApi::new(pod::RPC_DEADLINE));
        let cluster = KubeClusterApi::new(api.client(), agents, "local-test-token", NAMESPACE)?;
        Ok(Arc::new(Reconciler::new(
            Arc::new(cluster),
            kuberic_controller::production_evaluation_config(1, 1, 1),
        )))
    }

    pub fn recreate_controller(&mut self) -> Result<()> {
        self.reconciler = Self::reconciler(&self.api)?;
        Ok(())
    }

    pub async fn set(&self) -> Result<KubericSet> {
        let set = Api::<KubericSet>::namespaced(self.api.client(), NAMESPACE)
            .get(NAME)
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

    async fn check(&mut self) -> Result<()> {
        self.api.check()?;
        if self
            .supervisor_task
            .as_ref()
            .is_some_and(JoinHandle::is_finished)
        {
            let task = self.supervisor_task.take().unwrap();
            match task.await {
                Ok(Err(error)) => return Err(error.context("supervisor failed")),
                Err(error) => bail!("supervisor panicked: {error}"),
                Ok(Ok(())) => bail!("supervisor stopped unexpectedly"),
            }
        }
        ensure!(
            Instant::now() < self.deadline,
            "scenario deadline; errors={}; last reconcile error: {:?}; resources: {}",
            self.reconcile_errors,
            self.last_error,
            self.diagnostic_snapshot()
        );
        Ok(())
    }

    pub async fn step(&mut self) -> Result<()> {
        self.check().await?;
        let changed = self.api.changed();
        let notification = changed.notified();
        tokio::pin!(notification);
        notification.as_mut().enable();
        let outcome =
            tokio::time::timeout_at(self.deadline, self.reconciler.reconcile(NAMESPACE, NAME))
                .await
                .map_err(|error| {
                    anyhow!(
                        "reconcile deadline: {error}; errors={}; last={:?}; state={}",
                        self.reconcile_errors,
                        self.last_error,
                        self.diagnostic_snapshot()
                    )
                })?;
        let (delay, wake_on_change) = match outcome {
            Ok(action) => (action.requeue_after, true),
            Err(error) => {
                self.reconcile_errors += 1;
                if self.reconcile_errors <= 10 {
                    eprintln!("reconcile error {}: {error}", self.reconcile_errors);
                }
                self.last_error = Some(error.to_string());
                (Duration::from_millis(20), false)
            }
        };
        self.check().await?;
        if delay.is_zero() {
            tokio::task::yield_now().await;
        } else {
            tokio::select! {
                _ = notification, if wake_on_change => {}
                _ = tokio::time::sleep(delay.min(self.deadline.saturating_duration_since(Instant::now()))) => {}
            }
        }
        Ok(())
    }

    fn diagnostic_snapshot(&self) -> serde_json::Value {
        let mut objects = snapshot(&self.api);
        for object in &mut objects {
            if object["kind"] == "Secret" {
                object.as_object_mut().unwrap().remove("data");
            }
        }
        serde_json::json!(objects)
    }

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

    pub async fn service(&self, replica_id: i64) -> Result<Arc<KvService>> {
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
            .context("replica is not in topology")?;
        self.supervisor
            .lock()
            .await
            .hosts
            .get(member.identity.instance_id.as_str())
            .map(|host| host.service.clone())
            .context("host missing")
    }

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

    pub async fn service_uid(&self, uid: &str) -> Result<Arc<KvService>> {
        self.supervisor
            .lock()
            .await
            .hosts
            .get(uid)
            .map(|host| host.service.clone())
            .context("host missing")
    }

    pub async fn isolate_uid(&self, uid: &str, isolated: bool) -> Result<()> {
        ensure!(
            self.live_uids().await.iter().any(|live| live == uid),
            "isolation target not live"
        );
        self.network.isolate(uid, isolated).await?;
        ensure!(
            self.live_uids().await.iter().any(|live| live == uid),
            "isolation stopped host"
        );
        Ok(())
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
                let Some(intent) = handle
                    .public_v1_operation_intent(&completion.operation_id)
                    .await?
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
                    && intent.operation_id.to_string()
                        == format!(
                            "{}-configuration-{}",
                            required.operation_id, session.identity.replica_id
                        )
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
        let replica_id = pod["metadata"]["labels"][kuberic_controller::crd::REPLICA_ID_LABEL]
            .as_str()
            .context("replica label missing")?
            .parse()?;
        let report = GrpcAgentApi::new(pod::RPC_DEADLINE)
            .get_status(
                endpoint,
                "local-test-token",
                kuberic_runtime::control::proto::GetAgentStatusRequest {
                    protocol_version: kuberic_runtime::protocol::PROTOCOL_VERSION,
                    resource_uid: set.uid().context("set UID missing")?,
                    replica_id,
                    expected_instance_id: uid.to_owned(),
                },
            )
            .await
            .map_err(|error| anyhow!("agent report: {error:?}"))?;
        let report = kuberic_runtime::control::normalize_public_v1_agent_status_report(report)
            .map_err(|error| anyhow!("{error}"))?;
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
                || report
                    .replicator_creation_id
                    .as_str()
                    .starts_with("pending-")
                || report.role != session.role
                || report.read_status != AccessStatus::Granted
                || (report.write_status == AccessStatus::Granted)
                    != (session.role == ReplicaRole::Primary)
                || report.progress.first > report.progress.last
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

    pub async fn write(&mut self, key: &str, value: &str) -> Result<i64> {
        let primary = self.ready().await?;
        Ok(self
            .service(primary)
            .await?
            .replicate_put(key.into(), value.into())
            .await?)
    }

    pub async fn assert_values(&mut self, key: &str, value: &str) -> Result<()> {
        self.assert_values_on_members(key, value, 3).await
    }

    pub async fn assert_values_on_members(
        &mut self,
        key: &str,
        value: &str,
        expected: usize,
    ) -> Result<()> {
        loop {
            self.ready_members(expected).await?;
            let set = self.set().await?;
            let configuration = set
                .status
                .as_ref()
                .and_then(|status| status.public_v1_topology.as_ref())
                .context("topology missing")?
                .configuration()
                .map_err(|error| anyhow!("{error}"))?;
            let mut matching = 0;
            for member in &configuration.members {
                if self
                    .service(member.identity.replica_id.value())
                    .await?
                    .get(key)
                    .await?
                    .as_deref()
                    == Some(value)
                {
                    matching += 1;
                }
            }
            if matching == expected {
                self.check().await?;
                return Ok(());
            }
            self.step().await?;
        }
    }

    pub async fn stop(&self, replica_id: i64, restart: bool) -> Result<()> {
        let set = self.set().await?;
        let configuration = set
            .status
            .as_ref()
            .and_then(|status| status.public_v1_topology.as_ref())
            .context("topology missing")?
            .configuration()
            .map_err(|error| anyhow!("{error}"))?;
        let uid = configuration
            .members
            .iter()
            .find(|member| member.identity.replica_id.value() == replica_id)
            .context("member missing")?
            .identity
            .instance_id
            .to_string();
        let mut state = self.supervisor.lock().await;
        state.stopped.insert(uid.clone());
        let host = state.hosts.remove(&uid).context("host missing")?;
        tokio::time::timeout(Duration::from_secs(15), host.stop()).await??;
        if restart {
            state.stopped.remove(&uid);
        }
        Ok(())
    }

    pub async fn hold_deletion(&self, uid: &str, hold: bool) {
        let mut state = self.supervisor.lock().await;
        if hold {
            state.held_deletions.insert(uid.into());
        } else {
            state.held_deletions.remove(uid);
        }
    }

    pub fn transport(&self) -> super::transport::Network {
        self.network.clone()
    }

    pub async fn finish(mut self, result: Result<()>) -> Result<()> {
        let result = if result.is_ok() {
            self.check().await
        } else {
            result
        };
        if result.is_err() {
            let diagnostic = async {
                let api = KubeClusterApi::new(
                    self.api.client(),
                    Arc::new(GrpcAgentApi::new(Duration::from_secs(1))),
                    "local-test-token",
                    NAMESPACE,
                )?;
                let observation = api.observe(NAMESPACE, NAME).await?;
                let summaries = observation.agents.into_iter().map(|(key, observation)| {
                    let status = match observation {
                        kuberic_controller::observation::RawAgentObservation::PublicV1Report(report) => {
                            serde_json::json!({
                                "role": report.role,
                                "read": report.read_status,
                                "write": report.write_status,
                                "progress": report.progress,
                                "session": report.process_session_id,
                            })
                        }
                        other => serde_json::json!({"observation": format!("{other:?}")}),
                    };
                    serde_json::json!({"replica": key.replica_id, "instance": key.instance_id, "status": status})
                }).collect::<Vec<_>>();
                Ok::<_, anyhow::Error>(serde_json::json!(summaries))
            };
            match tokio::time::timeout(Duration::from_secs(5), diagnostic).await {
                Ok(Ok(summary)) => eprintln!("agent diagnostics: {summary}"),
                Ok(Err(error)) => eprintln!("agent diagnostics failed: {error:#}"),
                Err(error) => eprintln!("agent diagnostics timed out: {error}"),
            }
        }
        self.supervisor_shutdown.send_replace(true);
        let mut errors = result
            .err()
            .into_iter()
            .map(|error| format!("{error:#}"))
            .collect::<Vec<_>>();
        if let Some(task) = self.supervisor_task.take() {
            let mut task = task;
            match tokio::time::timeout(Duration::from_secs(20), &mut task).await {
                Ok(Ok(Ok(()))) => {}
                Ok(Ok(Err(error))) => errors.push(error.to_string()),
                Ok(Err(error)) => errors.push(error.to_string()),
                Err(error) => {
                    task.abort();
                    let _ = task.await;
                    errors.push(format!("supervisor teardown timed out: {error}"));
                }
            }
        }
        if let Err(error) = self.network.finish().await {
            errors.push(error.to_string());
        }
        let hosts = {
            let mut state = self.supervisor.lock().await;
            state.held_deletions.clear();
            std::mem::take(&mut state.hosts)
        };
        for (_, host) in hosts {
            match tokio::time::timeout(Duration::from_secs(15), host.stop()).await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => errors.push(error.to_string()),
                Err(error) => errors.push(format!("host teardown timed out: {error}")),
            }
        }
        if let Err(error) = self.api.check() {
            errors.push(error.to_string());
        }
        self.cleaned = true;
        if errors.is_empty() {
            Ok(())
        } else {
            let path = self.root.take().unwrap().keep();
            bail!(
                "{}; retained storage: {}",
                errors.join("; "),
                path.display()
            )
        }
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.supervisor_shutdown.send_replace(true);
        if !self.cleaned
            && let Some(root) = self.root.take()
        {
            eprintln!(
                "in-process harness dropped before cleanup; retained storage: {}",
                root.keep().display()
            );
        }
    }
}

async fn supervise(
    api: &TestApi,
    state: &Mutex<SupervisorState>,
    root: &std::path::Path,
    network: &Network,
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
                tokio::time::timeout(Duration::from_secs(15), host.stop()).await??;
            }
            if !state.held_deletions.contains(&uid) {
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
        let config = pod::resolve(pod, &pvcs, &secrets, root)?;
        let control_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let backend = format!("http://{}", control_listener.local_addr()?);
        let control = network.route("controller", &uid, &backend).await?;
        let replication = format!("http://{}", config.replication_address);
        let app_path = config.data_root.join("application");
        let storage = if KvPersistence::is_fresh_empty(&app_path)? {
            ApplicationStorageState::FreshEmpty
        } else {
            ApplicationStorageState::Established
        };
        let service = Arc::new(KvService::new(
            Arc::new(KvPersistence::open(app_path)?),
            replication,
            CopyGate::disabled(),
        ));
        let resolver = Arc::new(KubernetesDnsResolver::new(
            config.resource_uid.clone(),
            NAMESPACE,
        ));
        let data_root = config.data_root.clone();
        let (startup_shutdown, receiver) = watch::channel(false);
        let routed_uid = uid.clone();
        let routed_network = network.clone();
        let route = kuberic_runtime::replicator::stateless::TestingTcpRoute::register(
            uid.clone(),
            move |destination, endpoint| {
                let network = routed_network.clone();
                let uid = routed_uid.clone();
                Box::pin(async move {
                    network
                        .route(&uid, &destination, &endpoint)
                        .await
                        .map_err(std::io::Error::other)
                })
            },
        )?;
        let host = ReplicaHost::new(config, service.clone(), storage, resolver);
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
            if state.held_deletions.contains(&uid) {
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
            if !state.held_deletions.contains(uid) {
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
