use std::{
    collections::BTreeMap,
    fmt,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{ensure, Context, Result};
use k8s_openapi::api::core::v1::{ConfigMap, Pod, Service};
use kube::{
    api::{DeleteParams, ObjectMeta, PostParams, Preconditions},
    Api, Client, ResourceExt,
};

use crate::{
    plan::{new_allocation, owned},
    resources::{self, POD_UID},
    BootstrapWait, ControllerPhase, FmApi, KubericSet, KubericSetStatus, Observation, Plan,
    ProcessApi, Settings, Teardown, TeardownStage,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReconcileOutcome {
    pub changed: bool,
    pub blocked: Option<String>,
    /// Kubernetes object absence only; never a process-cleanup receipt.
    pub resource_absent: bool,
}

/// No effect was applied; callers may retry with a fresh observation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StaleObservation {
    Resources,
    Premise,
}

impl fmt::Display for StaleObservation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Resources => {
                "observation changed before effect; reobserve instead of applying stale authority"
            }
            Self::Premise => "reconciliation premise changed before effect",
        })
    }
}

impl std::error::Error for StaleObservation {}

pub struct Reconciler {
    settings: Settings,
    sets: Api<KubericSet>,
    pods: Api<Pod>,
    maps: Api<ConfigMap>,
    services: Api<Service>,
    fm: Arc<dyn FmApi>,
    processes: Arc<dyn ProcessApi>,
    clock: Arc<dyn Fn() -> Result<u64> + Send + Sync>,
}

impl Reconciler {
    pub fn new(
        client: Client,
        settings: Settings,
        fm: Arc<dyn FmApi>,
        processes: Arc<dyn ProcessApi>,
    ) -> Result<Self> {
        ensure!(
            !settings.namespace.is_empty(),
            "installation namespace is required"
        );
        Ok(Self {
            sets: Api::namespaced(client.clone(), &settings.namespace),
            pods: Api::namespaced(client.clone(), &settings.namespace),
            maps: Api::namespaced(client.clone(), &settings.namespace),
            services: Api::namespaced(client, &settings.namespace),
            settings,
            fm,
            processes,
            clock: Arc::new(|| {
                Ok(u64::try_from(
                    SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis(),
                )?)
            }),
        })
    }

    pub fn with_clock(mut self, clock: impl Fn() -> Result<u64> + Send + Sync + 'static) -> Self {
        self.clock = Arc::new(clock);
        self
    }

    pub async fn observe(&self, name: &str) -> Result<Observation> {
        let set = self.sets.get(name).await?;
        self.observe_set(set).await
    }

    async fn observe_set(&self, set: KubericSet) -> Result<Observation> {
        let name = set.name_any();
        let mut pods = Vec::new();
        let mut config_maps = Vec::new();
        for index in 1..=3 {
            let name = format!("{name}-node-{index}");
            pods.push(self.pods.get_opt(&name).await?);
            config_maps.push(self.maps.get_opt(&name).await?);
        }
        let processes = if let Some(cohort) = set.status.as_ref().and_then(KubericSetStatus::cohort)
        {
            self.processes.observe(&cohort).await?
        } else {
            None
        };
        Ok(Observation {
            set,
            pods,
            config_maps,
            service: self
                .services
                .get_opt(&resources::write_service_name(&name))
                .await?,
            fm: self.fm.observe().await?,
            processes,
            fm_allocation: self.fm.allocation().await?,
            process_allocation: self.processes.allocation().await?,
            now_unix_ms: (self.clock)()?,
        })
    }

