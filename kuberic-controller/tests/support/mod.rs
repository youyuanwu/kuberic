#![allow(dead_code)]

pub mod api;

use std::{
    collections::BTreeMap,
    sync::{
        atomic::{AtomicUsize, Ordering},
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
use kuberic_controller::{
    resources, ControllerPhase, FmApi, FmAuthority, FmObservation, FrozenCohort, KubericSet,
    KubericSetSpec, Reconciler, ServiceTemplate, Settings,
};
use kuberic_fm::{Outbound, State};
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
        run_root: "/run/fube".into(),
    }
}

#[derive(Default)]
pub struct ScriptedFm {
    owner: String,
    session: Mutex<Option<(FrozenCohort, State)>>,
    pub starts: AtomicUsize,
}

impl ScriptedFm {
    pub fn new() -> Self {
        Self {
            owner: uuid::Uuid::new_v4().to_string(),
            ..Self::default()
        }
    }

    pub async fn bootstrap(&self) {
        let mut session = self.session.lock().await;
        let (cohort, state) = session.as_mut().expect("accepted FM session");
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
        session
            .as_mut()
            .unwrap()
            .1
            .reconcile(Duration::ZERO)
            .unwrap()
            .pop()
            .unwrap()
    }

    pub async fn complete(&self, outbound: &Outbound) {
        let reply = native_completion(&outbound.message);
        self.session
            .lock()
            .await
            .as_mut()
            .unwrap()
            .1
            .observe(outbound.node, &reply)
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
    fn owner_id(&self) -> &str {
        &self.owner
    }
    async fn observe(&self) -> Result<Option<FmObservation>> {
        Ok(self
            .session
            .lock()
            .await
            .as_ref()
            .map(|(cohort, state)| FmObservation {
                cohort: cohort.clone(),
                status: state.status(),
            }))
    }
    async fn start(&self, cohort: &FrozenCohort) -> Result<()> {
        let mut session = self.session.lock().await;
        if let Some((existing, _)) = session.as_ref() {
            ensure!(existing == cohort, "different scripted cohort");
            return Ok(());
        }
        *session = Some((cohort.clone(), State::new(cohort.fm_config()?)?));
        self.starts.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    async fn invalidate(&self, cohort: &FrozenCohort, reason: &str) -> Result<()> {
        if let Some((existing, state)) = self.session.lock().await.as_mut() {
            ensure!(existing == cohort, "different scripted invalidation");
            state.fail(reason);
        }
        Ok(())
    }
}

pub fn native_completion(
    message: &fube_transport::message::Message,
) -> fube_transport::message::Message {
    let action = message.properties().unwrap().action;
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
        let controller = Reconciler::new(client.clone(), settings(), fm.clone()).unwrap();
        Self {
            api,
            client,
            fm,
            controller,
        }
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
