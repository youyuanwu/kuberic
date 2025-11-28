//! StatefulSet and related resource management
//!
//! This module contains functions to create and manage Kubernetes resources
//! for the xdata-app StatefulSet, including Services, ServiceAccount, and RBAC.

use crate::crd::{XdataApp, XdataAppSpec};
use k8s_openapi::api::apps::v1::{StatefulSet, StatefulSetSpec};
use k8s_openapi::api::core::v1::{
    Container, ContainerPort, EnvVar, PersistentVolumeClaim, PersistentVolumeClaimSpec,
    PodSecurityContext, PodSpec, PodTemplateSpec, Probe, ResourceRequirements, SeccompProfile,
    SecurityContext, Service, ServiceAccount, ServicePort, ServiceSpec, TCPSocketAction,
    VolumeMount,
};
use k8s_openapi::api::rbac::v1::{PolicyRule, Role, RoleBinding, RoleRef, Subject};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{LabelSelector, ObjectMeta, OwnerReference};
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;
use kube::ResourceExt;
use std::collections::BTreeMap;

const APP_NAME: &str = "xdata-app";

/// Create an owner reference for the XdataApp
fn create_owner_reference(xapp: &XdataApp) -> OwnerReference {
    OwnerReference {
        api_version: "xedio.io/v1".to_string(),
        kind: "XdataApp".to_string(),
        name: xapp.name_any(),
        uid: xapp.metadata.uid.clone().unwrap_or_default(),
        controller: Some(true),
        block_owner_deletion: Some(true),
    }
}

