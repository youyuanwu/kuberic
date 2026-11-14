pub mod api;

use std::{
    collections::BTreeMap,
    sync::{
        atomic::{AtomicU64, AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

use anyhow::{ensure, Context, Result};
use async_trait::async_trait;
use fube_ra::protocol::*;
use k8s_openapi::api::core::v1::{Pod, PodCondition, PodStatus};
use kube::{
    api::{ListParams, PostParams},
    Api, Client, ResourceExt,
};
use kuberic_fm::{Outbound, State};
use kuberic_operator::{
    resources, Allocation, BackendAdmission, CleanupState, ControllerPhase, FmApi, FmAuthority,
    FmObservation, FrozenCohort, KubericSet, KubericSetSpec, NodeCleanup, ProcessApi,
    ProcessObservation, Reconciler, ServiceTemplate, Settings,
};
use tokio::sync::Mutex;

pub const NAMESPACE: &str = "test-kuberic";
pub const NAME: &str = "sample";

pub fn spec(package: String, client_port: u16) -> KubericSetSpec {
    KubericSetSpec {
        replicas: 3,
        image: "sample:local".into(),
        package_path: package,
        client_port,
        service: ServiceTemplate {
            application: "fabric:/External".into(),
            application_type: "HostingLocalStatefulApp".into(),
            application_number: 1,
            package: "LocalService".into(),
            service_type: "LocalStatefulType".into(),
            name: "fabric:/External/Singleton".into(),
            package_version_instance: 1,
        },
        parameters: BTreeMap::from([("Arguments".into(), format!(
            "--stateful-lifecycle --data-replicator --replication-listen {{node_ip}}:0 --service-listen {{node_ip}}:{client_port}"
        ))]),
    }
}

pub fn settings() -> Settings {
    Settings {
        namespace: NAMESPACE.into(),
        fm: FmAuthority {
            id: "f".into(),
            instance: 1,
            address: "127.0.0.1:19001".into(),
            generation: 1,
        },
        node_port: 19002,
        teardown_timeout_ms: 60_000,
    }
}

struct ScriptedSession {
    cohort: FrozenCohort,
    state: State,
    joined: bool,
    now: Duration,
}

#[derive(Default)]
pub struct ScriptedFm {
    admission: BackendAdmission,
    session: Mutex<Option<ScriptedSession>>,
    pub starts: AtomicUsize,
    pub shutdowns: AtomicUsize,
    pub joins: AtomicUsize,
}

impl ScriptedFm {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn bootstrap(&self) {
        let mut session = self.session.lock().await;
        let ScriptedSession { cohort, state, .. } = session.as_mut().expect("accepted FM session");
        let config = cohort.fm_config().unwrap();
        for node in 0..3 {
            let up = NodeUp::fresh_stateless(
                cohort.allocation.nodes[node].pod_name.clone(),
                "fube-node".into(),
                config.nodes[node].address.into(),
                FabricVersionInstance::native_default(),
            )
            .unwrap();
            state.observe(node, &node_up_message(&up).unwrap()).unwrap();
            state
                .observe(
                    node,
                    &replica_up_message(
                        &ReplicaUp {
                            last: true,
                            ..ReplicaUp::default()
                        },
                        Owner::Fm,
                    )
                    .unwrap(),
                )
                .unwrap();
        }
    }

    pub async fn command(&self) -> Outbound {
        let mut session = self.session.lock().await;
        let session = session.as_mut().unwrap();
        session.state.reconcile(session.now).unwrap().pop().unwrap()
    }

    pub async fn retry(&self) -> Outbound {
        let mut session = self.session.lock().await;
        let session = session.as_mut().unwrap();
        session.now += Duration::from_millis(session.cohort.fm_config().unwrap().retry_interval_ms);
        session.state.reconcile(session.now).unwrap().pop().unwrap()
    }

    pub async fn complete(&self, outbound: &Outbound) {
        let reply = native_completion(&outbound.message);
        self.session
            .lock()
            .await
            .as_mut()
            .unwrap()
            .state
            .observe(outbound.node, &reply)
            .unwrap();
    }

    pub async fn disconnect(&self, node: usize) {
        self.session
            .lock()
            .await
            .as_mut()
            .unwrap()
            .state
            .disconnected(node)
            .unwrap();
    }

    pub async fn create(&self) {
        self.bootstrap().await;
        for _ in 0..4 {
            let command = self.command().await;
            self.complete(&command).await;
        }
    }
}

#[async_trait]
impl FmApi for ScriptedFm {
    async fn reserve(&self, allocation: &Allocation) -> Result<()> {
        self.admission.reserve(allocation).await
    }
    async fn allocation(&self) -> Result<Option<Allocation>> {
        Ok(self.admission.allocation().await)
    }
    async fn observe(&self) -> Result<Option<FmObservation>> {
        Ok(self
            .session
            .lock()
            .await
            .as_ref()
            .map(|session| FmObservation {
                cohort: session.cohort.clone(),
                status: session.state.status(),
                joined: session.joined,
            }))
    }
    async fn start(&self, cohort: &FrozenCohort) -> Result<()> {
        self.admission.require(&cohort.allocation).await?;
        let mut session = self.session.lock().await;
        if let Some(existing) = session.as_ref() {
            ensure!(&existing.cohort == cohort, "different scripted cohort");
            return Ok(());
        }
        *session = Some(ScriptedSession {
            cohort: cohort.clone(),
            state: State::new(cohort.fm_config()?)?,
            joined: false,
            now: Duration::ZERO,
        });
        self.starts.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    async fn shutdown(&self, cohort: &FrozenCohort) -> Result<()> {
        let mut session = self.session.lock().await;
        let session = session.as_mut().context("missing scripted FM")?;
        ensure!(session.cohort == *cohort, "different scripted shutdown");
        match session.state.phase() {
            kuberic_fm::Phase::Serving => {
                session.state.request_shutdown(session.now)?;
                self.shutdowns.fetch_add(1, Ordering::SeqCst);
            }
            kuberic_fm::Phase::Deleting
            | kuberic_fm::Phase::AwaitingNodeShutdown
            | kuberic_fm::Phase::Stopped => {}
            _ => anyhow::bail!("scripted native create has not completed"),
        }
        Ok(())
    }
    async fn join(&self, cohort: &FrozenCohort) -> Result<()> {
        let mut session = self.session.lock().await;
        let session = session.as_mut().context("missing scripted FM")?;
        ensure!(
            session.cohort == *cohort && session.state.phase() == kuberic_fm::Phase::Stopped,
            "scripted native shutdown is incomplete"
        );
        if !session.joined {
            session.joined = true;
            self.joins.fetch_add(1, Ordering::SeqCst);
        }
        Ok(())
    }
    async fn invalidate(&self, cohort: &FrozenCohort, reason: &str) -> Result<()> {
        if let Some(session) = self.session.lock().await.as_mut() {
            ensure!(session.cohort == *cohort, "different scripted invalidation");
            session.state.fail(reason);
        }
        Ok(())
    }
}

#[derive(Default)]
pub struct ProcessEvidence {
    admission: BackendAdmission,
    observation: Mutex<Option<ProcessObservation>>,
}

impl ProcessEvidence {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn publish(&self, cohort: &FrozenCohort, node: usize, outcome: CleanupState) {
        self.observe(cohort).await.unwrap();
        let mut observation = self.observation.lock().await;
        let observation = observation.as_mut().unwrap();
        assert_eq!(&observation.cohort, cohort);
        assert!(
            matches!(observation.nodes[node].outcome, CleanupState::Pending)
                || observation.nodes[node].outcome == outcome,
            "terminal cleanup evidence cannot change"
        );
        observation.nodes[node].outcome = outcome;
    }
}

#[async_trait]
impl ProcessApi for ProcessEvidence {
    async fn reserve(&self, allocation: &Allocation) -> Result<()> {
        self.admission.reserve(allocation).await
    }
    async fn allocation(&self) -> Result<Option<Allocation>> {
        Ok(self.admission.allocation().await)
    }
    async fn observe(&self, cohort: &FrozenCohort) -> Result<Option<ProcessObservation>> {
        if self.admission.allocation().await.as_ref() != Some(&cohort.allocation) {
            return Ok(None);
        }
        let mut observation = self.observation.lock().await;
        if observation.is_none() {
            *observation = Some(ProcessObservation {
                cohort: cohort.clone(),
                nodes: cohort
                    .bindings
                    .iter()
                    .map(|binding| NodeCleanup {
                        pod_uid: binding.pod_uid.clone(),
                        outcome: CleanupState::Pending,
                    })
                    .collect(),
            });
        }
        ensure!(
            observation
                .as_ref()
                .is_some_and(|observation| observation.cohort == *cohort),
            "process owner cannot adopt another cohort"
        );
        Ok(observation.clone())
    }
}

pub fn native_completion(
    message: &fube_transport::message::Message,
) -> fube_transport::message::Message {
    let action = message.properties().unwrap().action;
    if action == "DeleteReplica" {
        let request = decode_delete_replica(message).unwrap().0.replica.0;
        return delete_replica_reply_message(
            &AddInstanceReply {
                unit: request.unit,
                replica: Replica {
                    node: request.replica.node,
                    id: request.replica.id,
                    instance: request.replica.instance,
                    up: false,
                    state: 3,
                    ..Replica::default()
                },
                error: 0,
            },
            Owner::Fm,
        )
        .unwrap();
    }
    if action == "DoReconfiguration" {
        let mut configuration = decode_do_reconfiguration(message).unwrap().0.configuration;
        configuration.unit.previous = Epoch::default();
        for replica in &mut configuration.replicas.0 {
            replica.previous_role = 1;
        }
        return do_reconfiguration_reply_message(
            &ConfigurationReply {
                unit: configuration.unit,
                replicas: configuration.replicas,
                error: 0,
            },
            Owner::Fm,
        )
        .unwrap();
    }
    let request = match action.as_str() {
        "AddPrimary" => decode_add_primary(message).unwrap().0,
        "AddReplica" => decode_add_replica(message).unwrap().0,
        other => panic!("unexpected scripted command {other}"),
    };
    let mut replica = request.replica;
    replica.state = 2;
    replica.replication_endpoint = format!("native://{}/actor-original", replica.id);
    if action == "AddPrimary" {
        replica.service_endpoint = "client://opaque-original-primary".into();
    }
    let reply = AddInstanceReply {
        unit: request.unit,
        replica,
        error: 0,
    };
    if action == "AddPrimary" {
        add_primary_reply_message(&reply, Owner::Fm).unwrap()
    } else {
        add_replica_reply_message(&reply, Owner::Fm).unwrap()
    }
}

pub struct Fixture {
    pub api: api::MockApi,
    pub client: Client,
    pub fm: Arc<ScriptedFm>,
    pub processes: Arc<ProcessEvidence>,
    pub clock: Arc<AtomicU64>,
    pub controller: Reconciler,
}

impl Fixture {
    pub async fn new() -> Self {
        let api = api::MockApi::default();
        let client = api.client(NAMESPACE);
        Api::<KubericSet>::namespaced(client.clone(), NAMESPACE)
            .create(
                &PostParams::default(),
                &KubericSet::new(NAME, spec("/opt/fube/package".into(), 8080)),
            )
            .await
            .unwrap();
        let fm = Arc::new(ScriptedFm::new());
        let processes = Arc::new(ProcessEvidence::new());
        let clock = Arc::new(AtomicU64::new(1_000));
        let now = clock.clone();
        let controller = Reconciler::new(client.clone(), settings(), fm.clone(), processes.clone())
            .unwrap()
            .with_clock(move || Ok(now.load(Ordering::SeqCst)));
        Self {
            api,
            client,
            fm,
            processes,
            clock,
            controller,
        }
    }

    pub fn recreate(&self) -> Reconciler {
        let clock = self.clock.clone();
        Reconciler::new(
            self.client.clone(),
            settings(),
            self.fm.clone(),
            self.processes.clone(),
        )
        .unwrap()
        .with_clock(move || Ok(clock.load(Ordering::SeqCst)))
    }

    pub async fn set(&self) -> KubericSet {
        Api::<KubericSet>::namespaced(self.client.clone(), NAMESPACE)
            .get(NAME)
            .await
            .unwrap()
    }

    pub async fn pods(&self) -> Vec<Pod> {
        Api::<Pod>::namespaced(self.client.clone(), NAMESPACE)
            .list(&ListParams::default().labels(&format!(
                "{}={}",
                resources::SET_UID,
                self.set().await.uid().unwrap()
            )))
            .await
            .unwrap()
            .items
    }

    pub async fn drive(&self) {
        for _ in 0..64 {
            let outcome = self.controller.reconcile(NAME).await.unwrap();
            assert!(outcome.blocked.is_none(), "{outcome:?}");
            if !outcome.changed {
                self.api.check();
                return;
            }
        }
        panic!("controller did not reach waiting/stable state");
    }

    pub async fn provision(&self) {
        self.drive().await;
        for (index, pod) in self.pods().await.iter().enumerate() {
            publish_ip(&self.client, pod, &format!("127.0.0.{}", index + 2), true).await;
        }
        self.drive().await;
    }

    pub async fn serving(&self) {
        self.provision().await;
        self.fm.create().await;
        self.drive().await;
        assert_eq!(
            self.set().await.status.unwrap().phase,
            ControllerPhase::Serving
        );
    }
}

pub async fn publish_ip(client: &Client, pod: &Pod, ip: &str, ready: bool) {
    let mut next = pod.clone();
    next.status = Some(PodStatus {
        phase: Some("Running".into()),
        pod_ip: Some(ip.into()),
        conditions: Some(vec![PodCondition {
            type_: "Ready".into(),
            status: if ready { "True" } else { "False" }.into(),
            ..PodCondition::default()
        }]),
        ..PodStatus::default()
    });
    Api::<Pod>::namespaced(
        client.clone(),
        &pod.namespace().context("Pod namespace").unwrap(),
    )
    .replace_status(&pod.name_any(), &PostParams::default(), &next)
    .await
    .unwrap();
}
