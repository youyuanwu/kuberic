use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail, ensure};
use k8s_openapi::api::core::v1::{PersistentVolumeClaim, Pod, PodStatus, Secret};
use kube::{Api, ResourceExt, api::PostParams};
use kuberic_controller::cluster_api::{AgentApi, ClusterApi, GrpcAgentApi, KubeClusterApi};
use kuberic_controller::crd::{CONTROL_ADDRESS_ANNOTATION, KubericSet, KubericSetSpec};
use kuberic_controller::reconciler::Reconciler;
use kuberic_runtime::host::{
    ApplicationStorageState, KubernetesDnsResolver, ReplicaHost, RunningReplica,
};
use kuberic_runtime::protocol::types::{AccessStatus, ReplicaRole};
use kvstore2::{persistence::KvPersistence, service::KvService, state::CopyGate};
use tokio::sync::{Mutex, watch};
use tokio::task::JoinHandle;
use tokio::time::Instant;

use super::{
    kube::{ApiServer as TestApi, Kind},
    pod,
};

const NAMESPACE: &str = "in-process";
const NAME: &str = "sample";
const TIMEOUT: Duration = Duration::from_secs(150);

struct Hosted {
    service: Arc<KvService>,
    startup_shutdown: watch::Sender<bool>,
    startup: Option<JoinHandle<kuberic_runtime::host::Result<Option<RunningReplica>>>>,
    running: Option<RunningReplica>,
    data_root: PathBuf,
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
        let supervisor_task = tokio::spawn(async move {
            let result = loop {
                if *shutdown.borrow() || shutdown.has_changed().is_err() {
                    break Ok(());
                }
                if let Err(error) = supervise(&task_api, &task_state, &task_root).await {
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
        Ok(Api::<KubericSet>::namespaced(self.api.client(), NAMESPACE)
            .get(NAME)
            .await?)
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
        loop {
            self.check().await?;
            let set = self.set().await?;
            if let Some(status) = set.status.as_ref()
                && status.public_v1_transition.is_none()
                && status.public_v1_replacement.is_none()
                && status.public_v1_session_renewal.is_none()
                && let Some(topology) = status.public_v1_topology.as_ref()
            {
                let configuration = topology
                    .configuration()
                    .map_err(|error| anyhow!("{error}"))?;
                if configuration.members.len() == expected {
                    let mut granted = 0;
                    let mut admitted = 0;
                    let state = self.supervisor.lock().await;
                    for member in &configuration.members {
                        if let Some(host) = state.hosts.get(member.identity.instance_id.as_str())
                            && let Some(running) = &host.running
                        {
                            let _ = running;
                            let report = self.report(member.identity.replica_id.value()).await?;
                            if report.replica == member.identity && report.role == member.role {
                                if report.write_status == AccessStatus::Granted {
                                    granted += 1;
                                }
                                if report.role == ReplicaRole::Primary
                                    || report.read_status == AccessStatus::Granted
                                {
                                    admitted += 1;
                                }
                            }
                        }
                    }
                    if admitted == expected && granted == 1 {
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
        let pod = self
            .api
            .snapshot(Kind::Pod)
            .into_iter()
            .find(|pod| {
                pod["metadata"]["uid"].as_str() == Some(member.identity.instance_id.as_str())
            })
            .context("member Pod missing")?;
        let endpoint = pod["metadata"]["annotations"][CONTROL_ADDRESS_ANNOTATION]
            .as_str()
            .context("control address missing")?;
        let report = GrpcAgentApi::new(pod::RPC_DEADLINE)
            .get_status(
                endpoint,
                "local-test-token",
                kuberic_runtime::control::proto::GetAgentStatusRequest {
                    protocol_version: kuberic_runtime::protocol::PROTOCOL_VERSION,
                    resource_uid: set.uid().context("set UID missing")?,
                    replica_id,
                    expected_instance_id: member.identity.instance_id.to_string(),
                },
            )
            .await
            .map_err(|error| anyhow!("agent report: {error:?}"))?;
        kuberic_runtime::control::normalize_public_v1_agent_status_report(report)
            .map_err(|error| anyhow!("{error}"))
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
            match task.await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => errors.push(error.to_string()),
                Err(error) => errors.push(error.to_string()),
            }
        }
        let hosts = std::mem::take(&mut self.supervisor.lock().await.hosts);
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
        let control = format!("http://{}", control_listener.local_addr()?);
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
            uid,
            Hosted {
                service,
                startup_shutdown,
                startup: Some(startup),
                running: None,
                data_root,
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
