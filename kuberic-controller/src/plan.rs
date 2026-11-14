use std::{collections::BTreeMap, path::Path};

use anyhow::{ensure, Context, Result};
use k8s_openapi::api::core::v1::{ConfigMap, Pod, PodSpec, Service};
use kube::{api::ObjectMeta, ResourceExt};
use kuberic_fm::Phase;

use crate::{
    resources::{self, DEPLOYMENT, NODE_ID, POD_UID, SET_UID, SPEC},
    Allocation, Binding, ControllerPhase, FmObservation, FrozenCohort, KubericSet,
    KubericSetStatus, NodeAllocation, ProcessObservation, Settings, TeardownStage,
};

#[derive(Clone, Debug)]
pub struct Observation {
    pub set: KubericSet,
    pub pods: Vec<Option<Pod>>,
    pub config_maps: Vec<Option<ConfigMap>>,
    pub service: Option<Service>,
    pub fm: Option<FmObservation>,
    pub processes: Option<ProcessObservation>,
    pub fm_allocation: Option<Allocation>,
    pub process_allocation: Option<Allocation>,
    pub now_unix_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Plan {
    AddFinalizer,
    RemoveFinalizer,
    Allocate,
    CreateConfigMap(usize),
    CreateService,
    CreatePod(usize),
    LabelPod(usize),
    Freeze(Vec<Binding>),
    StartFm(FrozenCohort),
    ConfigureNode(usize),
    Route(BTreeMap<String, String>),
    Status(Box<KubericSetStatus>),
    BeginTeardown {
        cohort: FrozenCohort,
        service_uid: String,
    },
    AdvanceTeardown(TeardownStage),
    ShutdownFm(FrozenCohort),
    JoinFm(FrozenCohort),
    DeletePod(usize),
    DeleteConfigMap(usize),
    DeleteService,
    Wait,
    Blocked(String),
}

pub(crate) fn component(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 63
        && !matches!(value, "." | "..")
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_'))
}

fn allocation_template(set: &KubericSet, settings: &Settings) -> Result<Allocation> {
    let name = set.name_any();
    ensure!(
        !name.is_empty() && name.len() <= 40,
        "KubericSet name must have at most 40 characters"
    );
    ensure!(component(&name), "invalid KubericSet name");
    let fm = settings.fm.native()?;
    let mut node_id = u128::from_le_bytes(fm.identity.native().map_err(anyhow::Error::msg)?.id);
    let allocation = Allocation {
        resource_uid: set.uid().context("KubericSet has no UID")?,
        resource_name: name.clone(),
        namespace: settings.namespace.clone(),
        generation: set
            .metadata
            .generation
            .context("KubericSet has no generation")?,
        deployment: uuid::Uuid::from_u128(1).to_string(),
        partition: uuid::Uuid::from_u128(2).to_string(),
        fm: settings.fm.clone(),
        spec: set.spec.clone(),
        nodes: (1..=3)
            .map(|id| {
                node_id = node_id.wrapping_add(1);
                if node_id == 0 {
                    node_id = 1;
                }
                NodeAllocation {
                    node_id: format!("{node_id:x}"),
                    replica_id: id * 10,
                    pod_name: format!("{name}-node-{id}"),
                    config_map_name: format!("{name}-node-{id}"),
                }
            })
            .collect(),
    };
    validate_allocation(set, &allocation, settings, true)?;
    Ok(allocation)
}

pub(crate) fn new_allocation(set: &KubericSet, settings: &Settings) -> Result<Allocation> {
    let mut allocation = allocation_template(set, settings)?;
    allocation.deployment = uuid::Uuid::new_v4().to_string();
    allocation.partition = uuid::Uuid::new_v4().to_string();
    for node in &mut allocation.nodes {
        node.node_id = uuid::Uuid::new_v4().simple().to_string();
    }
    validate_allocation(set, &allocation, settings, true)?;
    Ok(allocation)
}

pub(crate) fn validate_allocation(
    set: &KubericSet,
    allocation: &Allocation,
    settings: &Settings,
    check_desired: bool,
) -> Result<()> {
    ensure!(
        set.namespace().as_deref() == Some(settings.namespace.as_str())
            && component(&settings.namespace),
        "KubericSet is outside the installation namespace"
    );
    ensure!(
        allocation.resource_uid == set.uid().context("KubericSet has no UID")?
            && component(&allocation.resource_uid)
            && allocation.resource_name == set.name_any()
            && allocation.namespace == settings.namespace,
        "allocation changed its Kubernetes resource identity"
    );
    if check_desired {
        ensure!(
            allocation.generation
                == set
                    .metadata
                    .generation
                    .context("KubericSet has no generation")?
                && allocation.spec == set.spec
                && allocation.fm == settings.fm,
            "resource spec/generation or native FM authority changed; replacement and rolling upgrade are unsupported"
        );
    }
    ensure!(
        allocation.spec.replicas == 3
            && !allocation.spec.image.is_empty()
            && allocation.spec.image.len() <= 1024
            && allocation.spec.client_port != 0
            && Path::new(&allocation.spec.package_path).is_absolute(),
        "only a fresh volatile 3/2 service with explicit image, package and ports is supported"
    );
    ensure!(
        (100..=600_000).contains(&settings.teardown_timeout_ms),
        "invalid teardown timeout"
    );
    for (name, value) in [("TargetReplicaSetSize", "3"), ("MinReplicaSetSize", "2")] {
        ensure!(
            allocation
                .spec
                .parameters
                .get(name)
                .is_none_or(|actual| actual == value),
            "unsupported {name}"
        );
    }
    ensure!(
        allocation.nodes.len() == 3,
        "allocation must contain three original nodes"
    );
    for (index, node) in allocation.nodes.iter().enumerate() {
        ensure!(
            node.pod_name == format!("{}-node-{}", set.name_any(), index + 1)
                && node.config_map_name == node.pod_name
                && node.replica_id == i64::try_from(index + 1)? * 10,
            "allocation changed original node slots"
        );
    }
    let cohort = FrozenCohort {
        allocation: allocation.clone(),
        bindings: (0..3)
            .map(|index| Binding {
                pod_uid: format!("validation-{index}"),
                config_map_uid: format!("validation-{index}"),
                ip: format!("127.0.0.{}", index + 2),
                control_port: settings.node_port,
            })
            .collect(),
    };
    cohort.fm_config()?;
    for index in 0..3 {
        resources::runner_config(&cohort, index)?;
    }
    Ok(())
}

pub(crate) fn admitted(observation: &Observation, allocation: &Allocation) -> Result<()> {
    ensure!(
        observation.fm_allocation.as_ref() == Some(allocation),
        "native FM backend has no original allocation admission; status cannot restore it"
    );
    ensure!(
        observation.process_allocation.as_ref() == Some(allocation),
        "process backend has no original allocation admission; cleanup ownership is unknown"
    );
    Ok(())
}

pub(crate) fn owned(metadata: &ObjectMeta, set: &KubericSet) -> bool {
    let Some(uid) = set.uid() else {
        return false;
    };
    metadata.owner_references.as_ref().is_some_and(|owners| {
        owners
            .iter()
            .filter(|owner| owner.controller == Some(true))
            .count()
            == 1
            && owners.iter().any(|owner| {
                owner.controller == Some(true)
                    && owner.uid == uid
                    && owner.name == set.name_any()
                    && owner.kind == "KubericSet"
                    && owner.api_version == "operator.kuberic.io/v1alpha2"
            })
    })
}

pub(crate) fn pod_spec_matches(actual: &PodSpec, expected: &PodSpec) -> bool {
    actual.restart_policy == expected.restart_policy
        && actual.containers.len() == expected.containers.len()
        && actual
            .containers
            .iter()
            .zip(&expected.containers)
            .all(|(actual, expected)| {
                actual.name == expected.name
                    && actual.image == expected.image
                    && actual.command == expected.command
                    && actual.args == expected.args
                    && actual.env == expected.env
                    && actual.volume_mounts == expected.volume_mounts
            })
        && match (&actual.volumes, &expected.volumes) {
            (Some(actual), Some(expected)) => {
                actual.len() == expected.len()
                    && expected.iter().all(|expected| {
                        actual.iter().any(|actual| {
                            actual.name == expected.name
                                && match (&expected.config_map, &expected.empty_dir) {
                                    (Some(expected), None) => {
                                        actual.empty_dir.is_none()
                                            && actual.config_map.as_ref().is_some_and(|actual| {
                                                actual.name == expected.name
                                                    && actual.items == expected.items
                                                    && actual.optional != Some(true)
                                            })
                                    }
                                    (None, Some(expected)) => {
                                        actual.config_map.is_none()
                                            && actual.empty_dir.as_ref() == Some(expected)
                                    }
                                    _ => false,
                                }
                        })
                    })
            }
            _ => false,
        }
}

pub(crate) fn metadata(
    metadata: &ObjectMeta,
    set: &KubericSet,
    allocation: &Allocation,
    allow_deleting: bool,
) -> Result<()> {
    ensure!(
        owned(metadata, set),
        "resource name collides with foreign ownership"
    );
    ensure!(
        (allow_deleting || metadata.deletion_timestamp.is_none())
            && metadata.labels.as_ref().is_some_and(|labels| {
                labels.get(SET_UID) == Some(&allocation.resource_uid)
                    && labels.get(DEPLOYMENT) == Some(&allocation.deployment)
            })
            && metadata
                .annotations
                .as_ref()
                .and_then(|annotations| annotations.get(SPEC))
                == Some(&serde_json::to_string(&allocation.spec)?),
        "owned resource is deleting or its frozen metadata changed"
    );
    Ok(())
}

pub fn evaluate(observation: &Observation, settings: &Settings) -> Plan {
    match evaluate_inner(observation, settings) {
        Ok(plan) => plan,
        Err(error) => Plan::Blocked(format!("{error:#}")),
    }
}

fn evaluate_inner(observation: &Observation, settings: &Settings) -> Result<Plan> {
    let set = &observation.set;
    ensure!(
        observation.pods.len() == 3 && observation.config_maps.len() == 3,
        "incomplete resource observation"
    );
    if set.metadata.deletion_timestamp.is_some() {
        return crate::teardown::evaluate(observation, settings);
    }
    if let Some(status) = &set.status {
        if status.phase == ControllerPhase::Blocked {
            return Ok(Plan::Blocked(
                status
                    .reason
                    .clone()
                    .context("blocked status has no reason")?,
            ));
        }
        ensure!(
            status.teardown.is_none(),
            "retained teardown cannot return to bootstrap"
        );
    }
    if !set
        .finalizers()
        .iter()
        .any(|finalizer| finalizer == resources::FINALIZER)
    {
        ensure!(
            set.status
                .as_ref()
                .and_then(|status| status.allocation.as_ref())
                .is_none(),
            "owned finalizer disappeared after allocation"
        );
        allocation_template(set, settings)?;
        return Ok(Plan::AddFinalizer);
    }
    let Some(allocation) = set
        .status
        .as_ref()
        .and_then(|status| status.allocation.as_ref())
    else {
        ensure!(
            observation.pods.iter().all(Option::is_none)
                && observation.config_maps.iter().all(Option::is_none)
                && observation.service.is_none()
                && observation.fm.is_none(),
            "existing resources or FM session have no accepted allocation; adoption is unsupported"
        );
        if let Some(draft) = &observation.fm_allocation {
            validate_allocation(set, draft, settings, true)?;
            ensure!(
                observation
                    .process_allocation
                    .as_ref()
                    .is_none_or(|allocation| allocation == draft),
                "backends retained different fresh allocation drafts"
            );
        } else {
            ensure!(
                observation.process_allocation.is_none(),
                "native FM draft is unavailable; a process backend cannot restore it"
            );
            allocation_template(set, settings)?;
        }
        return Ok(Plan::Allocate);
    };
    validate_allocation(set, allocation, settings, true)?;
    admitted(observation, allocation)?;
    let status = set.status.as_ref().context("missing accepted status")?;
    let cohort = status.cohort();
    if let Some(cohort) = &cohort {
        if let Some(nodes) = crate::teardown::process_nodes(observation, cohort)? {
            ensure!(
                nodes
                    .iter()
                    .all(|node| matches!(node.outcome, crate::CleanupState::Pending)),
                "owned process exited outside retained teardown"
            );
        }
    }
    if let Some(report) = &observation.fm {
        ensure!(
            cohort.as_ref() == Some(&report.cohort),
            "FM report belongs to a different frozen cohort"
        );
        ensure!(
            report.status.deployment == allocation.deployment
                && report.status.primary_node_id == allocation.nodes[1].node_id,
            "native status changed deployment or initial primary"
        );
        ensure!(
            report.status.phase != Phase::ControlUnknown && report.status.error.is_none(),
            "native FM control uncertainty: {}",
            report
                .status
                .error
                .as_deref()
                .unwrap_or("unknown native authority")
        );
    }
    for (index, pod) in observation.pods.iter().enumerate() {
        if let Some(pod) = pod {
            metadata(&pod.metadata, set, allocation, false)?;
            let desired = resources::pod(set, allocation, index)?;
            let actual = pod.spec.as_ref().context("owned Pod has no spec")?;
            let expected = desired.spec.as_ref().context("missing desired Pod spec")?;
            ensure!(
                pod_spec_matches(actual, expected)
                    && pod.labels().get(NODE_ID) == Some(&allocation.nodes[index].node_id)
                    && pod.status.as_ref().is_none_or(|status| {
                        !matches!(status.phase.as_deref(), Some("Failed" | "Succeeded"))
                            && status.container_statuses.as_ref().is_none_or(|containers| {
                                containers
                                    .iter()
                                    .all(|container| container.restart_count == 0)
                            })
                    }),
                "original Pod spec, process lifetime or node identity changed"
            );
        }
    }
    for map in observation.config_maps.iter().flatten() {
        metadata(&map.metadata, set, allocation, false)?;
        ensure!(
            map.binary_data.as_ref().is_none_or(BTreeMap::is_empty),
            "unexpected binary node configuration"
        );
    }
    if let Some(service) = &observation.service {
        metadata(&service.metadata, set, allocation, false)?;
        let expected = resources::service(set, allocation)?;
        ensure!(
            service
                .spec
                .as_ref()
                .context("owned Service has no spec")?
                .ports
                == expected.spec.context("missing desired Service spec")?.ports
                && service
                    .spec
                    .as_ref()
                    .is_some_and(|spec| spec.type_.as_deref() == Some("ClusterIP")),
            "client Service ports changed"
        );
    }
    if let Some(cohort) = &cohort {
        ensure!(cohort.bindings.len() == 3, "incomplete frozen Pod bindings");
        for (index, binding) in cohort.bindings.iter().enumerate() {
            let pod = observation.pods[index]
                .as_ref()
                .context("original Pod is missing; replacement is unsupported")?;
            let map = observation.config_maps[index]
                .as_ref()
                .context("original ConfigMap is missing")?;
            ensure!(
                component(&binding.pod_uid)
                    && component(&binding.config_map_uid)
                    && pod.uid().as_deref() == Some(&binding.pod_uid)
                    && map.uid().as_deref() == Some(&binding.config_map_uid)
                    && pod
                        .status
                        .as_ref()
                        .and_then(|status| status.pod_ip.as_deref())
                        == Some(&binding.ip)
                    && pod.labels().get(POD_UID) == Some(&binding.pod_uid),
                "original Pod/ConfigMap UID or Pod IP changed"
            );
        }
        cohort.fm_config()?;
    } else {
        ensure!(
            observation.fm.is_none(),
            "FM started before Pod bindings were accepted"
        );
        for map in observation.config_maps.iter().flatten() {
            ensure!(
                map.data.as_ref().is_none_or(BTreeMap::is_empty) && map.immutable != Some(true),
                "node configuration was published before exact Pod binding"
            );
        }
    }
    if let Some(index) = observation.config_maps.iter().position(Option::is_none) {
        return Ok(Plan::CreateConfigMap(index));
    }
    if observation.service.is_none() {
        return Ok(Plan::CreateService);
    }
    if let Some(index) = observation.pods.iter().position(Option::is_none) {
        return Ok(Plan::CreatePod(index));
    }
    let Some(cohort) = cohort else {
        for (index, pod) in observation.pods.iter().enumerate() {
            let pod = pod.as_ref().context("missing Pod")?;
            let uid = pod.uid().context("Pod has no UID")?;
            ensure!(component(&uid), "invalid Pod UID");
            if pod.labels().get(POD_UID) != Some(&uid) {
                return Ok(Plan::LabelPod(index));
            }
        }
        let mut bindings = Vec::new();
        for (pod, map) in observation.pods.iter().zip(&observation.config_maps) {
            let pod = pod.as_ref().context("missing Pod")?;
            let Some(ip) = pod
                .status
                .as_ref()
                .and_then(|status| status.pod_ip.as_ref())
            else {
                return Ok(Plan::Wait);
            };
            bindings.push(Binding {
                pod_uid: pod.uid().context("Pod has no UID")?,
                config_map_uid: map
                    .as_ref()
                    .context("missing ConfigMap")?
                    .uid()
                    .context("ConfigMap has no UID")?,
                ip: ip.clone(),
                control_port: settings.node_port,
            });
        }
        FrozenCohort {
            allocation: allocation.clone(),
            bindings: bindings.clone(),
        }
        .fm_config()?;
        return Ok(Plan::Freeze(bindings));
    };
    let Some(report) = &observation.fm else {
        return Ok(Plan::StartFm(cohort));
    };
    for (index, map) in observation.config_maps.iter().enumerate() {
        let map = map.as_ref().context("missing ConfigMap")?;
        let expected = BTreeMap::from([(
            "node.json".into(),
            serde_json::to_string(&resources::runner_config(&cohort, index)?)?,
        )]);
        if map.data.as_ref() == Some(&expected) && map.immutable == Some(true) {
            continue;
        }
        ensure!(
            map.data.as_ref().is_none_or(BTreeMap::is_empty) && map.immutable != Some(true),
            "frozen node configuration changed"
        );
        return Ok(Plan::ConfigureNode(index));
    }
    let serving = report.status.phase == Phase::Serving;
    ensure!(
        !serving
            || report
                .status
                .service_endpoint
                .as_ref()
                .is_some_and(|endpoint| !endpoint.is_empty()),
        "serving FM report has no native service endpoint"
    );
    let selector = if serving {
        BTreeMap::from([
            (SET_UID.into(), allocation.resource_uid.clone()),
            (DEPLOYMENT.into(), allocation.deployment.clone()),
            (NODE_ID.into(), allocation.nodes[1].node_id.clone()),
            (POD_UID.into(), cohort.bindings[1].pod_uid.clone()),
        ])
    } else {
        resources::disabled_selector(set)?
    };
    if observation
        .service
        .as_ref()
        .and_then(|service| service.spec.as_ref())
        .and_then(|spec| spec.selector.as_ref())
        != Some(&selector)
    {
        return Ok(Plan::Route(selector));
    }
    let mut next = status.clone();
    next.phase = match report.status.phase {
        Phase::Serving => ControllerPhase::Serving,
        Phase::Deleting | Phase::AwaitingNodeShutdown => ControllerPhase::Stopping,
        Phase::Stopped => ControllerPhase::Stopped,
        _ => ControllerPhase::Starting,
    };
    next.service_endpoint = report.status.service_endpoint.clone().filter(|_| serving);
    next.reason = None;
    if &next != status {
        return Ok(Plan::Status(Box::new(next)));
    }
    Ok(Plan::Wait)
}
