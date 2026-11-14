use std::{
    collections::BTreeMap,
    fmt,
    future::Future,
    net::{Ipv4Addr, SocketAddrV4},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};

use anyhow::{ensure, Context, Result};
use futures_util::StreamExt;
use k8s_openapi::api::core::v1::{ConfigMap, Pod, Service};
use kube::{
    api::ListParams,
    runtime::{controller::Action, watcher, Controller},
    Api, Client, ResourceExt,
};
use tokio::{net::TcpListener, sync::Mutex};

use crate::{
    processes::PodProcesses, resources, FmApi, FmAuthority, KubericSet, Plan, ProcessApi,
    ProcessFm, Reconciler, Settings, StaleObservation, TeardownStage,
};

#[derive(Clone, Debug)]
pub struct Config {
    pub namespace: String,
    pub advertise_ip: Ipv4Addr,
    pub node_port: u16,
    pub teardown_timeout_ms: u64,
    pub requeue: Duration,
}

struct Owner {
    name: String,
    uid: String,
    reconciler: Reconciler,
    fm: Arc<ProcessFm>,
    processes: Arc<PodProcesses>,
    completed: AtomicBool,
}

pub struct Operator {
    client: Client,
    config: Config,
    owners: Mutex<BTreeMap<String, Arc<Owner>>>,
}

impl Operator {
    pub fn new(client: Client, config: Config) -> Result<Self> {
        ensure!(
            !config.namespace.is_empty()
                && config.namespace.len() <= 63
                && config
                    .namespace
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
                && !config.namespace.starts_with('-')
                && !config.namespace.ends_with('-'),
            "a valid installation namespace is required"
        );
        ensure!(
            !config.advertise_ip.is_unspecified()
                && !config.advertise_ip.is_multicast()
                && config.node_port != 0
                && (100..=600_000).contains(&config.teardown_timeout_ms)
                && (Duration::from_millis(20)..=Duration::from_secs(30)).contains(&config.requeue),
            "invalid operator address, ports, deadline or requeue interval"
        );
        Ok(Self {
            client,
            config,
            owners: Mutex::new(BTreeMap::new()),
        })
    }

    async fn owner(&self, set: &KubericSet) -> Result<Arc<Owner>> {
        let uid = set.uid().context("KubericSet has no UID")?;
        ensure!(
            set.namespace().as_deref() == Some(&self.config.namespace),
            "KubericSet is outside the installation namespace"
        );
        let mut owners = self.owners.lock().await;
        if let Some(owner) = owners.get(&uid) {
            ensure!(
                owner.name == set.name_any(),
                "KubericSet UID changed its name"
            );
            return Ok(owner.clone());
        }
        // Keep the socket reserved until native startup; closing a probe listener
        // would permit another allocation to acquire the accepted address.
        let listener = TcpListener::bind(SocketAddrV4::new(self.config.advertise_ip, 0)).await?;
        let address = listener.local_addr()?.to_string();
        let fm = Arc::new(ProcessFm::with_listener(listener));
        let processes = Arc::new(PodProcesses::new(
            self.client.clone(),
            &self.config.namespace,
        ));
        let reconciler = Reconciler::new(
            self.client.clone(),
            Settings {
                namespace: self.config.namespace.clone(),
                fm: FmAuthority {
                    id: uuid::Uuid::new_v4().simple().to_string(),
                    instance: 1,
                    address,
                    generation: 1,
                },
                node_port: self.config.node_port,
                teardown_timeout_ms: self.config.teardown_timeout_ms,
            },
            fm.clone(),
            processes.clone(),
        )?;
        let owner = Arc::new(Owner {
            name: set.name_any(),
            uid: uid.clone(),
            reconciler,
            fm,
            processes,
            completed: AtomicBool::new(false),
        });
        owners.insert(uid, owner.clone());
        Ok(owner)
    }

    pub async fn reconcile(&self, set: &KubericSet) -> Result<Action> {
        let sets: Api<KubericSet> = Api::namespaced(self.client.clone(), &self.config.namespace);
        let Some(current) = sets.get_opt(&set.name_any()).await? else {
            return Ok(Action::await_change());
        };
        if current.uid() != set.uid() {
            return Ok(Action::await_change());
        }
        let owner = self.owner(&current).await?;
        let outcome = if current
            .status
            .as_ref()
            .and_then(|status| status.allocation.as_ref())
            .is_some()
            && owner.fm.allocation().await?.is_none()
        {
            let observation = owner.reconciler.observe(&owner.name).await?;
            ensure!(
                observation.set.uid().as_deref() == Some(&owner.uid),
                StaleObservation::Resources
            );
            owner.reconciler.apply(&observation, Plan::Blocked(
                "original operator backend ownership is unavailable; restart recovery and adoption are unsupported; remote writers may still be running".into()
            )).await?
        } else {
            owner
                .reconciler
                .reconcile_uid(&owner.name, &owner.uid)
                .await?
        };
        if let Some(reason) = outcome.blocked {
            if outcome.changed {
                println!(
                    "{}",
                    serde_json::json!({
                        "event": "reconcile_blocked", "name": owner.name, "set_uid": owner.uid,
                        "reason": reason, "writer_stopped": false
                    })
                );
            }
        }
        let after = sets.get_opt(&owner.name).await?;
        if after.as_ref().is_none_or(|set| {
            set.uid().as_deref() == Some(&owner.uid)
                && set
                    .status
                    .as_ref()
                    .and_then(|status| status.teardown.as_ref())
                    .is_some_and(|teardown| teardown.stage == TeardownStage::Complete)
        }) {
            self.complete(&owner).await?;
        }
        if after.is_none()
            && (owner.completed.load(Ordering::Acquire)
                || (owner.fm.allocation().await?.is_none()
                    && owner.processes.allocation().await?.is_none()))
        {
            self.owners.lock().await.remove(&owner.uid);
            return Ok(Action::await_change());
        }
        Ok(Action::requeue(if outcome.changed {
            self.config.requeue.min(Duration::from_millis(50))
        } else {
            self.config.requeue
        }))
    }