    async fn current(&self, observation: &Observation) -> Result<Observation> {
        let current = self.observe(&observation.set.name_any()).await?;
        ensure!(
            current.set.uid() == observation.set.uid()
                && current.set.resource_version() == observation.set.resource_version()
                && current
                    .pods
                    .iter()
                    .zip(&observation.pods)
                    .all(|(current, old)| {
                        current
                            .as_ref()
                            .map(|object| (object.uid(), object.resource_version()))
                            == old
                                .as_ref()
                                .map(|object| (object.uid(), object.resource_version()))
                    })
                && current
                    .config_maps
                    .iter()
                    .zip(&observation.config_maps)
                    .all(|(current, old)| {
                        current
                            .as_ref()
                            .map(|object| (object.uid(), object.resource_version()))
                            == old
                                .as_ref()
                                .map(|object| (object.uid(), object.resource_version()))
                    })
                && current
                    .service
                    .as_ref()
                    .map(|object| (object.uid(), object.resource_version()))
                    == observation
                        .service
                        .as_ref()
                        .map(|object| (object.uid(), object.resource_version())),
            StaleObservation::Resources
        );
        Ok(current)
    }

    async fn status(&self, set: &KubericSet, status: KubericSetStatus) -> Result<()> {
        let mut next = set.clone();
        next.status = Some(status);
        self.sets
            .replace_status(&set.name_any(), &PostParams::default(), &next)
            .await?;
        Ok(())
    }

    pub async fn reconcile(&self, name: &str) -> Result<ReconcileOutcome> {
        self.reconcile_for(name, None).await
    }

    pub async fn reconcile_uid(&self, name: &str, uid: &str) -> Result<ReconcileOutcome> {
        self.reconcile_for(name, Some(uid)).await
    }

    async fn reconcile_for(&self, name: &str, uid: Option<&str>) -> Result<ReconcileOutcome> {
        let Some(set) = self.sets.get_opt(name).await? else {
            return Ok(ReconcileOutcome {
                changed: false,
                blocked: None,
                resource_absent: true,
            });
        };
        ensure!(
            uid.is_none_or(|uid| set.uid().as_deref() == Some(uid)),
            StaleObservation::Resources
        );
        let observation = self.observe_set(set).await?;
        let plan = crate::evaluate(&observation, &self.settings);
        self.apply(&observation, plan).await
    }

