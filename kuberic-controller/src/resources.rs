use std::{collections::BTreeMap, path::PathBuf};

use anyhow::{Context, Result};
use k8s_openapi::{
    api::core::v1::{
        ConfigMap, ConfigMapVolumeSource, Container, EnvVar, Pod, PodSpec, Service, ServicePort,
        ServiceSpec, Volume, VolumeMount,
    },
    apimachinery::pkg::{apis::meta::v1::OwnerReference, util::intstr::IntOrString},
};
use kube::{api::ObjectMeta, ResourceExt};

use crate::{Allocation, FrozenCohort, KubericSet};

pub const SET_UID: &str = "operator.kuberic.io/set-uid";
pub const NODE_ID: &str = "operator.kuberic.io/node-id";
pub const POD_UID: &str = "operator.kuberic.io/pod-uid";
pub const DEPLOYMENT: &str = "operator.kuberic.io/deployment";
pub const SPEC: &str = "operator.kuberic.io/frozen-spec";
pub const CONTROLLER: &str = "kuberic-controller";
pub const FINALIZER: &str = "operator.kuberic.io/owned-teardown";
pub const RUN_ROOT: &str = "/run/fube";

pub fn write_service_name(name: &str) -> String {
    format!("{name}-write")
}

pub fn disabled_selector(set: &KubericSet) -> Result<BTreeMap<String, String>> {
    Ok(BTreeMap::from([
        (SET_UID.into(), set.uid().context("KubericSet has no UID")?),
        (NODE_ID.into(), "disabled".into()),
    ]))
}

pub fn owner(set: &KubericSet) -> Result<OwnerReference> {
    Ok(OwnerReference {
        api_version: "operator.kuberic.io/v1alpha2".into(),
        kind: "KubericSet".into(),
        name: set.name_any(),
        uid: set.uid().context("KubericSet has no UID")?,
        controller: Some(true),
        block_owner_deletion: Some(true),
    })
}

fn metadata(set: &KubericSet, allocation: &Allocation, name: &str) -> Result<ObjectMeta> {
    Ok(ObjectMeta {
        name: Some(name.into()),
        namespace: set.namespace(),
        owner_references: Some(vec![owner(set)?]),
        labels: Some(BTreeMap::from([
            (SET_UID.into(), allocation.resource_uid.clone()),
            (DEPLOYMENT.into(), allocation.deployment.clone()),
        ])),
        annotations: Some(BTreeMap::from([(
            SPEC.into(),
            serde_json::to_string(&allocation.spec)?,
        )])),
        ..ObjectMeta::default()
    })
}

pub fn config_map(set: &KubericSet, allocation: &Allocation, node: usize) -> Result<ConfigMap> {
    Ok(ConfigMap {
        metadata: metadata(set, allocation, &allocation.nodes[node].config_map_name)?,
        data: Some(BTreeMap::new()),
        ..ConfigMap::default()
    })
}

pub fn pod(set: &KubericSet, allocation: &Allocation, node: usize) -> Result<Pod> {
    let assigned = &allocation.nodes[node];
    let mut metadata = metadata(set, allocation, &assigned.pod_name)?;
    metadata
        .labels
        .get_or_insert_default()
        .insert(NODE_ID.into(), assigned.node_id.clone());
    let parent = PathBuf::from(RUN_ROOT).join(&allocation.deployment);
    Ok(Pod {
        metadata,
        spec: Some(PodSpec {
            automount_service_account_token: Some(false),
            restart_policy: Some("Never".into()),
            containers: vec![Container {
                name: "node".into(),
                image: Some(allocation.spec.image.clone()),
                command: Some(vec!["/sbin/tini".into(), "-g".into(), "--".into(), "/bin/sh".into(), "-ec".into()]),
                args: Some(vec![
                    "while [ ! -s /etc/fube/node.json ]; do sleep 1; done\nmkdir -p \"$KUBERIC_RUN_PARENT\"\nexec /opt/fube/fube-node --config /etc/fube/node.json".into()
                ]),
                env: Some(vec![EnvVar {
                    name: "KUBERIC_RUN_PARENT".into(),
                    value: Some(parent.to_str().context("run root is not UTF-8")?.into()),
                    ..EnvVar::default()
                }]),
                volume_mounts: Some(vec![
                    VolumeMount { name: "config".into(), mount_path: "/etc/fube".into(), read_only: Some(true), ..VolumeMount::default() },
                    VolumeMount { name: "run".into(), mount_path: RUN_ROOT.into(), ..VolumeMount::default() },
                ]),
                ..Container::default()
            }],
            volumes: Some(vec![
                Volume {
                    name: "config".into(),
                    config_map: Some(ConfigMapVolumeSource {
                        name: assigned.config_map_name.clone(),
                        ..ConfigMapVolumeSource::default()
                    }),
                    ..Volume::default()
                },
                Volume {
                    name: "run".into(),
                    empty_dir: Some(Default::default()),
                    ..Volume::default()
                },
            ]),
            ..PodSpec::default()
        }),
        ..Pod::default()
    })
}

pub fn service(set: &KubericSet, allocation: &Allocation) -> Result<Service> {
    Ok(Service {
        metadata: metadata(set, allocation, &write_service_name(&set.name_any()))?,
        spec: Some(ServiceSpec {
            selector: Some(disabled_selector(set)?),
            type_: Some("ClusterIP".into()),
            ports: Some(vec![ServicePort {
                name: Some("client".into()),
                port: i32::from(allocation.spec.client_port),
                target_port: Some(IntOrString::Int(i32::from(allocation.spec.client_port))),
                protocol: Some("TCP".into()),
                ..ServicePort::default()
            }]),
            ..ServiceSpec::default()
        }),
        ..Service::default()
    })
}

pub fn runner_config(cohort: &FrozenCohort, index: usize) -> Result<fube_node_runner::Config> {
    let fm = cohort.fm_config()?;
    let allocation = &cohort.allocation;
    let node = fm.nodes.get(index).context("foreign node index")?;
    let binding = &cohort.bindings[index];
    let mut parameters = allocation.spec.parameters.clone();
    parameters.insert("TargetReplicaSetSize".into(), "3".into());
    parameters.insert("MinReplicaSetSize".into(), "2".into());
    let config = fube_node_runner::Config {
        schema_version: 1,
        deployment: allocation.deployment.clone(),
        name: allocation.nodes[index].pod_name.clone(),
        node: node.clone(),
        fm: fm.fm,
        peers: fm
            .nodes
            .iter()
            .enumerate()
            .filter(|(peer, _)| *peer != index)
            .map(|(_, peer)| peer.clone())
            .collect(),
        partition: allocation.partition.clone(),
        package: PathBuf::from(&allocation.spec.package_path),
        application: allocation.spec.service.application.clone(),
        package_version_instance: allocation.spec.service.package_version_instance,
        parameters: parameters
            .into_iter()
            .map(|(name, value)| (name, value.replace("{node_ip}", &binding.ip)))
            .collect(),
        run_directory: PathBuf::from(RUN_ROOT)
            .join(&allocation.deployment)
            .join(&binding.pod_uid),
        startup_timeout_ms: 30_000,
        shutdown_timeout_ms: 20_000,
    };
    config.validate().map_err(anyhow::Error::msg)?;
    Ok(config)
}
