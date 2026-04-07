use std::collections::{BTreeMap, HashMap};
use std::time::Duration;

use k8s_openapi::api::core::v1::Pod;
use kube::ResourceExt;
use tokio::sync::Mutex;
use tracing::{info, warn};

use kubelicate_core::driver::{PartitionDriver, ReplicaHandle};
use kubelicate_core::types::ReplicaId;

use crate::cluster_api::ClusterApi;
use crate::crd::{
    EpochStatus, KubelicateSet, KubelicateSetSpec, KubelicateSetStatus, MemberStatus, Phase,
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
    set: &KubelicateSet,
    api: &dyn ClusterApi,
    state: &ReconcilerState,
) -> Result<ReconcileAction, String> {
    let name = set.name_any();
    let namespace = set.namespace().unwrap_or_default();

    info!(name, namespace, "reconciling KubelicateSet");

    let label_selector = format!("kubelicate.io/set={}", name);
    let pods = api.list_pods(&namespace, &label_selector).await?;

    let ready_pods: Vec<&Pod> = pods.iter().filter(|p| is_pod_ready(p)).collect();

    let current_phase = set
        .status
        .as_ref()
        .map(|s| s.phase.clone())
        .unwrap_or_default();

    match current_phase {
        Phase::Pending => {
            info!(name, "creating partition pods");
            create_pods(api, set, &namespace).await?;
            let status = KubelicateSetStatus {
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
                    labels.insert("kubelicate.io/role".to_string(), role.to_string());
                    let _ = api.patch_pod_labels(&namespace, &pod_name, labels).await;
                }
            }

            // Update CRD status
            let epoch = driver.epoch();
            let primary_name = driver.primary_id().map(|id| format!("{}-{}", name, id - 1));
            let members = build_member_status(&pods, &set.spec);

            let status = KubelicateSetStatus {
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

            // Check if primary is healthy
            if let Some(ref primary_name) = current_primary {
                let primary_healthy = pods
                    .iter()
                    .find(|p| p.name_any() == *primary_name)
                    .map(is_pod_ready)
                    .unwrap_or(false);

                if !primary_healthy {
                    warn!(name, primary = %primary_name, "primary unhealthy, initiating failover");
                    let status = KubelicateSetStatus {
                        phase: Phase::FailingOver,
                        ..set.status.clone().unwrap_or_default()
                    };
                    api.patch_set_status(&namespace, &name, &status).await?;
                    return Ok(ReconcileAction::Requeue(Duration::from_secs(1)));
                }
            }

            // Check scaling
            let desired = set.spec.replicas as usize;
            if pods.len() < desired {
                info!(name, actual = pods.len(), desired, "scaling up");
                for i in pods.len()..desired {
                    let pod = build_pod(set, &namespace, i as i32);
                    api.create_pod(&namespace, &pod).await?;
                }
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
                    labels.insert("kubelicate.io/role".to_string(), "primary".to_string());
                    let _ = api
                        .patch_pod_labels(&namespace, &new_primary_name, labels)
                        .await;

                    let mut labels = BTreeMap::new();
                    labels.insert("kubelicate.io/role".to_string(), "secondary".to_string());
                    let _ = api
                        .patch_pod_labels(&namespace, &current_primary_name, labels)
                        .await;

                    let members = build_member_status(&pods, &set.spec);
                    let status = KubelicateSetStatus {
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

        Phase::Switchover | Phase::Deleting => {
            Ok(ReconcileAction::Requeue(Duration::from_secs(10)))
        }
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

fn build_member_status(pods: &[Pod], spec: &KubelicateSetSpec) -> Vec<MemberStatus> {
    pods.iter()
        .map(|pod| {
            let name = pod.name_any();
            let labels = pod.metadata.labels.as_ref();
            let role = labels
                .and_then(|l| l.get("kubelicate.io/role"))
                .cloned()
                .unwrap_or_default();
            let id: i64 = labels
                .and_then(|l| l.get("kubelicate.io/replica-id"))
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
    set: &KubelicateSet,
    namespace: &str,
) -> Result<(), String> {
    for i in 0..set.spec.replicas {
        let pod = build_pod(set, namespace, i);
        api.create_pod(namespace, &pod).await?;
    }
    Ok(())
}

fn build_pod(set: &KubelicateSet, namespace: &str, index: i32) -> Pod {
    let name = format!("{}-{}", set.name_any(), index);
    let set_name = set.name_any();
    let role = if index == 0 { "primary" } else { "secondary" };

    serde_json::from_value(serde_json::json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": {
            "name": name,
            "namespace": namespace,
            "labels": {
                "kubelicate.io/set": set_name,
                "kubelicate.io/role": role,
                "kubelicate.io/replica-id": index.to_string()
            },
            "ownerReferences": [{
                "apiVersion": "kubelicate.io/v1",
                "kind": "KubelicateSet",
                "name": set_name,
                "uid": set.metadata.uid.as_deref().unwrap_or(""),
                "controller": true,
                "blockOwnerDeletion": true
            }]
        },
        "spec": {
            "containers": [{
                "name": "app",
                "image": set.spec.image,
                "ports": [
                    { "containerPort": set.spec.port, "name": "app" },
                    { "containerPort": set.spec.control_port, "name": "control" },
                    { "containerPort": set.spec.data_port, "name": "data" }
                ]
            }]
        }
    }))
    .expect("valid pod json")
}
