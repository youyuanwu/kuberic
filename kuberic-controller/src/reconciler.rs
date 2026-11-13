use std::{collections::BTreeMap, sync::Arc};

use anyhow::{ensure, Context, Result};
use k8s_openapi::api::core::v1::{ConfigMap, Pod, Service};
use kube::{api::PostParams, Api, Client, ResourceExt};

use crate::{
    plan::{new_allocation, owned},
    resources::{self, POD_UID},
    ControllerPhase, FmApi, KubericSet, KubericSetStatus, Observation, Plan, Settings,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReconcileOutcome {
    pub changed: bool,
    pub blocked: Option<String>,
}

pub struct Reconciler {
    settings: Settings,
    sets: Api<KubericSet>,
    pods: Api<Pod>,
    maps: Api<ConfigMap>,
    services: Api<Service>,
    fm: Arc<dyn FmApi>,
}

impl Reconciler {
    pub fn new(client: Client, settings: Settings, fm: Arc<dyn FmApi>) -> Result<Self> {
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
        })
    }

    pub async fn observe(&self, name: &str) -> Result<Observation> {
        let set = self.sets.get(name).await?;
        let mut pods = Vec::new();
        let mut config_maps = Vec::new();
        for index in 1..=3 {
            let name = format!("{name}-node-{index}");
            pods.push(self.pods.get_opt(&name).await?);
            config_maps.push(self.maps.get_opt(&name).await?);
        }
        Ok(Observation {
            set,
            pods,
            config_maps,
            service: self
                .services
                .get_opt(&resources::write_service_name(name))
                .await?,
            fm: self.fm.observe().await?,
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
            "observation changed before effect; reobserve instead of applying stale authority"
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
        let observation = self.observe(name).await?;
        let plan = crate::evaluate(&observation, &self.settings, self.fm.owner_id());
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
            });
        }
        let current = self.current(observation).await?;
        ensure!(
            crate::evaluate(&current, &self.settings, self.fm.owner_id()) == plan,
            "reconciliation premise changed before effect"
        );
        let set = &current.set;
        let accepted = set.status.as_ref().cloned().unwrap_or_default();
        let allocation = accepted.allocation.as_ref();
        match plan {
            Plan::Allocate => {
                self.status(
                    set,
                    KubericSetStatus {
                        allocation: Some(new_allocation(set, &self.settings, self.fm.owner_id())?),
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
                next.phase = ControllerPhase::Configuring;
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
                self.status(set, status).await?;
            }
            Plan::Wait | Plan::Blocked(_) => unreachable!("handled before effects"),
        }
        Ok(ReconcileOutcome {
            changed: true,
            blocked: None,
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
                && report.cohort.allocation.fm_owner == self.fm.owner_id()
            {
                if let Err(error) = self.fm.invalidate(&report.cohort, &reason).await {
                    errors.push(format!("invalidating native FM: {error:#}"));
                }
            }
        }
        let mut changed = false;
        if let Some(mut service) = current.service {
            if owned(&service.metadata, &current.set) {
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
        })
    }
}
