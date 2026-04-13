use std::collections::{BTreeMap, HashMap};
use std::time::Duration;

use k8s_openapi::api::core::v1::{
    Container, ContainerPort, PersistentVolumeClaim, PersistentVolumeClaimSpec, Pod, PodSpec,
    Probe, Service, ServicePort, ServiceSpec, TCPSocketAction, VolumeResourceRequirements,
};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;
use kube::ResourceExt;
use tokio::sync::Mutex;
use tracing::{info, warn};

use kuberic_core::driver::{PartitionDriver, ReplicaHandle};
use kuberic_core::types::ReplicaId;

use crate::cluster_api::ClusterApi;
use crate::crd::{
    EpochStatus, KubericSet, KubericSetSpec, KubericSetStatus, MemberStatus, Phase,
    ReconfigurationPhase,
};

/// Shared state across reconciliation loops.
pub struct ReconcilerState {
    /// Per-set partition drivers, keyed by "{namespace}/{name}".
    pub drivers: Mutex<HashMap<String, PartitionDriver>>,
}

impl Default for ReconcilerState {
    fn default() -> Self {
        Self {
            drivers: Mutex::new(HashMap::new()),
        }
    }
}

/// Result of a reconciliation — either requeue after a duration, or done.
pub enum ReconcileAction {
    Requeue(Duration),
}