    pub async fn apply(&self, observation: &Observation, plan: Plan) -> Result<ReconcileOutcome> {
        if let Plan::Blocked(reason) = plan {
            return self.block(observation, reason).await;
        }
        if matches!(plan, Plan::Wait) {
            return Ok(ReconcileOutcome {
                changed: false,
                blocked: None,
                resource_absent: false,
            });
        }
        let current = self.current(observation).await?;
        ensure!(
            crate::evaluate(&current, &self.settings) == plan,
            StaleObservation::Premise
        );
        let set = &current.set;
        let accepted = set.status.as_ref().cloned().unwrap_or_default();
        let allocation = accepted.allocation.as_ref();
        match plan {
            Plan::AddFinalizer | Plan::RemoveFinalizer => {
                let mut next = set.clone();
                let finalizers = next.metadata.finalizers.get_or_insert_default();
                if matches!(plan, Plan::AddFinalizer) {
                    if !finalizers
                        .iter()
                        .any(|finalizer| finalizer == resources::FINALIZER)
                    {
                        finalizers.push(resources::FINALIZER.into());
                    }
                } else {
                    finalizers.retain(|finalizer| finalizer != resources::FINALIZER);
                }
                self.sets
                    .replace(&set.name_any(), &PostParams::default(), &next)
                    .await?;
            }
            Plan::Allocate => {
                let draft = match current.fm_allocation.as_ref() {
                    Some(draft) => draft.clone(),
                    None => new_allocation(set, &self.settings)?,
                };
                self.fm.reserve(&draft).await?;
                self.processes.reserve(&draft).await?;
                self.status(
                    set,
                    KubericSetStatus {
                        allocation: Some(draft),
                        ..KubericSetStatus::default()
                    },
                )
                .await?;
            }
            Plan::CreateConfigMap(index) => {
                self.maps
                    .create(
                        &PostParams::default(),
                        &resources::config_map(
                            set,
                            allocation.context("missing allocation")?,
                            index,
                        )?,
                    )
                    .await?;
            }
            Plan::CreateService => {
                self.services
                    .create(
                        &PostParams::default(),
                        &resources::service(set, allocation.context("missing allocation")?)?,
                    )
                    .await?;
            }
            Plan::CreatePod(index) => {
                self.pods
                    .create(
                        &PostParams::default(),
                        &resources::pod(set, allocation.context("missing allocation")?, index)?,
                    )
                    .await?;
            }
            Plan::LabelPod(index) => {
                let mut pod = current
                    .pods
                    .get(index)
                    .and_then(Option::as_ref)
                    .context("missing original Pod")?
                    .clone();
                let uid = pod.uid().context("Pod has no UID")?;
                pod.metadata
                    .labels
                    .get_or_insert_default()
                    .insert(POD_UID.into(), uid);
                self.pods
                    .replace(&pod.name_any(), &PostParams::default(), &pod)
                    .await?;
            }
            Plan::Freeze(bindings) => {
                let mut next = accepted;
                next.bindings = Some(bindings);
                next.phase = if set.metadata.deletion_timestamp.is_some() {
                    ControllerPhase::Stopping
                } else {
                    ControllerPhase::Configuring
                };
                self.status(set, next).await?;
            }
            Plan::StartFm(cohort) => {
                self.fm.start(&cohort).await?;
            }
            Plan::ConfigureNode(index) => {
                let cohort = accepted.cohort().context("missing frozen cohort")?;
                let mut map = current
                    .config_maps
                    .get(index)
                    .and_then(Option::as_ref)
                    .context("missing original ConfigMap")?
                    .clone();
                map.data = Some(BTreeMap::from([(
                    "node.json".into(),
                    serde_json::to_string(&resources::runner_config(&cohort, index)?)?,
                )]));
                map.immutable = Some(true);
                self.maps
                    .replace(&map.name_any(), &PostParams::default(), &map)
                    .await?;
            }
            Plan::Route(selector) => {
                let mut service = current.service.context("missing owned Service")?;
                service
                    .spec
                    .as_mut()
                    .context("Service has no spec")?
                    .selector = Some(selector);
                self.services
                    .replace(&service.name_any(), &PostParams::default(), &service)
                    .await?;
            }
            Plan::Status(status) => {
                self.status(set, *status).await?;
            }
            Plan::AwaitBootstrap => {
                let mut next = accepted;
                next.phase = ControllerPhase::Stopping;
                next.service_endpoint = None;
                next.primary_node_id = None;
                next.reason = None;
                next.bootstrap_wait = Some(BootstrapWait {
                    started_unix_ms: current.now_unix_ms,
                    deadline_unix_ms: current
                        .now_unix_ms
                        .checked_add(self.settings.teardown_timeout_ms)
                        .context("bootstrap completion deadline overflow")?,
                });
                self.status(set, next).await?;
            }
            Plan::BeginTeardown {
                cohort,
                service_uid,
            } => {
                let mut next = accepted;
                next.phase = ControllerPhase::Stopping;
                next.service_endpoint = None;
                next.primary_node_id = None;
                next.reason = None;
                next.teardown = Some(Teardown {
                    cohort,
                    service_uid,
                    started_unix_ms: current.now_unix_ms,
                    deadline_unix_ms: current
                        .now_unix_ms
                        .checked_add(self.settings.teardown_timeout_ms)
                        .context("teardown deadline overflow")?,
                    stage: TeardownStage::NativeDeletion,
                });
                self.status(set, next).await?;
            }
            Plan::AdvanceTeardown(stage) => {
                let mut next = accepted;
                let teardown = next
                    .teardown
                    .as_mut()
                    .context("missing retained teardown")?;
                teardown.stage = stage;
                next.phase = if stage == TeardownStage::Complete {
                    ControllerPhase::Stopped
                } else {
                    ControllerPhase::Stopping
                };
                self.status(set, next).await?;
            }
            Plan::ShutdownFm(cohort) => self.fm.shutdown(&cohort).await?,
            Plan::JoinFm(cohort) => self.fm.join(&cohort).await?,
            Plan::DeletePod(index) => {
                let pod = current
                    .pods
                    .get(index)
                    .and_then(Option::as_ref)
                    .context("missing original Pod")?;
                self.pods
                    .delete(&pod.name_any(), &delete_params(&pod.metadata, true)?)
                    .await?;
            }
            Plan::DeleteConfigMap(index) => {
                let map = current
                    .config_maps
                    .get(index)
                    .and_then(Option::as_ref)
                    .context("missing original ConfigMap")?;
                self.maps
                    .delete(&map.name_any(), &delete_params(&map.metadata, false)?)
                    .await?;
            }
            Plan::DeleteService => {
                let service = current
                    .service
                    .as_ref()
                    .context("missing original Service")?;
                self.services
                    .delete(
                        &service.name_any(),
                        &delete_params(&service.metadata, false)?,
                    )
                    .await?;
            }
            Plan::Wait | Plan::Blocked(_) => unreachable!("handled before effects"),
        }
        Ok(ReconcileOutcome {
            changed: true,
            blocked: None,
            resource_absent: false,
        })
    }