/// Build a StatefulSet from XdataApp spec
pub fn build_statefulset(xapp: &XdataApp) -> StatefulSet {
    let name = xapp.metadata.name.as_ref().unwrap();
    let namespace = xapp.metadata.namespace.as_ref().unwrap();
    let spec = &xapp.spec;

    let mut labels = BTreeMap::new();
    labels.insert("app".to_string(), APP_NAME.to_string());
    labels.insert("xdataapp".to_string(), name.clone());

    StatefulSet {
        metadata: ObjectMeta {
            name: Some(name.clone()),
            namespace: Some(namespace.clone()),
            labels: Some(labels.clone()),
            owner_references: Some(vec![create_owner_reference(xapp)]),
            ..Default::default()
        },
        spec: Some(StatefulSetSpec {
            service_name: Some(APP_NAME.to_string()),
            replicas: Some(spec.replicas),
            selector: LabelSelector {
                match_labels: Some(labels.clone()),
                ..Default::default()
            },
            template: build_pod_template(spec, &labels),
            volume_claim_templates: Some(vec![build_volume_claim_template(spec)]),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// Build the pod template for the StatefulSet
fn build_pod_template(spec: &XdataAppSpec, labels: &BTreeMap<String, String>) -> PodTemplateSpec {
    PodTemplateSpec {
        metadata: Some(ObjectMeta {
            labels: Some(labels.clone()),
            ..Default::default()
        }),
        spec: Some(PodSpec {
            service_account_name: Some(APP_NAME.to_string()),
            containers: vec![build_container(spec)],
            security_context: Some(PodSecurityContext {
                seccomp_profile: Some(SeccompProfile {
                    type_: "RuntimeDefault".to_string(),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        }),
    }
}

/// Build the container spec
fn build_container(spec: &XdataAppSpec) -> Container {
    let mut env_vars = Vec::new();

    // Add custom environment variables from spec
    for env in &spec.env {
        env_vars.push(EnvVar {
            name: env.name.clone(),
            value: Some(env.value.clone()),
            ..Default::default()
        });
    }

    // Add RUST_LOG default only if not already specified
    if !env_vars.iter().any(|e| e.name == "RUST_LOG") {
        env_vars.insert(
            0,
            EnvVar {
                name: "RUST_LOG".to_string(),
                value: Some("info".to_string()),
                ..Default::default()
            },
        );
    }

    Container {
        name: APP_NAME.to_string(),
        image: Some(spec.image.clone()),
        image_pull_policy: Some(spec.image_pull_policy.clone()),
        env: Some(env_vars),
        ports: Some(vec![ContainerPort {
            container_port: spec.port,
            name: Some("http".to_string()),
            protocol: Some("TCP".to_string()),
            ..Default::default()
        }]),
        volume_mounts: Some(vec![VolumeMount {
            name: "data".to_string(),
            mount_path: "/data".to_string(),
            ..Default::default()
        }]),
        liveness_probe: Some(build_liveness_probe(spec.port)),
        readiness_probe: Some(build_readiness_probe(spec.port)),
        resources: Some(build_resource_requirements(spec)),
        security_context: Some(SecurityContext {
            run_as_non_root: Some(true),
            run_as_user: Some(10000),
            allow_privilege_escalation: Some(false),
            read_only_root_filesystem: Some(true),
            capabilities: Some(k8s_openapi::api::core::v1::Capabilities {
                drop: Some(vec!["ALL".to_string()]),
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// Build liveness probe
fn build_liveness_probe(port: i32) -> Probe {
    Probe {
        tcp_socket: Some(TCPSocketAction {
            port: IntOrString::Int(port),
            ..Default::default()
        }),
        initial_delay_seconds: Some(10),
        period_seconds: Some(10),
        timeout_seconds: Some(5),
        failure_threshold: Some(3),
        ..Default::default()
    }
}

/// Build readiness probe
fn build_readiness_probe(port: i32) -> Probe {
    Probe {
        tcp_socket: Some(TCPSocketAction {
            port: IntOrString::Int(port),
            ..Default::default()
        }),
        initial_delay_seconds: Some(5),
        period_seconds: Some(5),
        timeout_seconds: Some(3),
        failure_threshold: Some(2),
        ..Default::default()
    }
}

/// Build resource requirements
fn build_resource_requirements(spec: &XdataAppSpec) -> ResourceRequirements {
    let mut requests = BTreeMap::new();
    requests.insert(
        "memory".to_string(),
        Quantity(spec.resources.requests.memory.clone()),
    );
    requests.insert(
        "cpu".to_string(),
        Quantity(spec.resources.requests.cpu.clone()),
    );

    let mut limits = BTreeMap::new();
    limits.insert(
        "memory".to_string(),
        Quantity(spec.resources.limits.memory.clone()),
    );
    limits.insert(
        "cpu".to_string(),
        Quantity(spec.resources.limits.cpu.clone()),
    );

    ResourceRequirements {
        requests: Some(requests),
        limits: Some(limits),
        ..Default::default()
    }
}

/// Build volume claim template
fn build_volume_claim_template(spec: &XdataAppSpec) -> PersistentVolumeClaim {
    let mut resources = BTreeMap::new();
    resources.insert("storage".to_string(), Quantity(spec.storage.clone()));

    PersistentVolumeClaim {
        metadata: ObjectMeta {
            name: Some("data".to_string()),
            ..Default::default()
        },
        spec: Some(PersistentVolumeClaimSpec {
            access_modes: Some(vec!["ReadWriteOnce".to_string()]),
            resources: Some(k8s_openapi::api::core::v1::VolumeResourceRequirements {
                requests: Some(resources),
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// Build the headless Service for the StatefulSet
pub fn build_headless_service(xapp: &XdataApp) -> Service {
    let namespace = xapp.metadata.namespace.as_ref().unwrap();

    let mut labels = BTreeMap::new();
    labels.insert("app".to_string(), APP_NAME.to_string());

    Service {
        metadata: ObjectMeta {
            name: Some(APP_NAME.to_string()),
            namespace: Some(namespace.clone()),
            labels: Some(labels.clone()),
            owner_references: Some(vec![create_owner_reference(xapp)]),
            ..Default::default()
        },
        spec: Some(ServiceSpec {
            cluster_ip: Some("None".to_string()),
            ports: Some(vec![ServicePort {
                port: 80,
                target_port: Some(IntOrString::Int(xapp.spec.port)),
                protocol: Some("TCP".to_string()),
                name: Some("http".to_string()),
                ..Default::default()
            }]),
            selector: Some(labels),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// Build NodePort services for individual pods
pub fn build_nodeport_services(xapp: &XdataApp) -> Vec<Service> {
    if !xapp.spec.node_port_enabled {
        return vec![];
    }

    let name = xapp.metadata.name.as_ref().unwrap();
    let namespace = xapp.metadata.namespace.as_ref().unwrap();
    let replicas = xapp.spec.replicas;
    let base_port = xapp.spec.node_port_base;

    (0..replicas)
        .map(|i| {
            let pod_name = format!("{}-{}", name, i);
            let node_port = base_port + i;

            let mut labels = BTreeMap::new();
            labels.insert("app".to_string(), APP_NAME.to_string());

            let mut selector = BTreeMap::new();
            selector.insert("app".to_string(), APP_NAME.to_string());
            selector.insert(
                "statefulset.kubernetes.io/pod-name".to_string(),
                pod_name.clone(),
            );

            Service {
                metadata: ObjectMeta {
                    name: Some(pod_name.clone()),
                    namespace: Some(namespace.clone()),
                    labels: Some(labels),
                    owner_references: Some(vec![create_owner_reference(xapp)]),
                    ..Default::default()
                },
                spec: Some(ServiceSpec {
                    type_: Some("NodePort".to_string()),
                    ports: Some(vec![ServicePort {
                        port: 80,
                        target_port: Some(IntOrString::Int(xapp.spec.port)),
                        node_port: Some(node_port),
                        protocol: Some("TCP".to_string()),
                        name: Some("http".to_string()),
                        ..Default::default()
                    }]),
                    selector: Some(selector),
                    ..Default::default()
                }),
                ..Default::default()
            }
        })
        .collect()
}

/// Build ServiceAccount for xdata-app
pub fn build_service_account(xapp: &XdataApp) -> ServiceAccount {
    let namespace = xapp.metadata.namespace.as_ref().unwrap();

    ServiceAccount {
        metadata: ObjectMeta {
            name: Some(APP_NAME.to_string()),
            namespace: Some(namespace.clone()),
            owner_references: Some(vec![create_owner_reference(xapp)]),
            ..Default::default()
        },
        ..Default::default()
    }
}

/// Build Role for leader election
pub fn build_role(xapp: &XdataApp) -> Role {
    let namespace = xapp.metadata.namespace.as_ref().unwrap();

    Role {
        metadata: ObjectMeta {
            name: Some(format!("{}-leader-election", APP_NAME)),
            namespace: Some(namespace.clone()),
            owner_references: Some(vec![create_owner_reference(xapp)]),
            ..Default::default()
        },
        rules: Some(vec![PolicyRule {
            api_groups: Some(vec!["coordination.k8s.io".to_string()]),
            resources: Some(vec!["leases".to_string()]),
            verbs: vec![
                "get".to_string(),
                "create".to_string(),
                "update".to_string(),
                "patch".to_string(),
                "delete".to_string(),
            ],
            ..Default::default()
        }]),
    }
}

/// Build RoleBinding for leader election
pub fn build_role_binding(xapp: &XdataApp) -> RoleBinding {
    let namespace = xapp.metadata.namespace.as_ref().unwrap();

    RoleBinding {
        metadata: ObjectMeta {
            name: Some(format!("{}-leader-election", APP_NAME)),
            namespace: Some(namespace.clone()),
            owner_references: Some(vec![create_owner_reference(xapp)]),
            ..Default::default()
        },
        subjects: Some(vec![Subject {
            kind: "ServiceAccount".to_string(),
            name: APP_NAME.to_string(),
            namespace: Some(namespace.clone()),
            ..Default::default()
        }]),
        role_ref: RoleRef {
            api_group: "rbac.authorization.k8s.io".to_string(),
            kind: "Role".to_string(),
            name: format!("{}-leader-election", APP_NAME),
        },
    }
}