/// Main reconciliation logic, decoupled from kube-runtime.
/// Takes a ClusterApi trait object so it can be tested without a real cluster.
pub async fn reconcile_set(
    set: &KubericSet,
    api: &dyn ClusterApi,
    state: &ReconcilerState,
) -> Result<ReconcileAction, String> {
    let name = set.name_any();
    let namespace = set.namespace().unwrap_or_default();

    info!(name, namespace, "reconciling KubericSet");

    let label_selector = format!("kuberic.io/set={}", name);
    let pods = api.list_pods(&namespace, &label_selector).await?;

    let ready_pods: Vec<&Pod> = pods.iter().filter(|p| is_pod_ready(p)).collect();

    let current_phase = set
        .status
        .as_ref()
        .map(|s| s.phase.clone())
        .unwrap_or_default();

    match current_phase {
        Phase::Pending => {
            info!(name, "creating partition pods and services");
            create_services(api, set, &namespace).await?;
            create_pods(api, set, &namespace).await?;
            let status = KubericSetStatus {
                phase: Phase::Creating,
                ..Default::default()
            };
            api.patch_set_status(&namespace, &name, &status).await?;
            Ok(ReconcileAction::Requeue(Duration::from_secs(5)))
        }

        Phase::Creating => {
            let desired = set.spec.replicas as usize;
            if ready_pods.len() < desired {
                info!(name, ready = ready_pods.len(), desired, "waiting for pods");
                return Ok(ReconcileAction::Requeue(Duration::from_secs(5)));
            }

            info!(name, "all pods ready, initializing partition via driver");

            // Create ReplicaHandles
            let mut handles: Vec<Box<dyn ReplicaHandle>> = Vec::new();
            for (idx, pod) in pods.iter().enumerate() {
                let replica_id = idx as ReplicaId + 1;
                match api.create_replica_handle(replica_id, pod, &set.spec).await {
                    Ok(handle) => handles.push(handle),
                    Err(e) => {
                        warn!(pod = pod.name_any(), error = %e, "failed to create handle");
                        return Ok(ReconcileAction::Requeue(Duration::from_secs(5)));
                    }
                }
            }

            // Run driver create_partition
            let mut driver = PartitionDriver::new();
            driver
                .create_partition(handles)
                .await
                .map_err(|e| e.to_string())?;

            // Update pod labels
            if let Some(primary_id) = driver.primary_id() {
                for member_id in driver.replica_ids() {
                    let pod_name = format!("{}-{}", name, member_id - 1);
                    let role = if member_id == primary_id {
                        "primary"
                    } else {
                        "secondary"
                    };
                    let mut labels = BTreeMap::new();
                    labels.insert("kuberic.io/role".to_string(), role.to_string());
                    let _ = api.patch_pod_labels(&namespace, &pod_name, labels).await;
                }
            }

            // Update CRD status
            let epoch = driver.epoch();
            let primary_name = driver.primary_id().map(|id| format!("{}-{}", name, id - 1));
            let members = build_member_status(&pods, &set.spec);

            let status = KubericSetStatus {
                epoch: EpochStatus {
                    data_loss_number: epoch.data_loss_number,
                    configuration_number: epoch.configuration_number,
                },
                current_primary: primary_name.clone(),
                target_primary: primary_name,
                phase: Phase::Healthy,
                reconfiguration_phase: ReconfigurationPhase::None,
                ready_replicas: ready_pods.len() as i32,
                replicas: pods.len() as i32,
                members,
                primary_failing_since: None,
            };
            api.patch_set_status(&namespace, &name, &status).await?;

            // Store driver
            let set_key = format!("{}/{}", namespace, name);
            state.drivers.lock().await.insert(set_key, driver);

            Ok(ReconcileAction::Requeue(Duration::from_secs(30)))
        }

        Phase::Healthy => {
            let current_primary = set.status.as_ref().and_then(|s| s.current_primary.clone());
            let desired = set.spec.replicas as usize;
            let actual = pods.len();
            let set_key = format!("{}/{}", namespace, name);

            // --- Replica health check (primary + secondaries) ---
            // Probe all replicas via get_status to detect crashed/restarted pods.
            // Must run BEFORE switchover — don't switchover to a stale target.
            if let Some(driver) = state.drivers.lock().await.get_mut(&set_key) {
                let current_epoch = driver.epoch();
                let primary_id = driver.primary_id();
                let replica_ids = driver.replica_ids();

                let mut stale_ids: Vec<ReplicaId> = Vec::new();
                let mut primary_stale = false;

                for &replica_id in &replica_ids {
                    if let Some(handle) = driver.handle(replica_id) {
                        let is_stale = match handle.get_status().await {
                            Ok(s) if s.epoch != current_epoch => {
                                warn!(name, replica_id, ?current_epoch,
                                    actual_epoch = ?s.epoch,
                                    "replica epoch mismatch — pod restarted");
                                true
                            }
                            Ok(s) if s.role == kuberic_core::types::Role::Unknown => {
                                warn!(name, replica_id, "replica role=Unknown — pod restarted");
                                true
                            }
                            Err(e) => {
                                warn!(name, replica_id, error = %e,
                                    "replica unreachable — stale handle");
                                true
                            }
                            Ok(_) => false,
                        };

                        if is_stale {
                            if Some(replica_id) == primary_id {
                                primary_stale = true;
                            } else {
                                stale_ids.push(replica_id);
                            }
                        }
                    }
                }

                // Also check K8s-level readiness for primary
                if !primary_stale {
                    if let Some(ref primary_name) = current_primary {
                        let primary_ready = pods
                            .iter()
                            .find(|p| p.name_any() == *primary_name)
                            .map(is_pod_ready)
                            .unwrap_or(false);
                        if !primary_ready {
                            primary_stale = true;
                        }
                    }
                }

                // Primary stale → failover (takes priority over everything)
                if primary_stale {
                    warn!(name, "primary unhealthy, initiating failover");
                    // Also remove any stale secondaries before failover
                    for &id in &stale_ids {
                        info!(
                            name,
                            replica_id = id,
                            "removing stale secondary before failover"
                        );
                        driver.remove_replica_from_driver(id);
                    }
                    let status = KubericSetStatus {
                        phase: Phase::FailingOver,
                        ..set.status.clone().unwrap_or_default()
                    };
                    api.patch_set_status(&namespace, &name, &status).await?;
                    return Ok(ReconcileAction::Requeue(Duration::from_secs(1)));
                }

                // Stale secondaries → remove and requeue
                if !stale_ids.is_empty() {
                    for &id in &stale_ids {
                        info!(name, replica_id = id, "removing stale secondary handle");
                        driver.remove_replica_from_driver(id);
                    }
                    let status = KubericSetStatus {
                        ready_replicas: driver.replica_ids().len() as i32,
                        replicas: pods.len() as i32,
                        members: build_member_status(&pods, &set.spec),
                        ..set.status.clone().unwrap_or_default()
                    };
                    api.patch_set_status(&namespace, &name, &status).await?;
                    return Ok(ReconcileAction::Requeue(Duration::from_secs(1)));
                }
            } else {
                // No driver — check primary readiness via K8s only (bootstrap path)
                if let Some(ref primary_name) = current_primary {
                    let primary_healthy = pods
                        .iter()
                        .find(|p| p.name_any() == *primary_name)
                        .map(is_pod_ready)
                        .unwrap_or(false);

                    if !primary_healthy {
                        warn!(name, primary = %primary_name, "primary unhealthy (no driver), initiating failover");
                        let status = KubericSetStatus {
                            phase: Phase::FailingOver,
                            ..set.status.clone().unwrap_or_default()
                        };
                        api.patch_set_status(&namespace, &name, &status).await?;
                        return Ok(ReconcileAction::Requeue(Duration::from_secs(1)));
                    }
                }
            }

            // --- Switchover check (only when all replicas are healthy) ---
            let target_primary = set.status.as_ref().and_then(|s| s.target_primary.clone());
            if let (Some(current), Some(target)) = (&current_primary, &target_primary)
                && current != target
            {
                info!(name, current = %current, target = %target, "switchover requested");
                let status = KubericSetStatus {
                    phase: Phase::Switchover,
                    ..set.status.clone().unwrap_or_default()
                };
                api.patch_set_status(&namespace, &name, &status).await?;
                return Ok(ReconcileAction::Requeue(Duration::from_secs(1)));
            }

            // Scale-up: create missing pods
            if actual < desired {
                info!(name, actual, desired, "scale-up: creating pods");
                for i in actual..desired {
                    ensure_pvc(api, set, &namespace, i as i32).await?;
                    ensure_pod(api, set, &namespace, i as i32).await?;
                }
                return Ok(ReconcileAction::Requeue(Duration::from_secs(5)));
            }

            // Scale-up: add newly ready pods to the partition via driver
            if let Some(driver) = state.drivers.lock().await.get_mut(&set_key) {
                let driver_count = driver.replica_ids().len();
                if driver_count < desired {
                    // Find pods that are ready but not in the driver
                    for pod in &ready_pods {
                        let pod_name = pod.name_any();
                        let replica_id: ReplicaId = pod
                            .metadata
                            .labels
                            .as_ref()
                            .and_then(|l| l.get("kuberic.io/pod-index"))
                            .and_then(|v| v.parse().ok())
                            .unwrap_or(0)
                            + 1; // labels are 0-indexed, IDs are 1-indexed

                        if driver.handle(replica_id).is_none() {
                            info!(name, pod = %pod_name, replica_id, "scale-up: adding replica to partition");
                            match api.create_replica_handle(replica_id, pod, &set.spec).await {
                                Ok(handle) => {
                                    if let Err(e) = driver.add_replica(handle).await {
                                        warn!(replica_id, error = %e, "failed to add replica");
                                        return Ok(ReconcileAction::Requeue(Duration::from_secs(
                                            5,
                                        )));
                                    }
                                    // Update label
                                    let mut labels = BTreeMap::new();
                                    labels.insert(
                                        "kuberic.io/role".to_string(),
                                        "secondary".to_string(),
                                    );
                                    let _ =
                                        api.patch_pod_labels(&namespace, &pod_name, labels).await;
                                }
                                Err(e) => {
                                    warn!(pod = %pod_name, error = %e, "failed to create handle");
                                    return Ok(ReconcileAction::Requeue(Duration::from_secs(5)));
                                }
                            }
                        }
                    }

                    // Update status
                    let status = KubericSetStatus {
                        ready_replicas: driver.replica_ids().len() as i32,
                        replicas: pods.len() as i32,
                        members: build_member_status(&pods, &set.spec),
                        ..set.status.clone().unwrap_or_default()
                    };
                    api.patch_set_status(&namespace, &name, &status).await?;
                }
            }

            // Scale-down: remove excess secondaries via driver, then delete pods
            if actual > desired
                && let Some(driver) = state.drivers.lock().await.get_mut(&set_key)
            {
                let primary_id = driver.primary_id();

                // Pick the highest-ID secondary to remove (prefer newest)
                let mut to_remove: Vec<ReplicaId> = driver
                    .replica_ids()
                    .into_iter()
                    .filter(|id| Some(*id) != primary_id)
                    .collect();
                to_remove.sort();
                to_remove.reverse(); // highest first

                let remove_count = actual - desired;
                for replica_id in to_remove.into_iter().take(remove_count) {
                    info!(name, replica_id, "scale-down: removing secondary");
                    if let Err(e) = driver
                        .remove_secondary(replica_id, set.spec.min_replicas as usize)
                        .await
                    {
                        warn!(replica_id, error = %e, "failed to remove secondary");
                        return Ok(ReconcileAction::Requeue(Duration::from_secs(5)));
                    }

                    // Delete the pod
                    let pod_name = format!("{}-{}", name, replica_id - 1);
                    let _ = api.delete_pod(&namespace, &pod_name).await;
                }

                // Update status
                let status = KubericSetStatus {
                    ready_replicas: driver.replica_ids().len() as i32,
                    replicas: driver.replica_ids().len() as i32,
                    members: build_member_status(&pods, &set.spec),
                    ..set.status.clone().unwrap_or_default()
                };
                api.patch_set_status(&namespace, &name, &status).await?;
            }

            Ok(ReconcileAction::Requeue(Duration::from_secs(30)))
        }

        Phase::FailingOver => {
            let set_key = format!("{}/{}", namespace, name);
            let mut drivers = state.drivers.lock().await;

            let current_primary_name = set
                .status
                .as_ref()
                .and_then(|s| s.current_primary.clone())
                .unwrap_or_default();

            if let Some(driver) = drivers.get_mut(&set_key) {
                if let Some(primary_id) = driver.primary_id() {
                    info!(name, primary_id, "running driver failover");
                    driver
                        .failover(primary_id)
                        .await
                        .map_err(|e| e.to_string())?;

                    let new_primary_id = driver.primary_id().unwrap();
                    let new_primary_name = format!("{}-{}", name, new_primary_id - 1);
                    let epoch = driver.epoch();

                    // Update labels
                    let mut labels = BTreeMap::new();
                    labels.insert("kuberic.io/role".to_string(), "primary".to_string());
                    let _ = api
                        .patch_pod_labels(&namespace, &new_primary_name, labels)
                        .await;

                    let mut labels = BTreeMap::new();
                    labels.insert("kuberic.io/role".to_string(), "secondary".to_string());
                    let _ = api
                        .patch_pod_labels(&namespace, &current_primary_name, labels)
                        .await;

                    let members = build_member_status(&pods, &set.spec);
                    let status = KubericSetStatus {
                        epoch: EpochStatus {
                            data_loss_number: epoch.data_loss_number,
                            configuration_number: epoch.configuration_number,
                        },
                        current_primary: Some(new_primary_name.clone()),
                        target_primary: Some(new_primary_name),
                        phase: Phase::Healthy,
                        reconfiguration_phase: ReconfigurationPhase::None,
                        ready_replicas: ready_pods.len() as i32,
                        replicas: pods.len() as i32,
                        members,
                        primary_failing_since: None,
                    };
                    api.patch_set_status(&namespace, &name, &status).await?;
                }
            } else {
                warn!(name, "no driver state for failover, requeueing");
            }

            Ok(ReconcileAction::Requeue(Duration::from_secs(10)))
        }

        Phase::Switchover => {
            let set_key = format!("{}/{}", namespace, name);
            let mut drivers = state.drivers.lock().await;

            let target_primary_name = set
                .status
                .as_ref()
                .and_then(|s| s.target_primary.clone())
                .unwrap_or_default();

            let current_primary_name = set
                .status
                .as_ref()
                .and_then(|s| s.current_primary.clone())
                .unwrap_or_default();

            if let Some(driver) = drivers.get_mut(&set_key) {
                // Resolve target pod name → replica ID
                let target_id: Option<ReplicaId> = pods
                    .iter()
                    .find(|p| p.name_any() == target_primary_name)
                    .and_then(|p| {
                        p.metadata
                            .labels
                            .as_ref()
                            .and_then(|l| l.get("kuberic.io/pod-index"))
                            .and_then(|v| v.parse::<i64>().ok())
                            .map(|idx| idx + 1)
                    });

                if let Some(target_id) = target_id {
                    info!(name, target_id, target = %target_primary_name, "running driver switchover");
                    driver
                        .switchover(target_id)
                        .await
                        .map_err(|e| e.to_string())?;

                    let epoch = driver.epoch();

                    // Update labels
                    let mut labels = BTreeMap::new();
                    labels.insert("kuberic.io/role".to_string(), "primary".to_string());
                    let _ = api
                        .patch_pod_labels(&namespace, &target_primary_name, labels)
                        .await;

                    let mut labels = BTreeMap::new();
                    labels.insert("kuberic.io/role".to_string(), "secondary".to_string());
                    let _ = api
                        .patch_pod_labels(&namespace, &current_primary_name, labels)
                        .await;

                    let members = build_member_status(&pods, &set.spec);
                    let status = KubericSetStatus {
                        epoch: EpochStatus {
                            data_loss_number: epoch.data_loss_number,
                            configuration_number: epoch.configuration_number,
                        },
                        current_primary: Some(target_primary_name.clone()),
                        target_primary: Some(target_primary_name),
                        phase: Phase::Healthy,
                        reconfiguration_phase: ReconfigurationPhase::None,
                        ready_replicas: ready_pods.len() as i32,
                        replicas: pods.len() as i32,
                        members,
                        primary_failing_since: None,
                    };
                    api.patch_set_status(&namespace, &name, &status).await?;
                } else {
                    warn!(name, target = %target_primary_name, "switchover target not found");
                }
            } else {
                warn!(name, "no driver state for switchover, requeueing");
            }

            Ok(ReconcileAction::Requeue(Duration::from_secs(10)))
        }

        Phase::Deleting => Ok(ReconcileAction::Requeue(Duration::from_secs(10))),
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn is_pod_ready(pod: &Pod) -> bool {
    pod.status
        .as_ref()
        .and_then(|s| s.conditions.as_ref())
        .map(|conditions| {
            conditions
                .iter()
                .any(|c| c.type_ == "Ready" && c.status == "True")
        })
        .unwrap_or(false)
}

fn build_member_status(pods: &[Pod], spec: &KubericSetSpec) -> Vec<MemberStatus> {
    pods.iter()
        .map(|pod| {
            let name = pod.name_any();
            let labels = pod.metadata.labels.as_ref();
            let role = labels
                .and_then(|l| l.get("kuberic.io/role"))
                .cloned()
                .unwrap_or_default();
            let id: i64 = labels
                .and_then(|l| l.get("kuberic.io/pod-index"))
                .and_then(|v| v.parse().ok())
                .unwrap_or(0);
            let pod_ip = pod
                .status
                .as_ref()
                .and_then(|s| s.pod_ip.as_ref())
                .cloned()
                .unwrap_or_default();

            MemberStatus {
                name,
                id,
                role,
                current_progress: 0,
                healthy: is_pod_ready(pod),
                control_address: format!("http://{}:{}", pod_ip, spec.control_port),
                data_address: format!("http://{}:{}", pod_ip, spec.data_port),
            }
        })
        .collect()
}

async fn create_pods(
    api: &dyn ClusterApi,
    set: &KubericSet,
    namespace: &str,
) -> Result<(), String> {
    for i in 0..set.spec.replicas {
        ensure_pvc(api, set, namespace, i).await?;
        ensure_pod(api, set, namespace, i).await?;
    }
    Ok(())
}

async fn ensure_pvc(
    api: &dyn ClusterApi,
    set: &KubericSet,
    namespace: &str,
    index: i32,
) -> Result<(), String> {
    let name = format!("{}-{}-data", set.name_any(), index);
    if api.get_pvc(namespace, &name).await.is_ok() {
        return Ok(());
    }
    let pvc = build_pvc(set, namespace, index);
    api.create_pvc(namespace, &pvc).await
}

async fn ensure_pod(
    api: &dyn ClusterApi,
    set: &KubericSet,
    namespace: &str,
    index: i32,
) -> Result<(), String> {
    let pod = build_pod(set, namespace, index);
    // create_pod is already idempotent (409 → Ok)
    api.create_pod(namespace, &pod).await
}

fn build_pod(set: &KubericSet, namespace: &str, index: i32) -> Pod {
    let name = format!("{}-{}", set.name_any(), index);
    let set_name = set.name_any();
    let role = if index == 0 { "primary" } else { "secondary" };
    let replica_id = (index + 1).to_string();

    let mut labels = BTreeMap::new();
    labels.insert("kuberic.io/set".into(), set_name.clone());
    labels.insert("kuberic.io/role".into(), role.into());
    labels.insert("kuberic.io/pod-index".into(), index.to_string());

    let owner_ref = serde_json::from_value(serde_json::json!({
        "apiVersion": "kuberic.io/v1",
        "kind": "KubericSet",
        "name": set_name,
        "uid": set.metadata.uid.as_deref().unwrap_or(""),
        "controller": true,
        "blockOwnerDeletion": true
    }))
    .expect("valid owner reference");

    let readiness_probe = Probe {
        tcp_socket: Some(TCPSocketAction {
            port: IntOrString::Int(set.spec.control_port),
            ..Default::default()
        }),
        initial_delay_seconds: Some(5),
        period_seconds: Some(5),
        timeout_seconds: Some(3),
        failure_threshold: Some(2),
        ..Default::default()
    };

    let liveness_probe = Probe {
        tcp_socket: Some(TCPSocketAction {
            port: IntOrString::Int(set.spec.control_port),
            ..Default::default()
        }),
        initial_delay_seconds: Some(10),
        period_seconds: Some(10),
        timeout_seconds: Some(5),
        failure_threshold: Some(3),
        ..Default::default()
    };

    let env = vec![
        k8s_openapi::api::core::v1::EnvVar {
            name: "KUBERIC_REPLICA_ID".into(),
            value: Some(replica_id),
            ..Default::default()
        },
        k8s_openapi::api::core::v1::EnvVar {
            name: "KUBERIC_CONTROL_BIND".into(),
            value: Some(format!("0.0.0.0:{}", set.spec.control_port)),
            ..Default::default()
        },
        k8s_openapi::api::core::v1::EnvVar {
            name: "KUBERIC_DATA_BIND".into(),
            value: Some(format!("0.0.0.0:{}", set.spec.data_port)),
            ..Default::default()
        },
        k8s_openapi::api::core::v1::EnvVar {
            name: "KUBERIC_CLIENT_BIND".into(),
            value: Some(format!("0.0.0.0:{}", set.spec.port)),
            ..Default::default()
        },
        k8s_openapi::api::core::v1::EnvVar {
            name: "RUST_LOG".into(),
            value: Some("info".into()),
            ..Default::default()
        },
    ];

    Pod {
        metadata: kube::api::ObjectMeta {
            name: Some(name),
            namespace: Some(namespace.into()),
            labels: Some(labels),
            owner_references: Some(vec![owner_ref]),
            ..Default::default()
        },
        spec: Some(PodSpec {
            containers: vec![Container {
                name: "app".into(),
                image: Some(set.spec.image.clone()),
                image_pull_policy: Some("IfNotPresent".into()),
                ports: Some(vec![
                    ContainerPort {
                        container_port: set.spec.port,
                        name: Some("app".into()),
                        ..Default::default()
                    },
                    ContainerPort {
                        container_port: set.spec.control_port,
                        name: Some("control".into()),
                        ..Default::default()
                    },
                    ContainerPort {
                        container_port: set.spec.data_port,
                        name: Some("data".into()),
                        ..Default::default()
                    },
                ]),
                env: Some(env),
                readiness_probe: Some(readiness_probe),
                liveness_probe: Some(liveness_probe),
                ..Default::default()
            }],
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn build_pvc(set: &KubericSet, namespace: &str, index: i32) -> PersistentVolumeClaim {
    let name = format!("{}-{}-data", set.name_any(), index);
    let set_name = set.name_any();

    let mut labels = BTreeMap::new();
    labels.insert("kuberic.io/set".into(), set_name);
    labels.insert("kuberic.io/pod-index".into(), index.to_string());

    let mut requests = BTreeMap::new();
    requests.insert("storage".into(), Quantity(set.spec.storage.clone()));

    PersistentVolumeClaim {
        metadata: kube::api::ObjectMeta {
            name: Some(name),
            namespace: Some(namespace.into()),
            labels: Some(labels),
            ..Default::default()
        },
        spec: Some(PersistentVolumeClaimSpec {
            access_modes: Some(vec!["ReadWriteOnce".into()]),
            resources: Some(VolumeResourceRequirements {
                requests: Some(requests),
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    }
}

async fn create_services(
    api: &dyn ClusterApi,
    set: &KubericSet,
    namespace: &str,
) -> Result<(), String> {
    let set_name = set.name_any();

    // -rw: routes to primary only
    let mut rw_selector = BTreeMap::new();
    rw_selector.insert("kuberic.io/set".into(), set_name.clone());
    rw_selector.insert("kuberic.io/role".into(), "primary".into());
    api.create_service(
        namespace,
        &build_service(
            &set_name,
            namespace,
            &format!("{}-rw", set_name),
            rw_selector,
            &set.spec,
        ),
    )
    .await?;

    // -ro: routes to secondaries only
    let mut ro_selector = BTreeMap::new();
    ro_selector.insert("kuberic.io/set".into(), set_name.clone());
    ro_selector.insert("kuberic.io/role".into(), "secondary".into());
    api.create_service(
        namespace,
        &build_service(
            &set_name,
            namespace,
            &format!("{}-ro", set_name),
            ro_selector,
            &set.spec,
        ),
    )
    .await?;

    // -r: routes to all pods
    let mut r_selector = BTreeMap::new();
    r_selector.insert("kuberic.io/set".into(), set_name.clone());
    api.create_service(
        namespace,
        &build_service(
            &set_name,
            namespace,
            &format!("{}-r", set_name),
            r_selector,
            &set.spec,
        ),
    )
    .await?;

    Ok(())
}

fn build_service(
    set_name: &str,
    namespace: &str,
    name: &str,
    selector: BTreeMap<String, String>,
    spec: &KubericSetSpec,
) -> Service {
    let mut labels = BTreeMap::new();
    labels.insert("kuberic.io/set".into(), set_name.into());

    Service {
        metadata: kube::api::ObjectMeta {
            name: Some(name.into()),
            namespace: Some(namespace.into()),
            labels: Some(labels),
            ..Default::default()
        },
        spec: Some(ServiceSpec {
            selector: Some(selector),
            ports: Some(vec![
                ServicePort {
                    port: spec.port,
                    name: Some("app".into()),
                    ..Default::default()
                },
                ServicePort {
                    port: spec.control_port,
                    name: Some("control".into()),
                    ..Default::default()
                },
                ServicePort {
                    port: spec.data_port,
                    name: Some("data".into()),
                    ..Default::default()
                },
            ]),
            ..Default::default()
        }),
        ..Default::default()
    }
}