    async fn block(&self, observation: &Observation, reason: String) -> Result<ReconcileOutcome> {
        let current = self.observe(&observation.set.name_any()).await?;
        ensure!(
            current.set.uid() == observation.set.uid(),
            "KubericSet UID changed before withdrawal"
        );
        let mut errors = Vec::new();
        if let Some(report) = &current.fm {
            if report.cohort.allocation.resource_uid
                == current.set.uid().context("KubericSet has no UID")?
                && current.fm_allocation.as_ref() == Some(&report.cohort.allocation)
                && !report.joined
            {
                if let Err(error) = self.fm.invalidate(&report.cohort, &reason).await {
                    errors.push(format!("invalidating native FM: {error:#}"));
                }
            }
        }
        let mut changed = false;
        if let Some(mut service) = current.service {
            let original_service = current
                .set
                .status
                .as_ref()
                .and_then(|status| status.teardown.as_ref())
                .is_none_or(|teardown| {
                    service.metadata.uid.as_deref() == Some(teardown.service_uid.as_str())
                });
            if owned(&service.metadata, &current.set) && original_service {
                let spec = service.spec.as_mut().context("Service has no spec")?;
                let disabled = resources::disabled_selector(&current.set)?;
                if spec.selector.as_ref() != Some(&disabled) {
                    spec.selector = Some(disabled);
                    match self
                        .services
                        .replace(&service.name_any(), &PostParams::default(), &service)
                        .await
                    {
                        Ok(_) => changed = true,
                        Err(error) => errors.push(format!("withdrawing client Service: {error}")),
                    }
                }
            }
        }
        let mut next = current.set.status.clone().unwrap_or_default();
        next.phase = ControllerPhase::Blocked;
        next.service_endpoint = None;
        next.primary_node_id = None;
        next.reason = Some(reason.clone());
        if current.set.status.as_ref() != Some(&next) {
            match self.status(&current.set, next).await {
                Ok(()) => changed = true,
                Err(error) => errors.push(format!("persisting blocked status: {error:#}")),
            }
        }
        ensure!(errors.is_empty(), "{}", errors.join("; "));
        Ok(ReconcileOutcome {
            changed,
            blocked: Some(reason),
            resource_absent: false,
        })
    }
}

fn delete_params(metadata: &ObjectMeta, graceful_pod: bool) -> Result<DeleteParams> {
    Ok(DeleteParams {
        preconditions: Some(Preconditions {
            uid: Some(metadata.uid.clone().context("delete target has no UID")?),
            resource_version: Some(
                metadata
                    .resource_version
                    .clone()
                    .context("delete target has no resourceVersion")?,
            ),
        }),
        grace_period_seconds: graceful_pod.then_some(30),
        ..DeleteParams::default()
    })
}