    async fn complete(&self, owner: &Owner) -> Result<()> {
        if owner.completed.load(Ordering::Acquire) {
            return Ok(());
        }
        let Some(native) = owner.fm.observe().await? else {
            return Ok(());
        };
        if !native.joined || native.status.phase != kuberic_fm::Phase::Stopped {
            return Ok(());
        }
        ensure!(
            native.status.error.is_none()
                && native.status.deleted_nodes == native.cohort.bindings.len(),
            "native FM joined without original deletion acknowledgements"
        );
        let reports = owner.processes.reports(&native.cohort).await?;
        let selector =
            ListParams::default().labels(&format!("{}={}", resources::SET_UID, owner.uid));
        if !Api::<Pod>::namespaced(self.client.clone(), &self.config.namespace)
            .list(&selector)
            .await?
            .items
            .is_empty()
            || !Api::<ConfigMap>::namespaced(self.client.clone(), &self.config.namespace)
                .list(&selector)
                .await?
                .items
                .is_empty()
            || !Api::<Service>::namespaced(self.client.clone(), &self.config.namespace)
                .list(&selector)
                .await?
                .items
                .is_empty()
        {
            return Ok(());
        }
        println!(
            "{}",
            serde_json::json!({
                "event": "lifecycle_complete", "name": owner.name, "set_uid": owner.uid,
                "cohort": native.cohort,
                "deployment": native.cohort.allocation.deployment, "reports": reports,
                "primary_node_id": native.status.primary_node_id,
                "native_joined": true, "resources_absent": true
            })
        );
        owner.completed.store(true, Ordering::Release);
        Ok(())
    }

    pub async fn stop(&self) -> Result<()> {
        let owners: Vec<_> = self.owners.lock().await.values().cloned().collect();
        let sets: Api<KubericSet> = Api::namespaced(self.client.clone(), &self.config.namespace);
        let mut errors = Vec::new();
        for owner in owners {
            if owner.completed.load(Ordering::Acquire) {
                continue;
            }
            let result: Result<()> = async {
                let Some(set) = sets.get_opt(&owner.name).await? else {
                    return Ok(());
                };
                ensure!(set.uid().as_deref() == Some(&owner.uid), "original KubericSet was replaced");
                let observation = owner.reconciler.observe(&owner.name).await?;
                ensure!(observation.set.uid().as_deref() == Some(&owner.uid), "KubericSet changed before operator withdrawal");
                let outcome = owner.reconciler.apply(
                    &observation,
                    Plan::Blocked("operator stopped; restart recovery is unsupported; remote writers may still be running".into()),
                ).await?;
                ensure!(outcome.blocked.is_some(), "operator shutdown did not withdraw publication");
                Ok(())
            }.await;
            if let Err(error) = result {
                errors.push(format!("{}: {error:#}", owner.name));
            }
        }
        ensure!(
            errors.is_empty(),
            "operator withdrawal failed: {}",
            errors.join("; ")
        );
        Ok(())
    }

    pub async fn run(
        self: Arc<Self>,
        stop: impl Future<Output = ()> + Send + Sync + 'static,
    ) -> Result<()> {
        let sets: Api<KubericSet> = Api::namespaced(self.client.clone(), &self.config.namespace);
        // Fail startup explicitly for a missing CRD or inaccessible namespace.
        sets.list(&ListParams::default().limit(1)).await?;
        println!(
            "{}",
            serde_json::json!({
                "event": "operator_started", "namespace": self.config.namespace,
                "restart_recovery": false
            })
        );
        Controller::new(sets, watcher::Config::default())
            .owns(Api::<Pod>::namespaced(self.client.clone(), &self.config.namespace), watcher::Config::default())
            .owns(Api::<ConfigMap>::namespaced(self.client.clone(), &self.config.namespace), watcher::Config::default())
            .owns(Api::<Service>::namespaced(self.client.clone(), &self.config.namespace), watcher::Config::default())
            .graceful_shutdown_on(stop)
            .run(
                |set, operator: Arc<Self>| async move {
                    operator.reconcile(&set).await.map_err(ReconcileError)
                },
                |set, error, operator| {
                    let transient = error.0.is::<StaleObservation>()
                        || matches!(error.0.downcast_ref::<kube::Error>(),
                            Some(kube::Error::Api(response)) if response.code == 409 && response.reason == "Conflict");
                    eprintln!("{}", serde_json::json!({
                        "event": "reconcile_error", "name": set.name_any(), "set_uid": set.uid(),
                        "retry_observation": transient, "error": format!("{error:#}")
                    }));
                    Action::requeue(if transient {
                        Duration::from_millis(50)
                    } else {
                        operator.config.requeue.max(Duration::from_secs(1))
                    })
                },
                self.clone(),
            )
            .for_each(|result| async {
                if let Err(error) = result {
                    eprintln!("{}", serde_json::json!({"event": "controller_error", "error": error.to_string()}));
                }
            })
            .await;
        self.stop().await
    }
}

#[derive(Debug)]
struct ReconcileError(anyhow::Error);

impl fmt::Display for ReconcileError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{:#}", self.0)
    }
}

impl std::error::Error for ReconcileError {}
