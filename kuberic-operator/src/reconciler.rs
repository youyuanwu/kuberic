use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use k8s_openapi::api::core::v1::{
    Container, ContainerPort, EnvVarSource, ObjectFieldSelector, PersistentVolumeClaim,
    PersistentVolumeClaimSpec, Pod, PodSpec, Probe, Service, ServicePort, ServiceSpec,
    TCPSocketAction, VolumeResourceRequirements,
};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;
use kube::ResourceExt;
use tokio::sync::Mutex;
use tracing::{info, warn};

use kuberic_core::driver::{PartitionDriver, ReplicaHandle};
use kuberic_core::types::{Epoch, ReplicaId, ReplicaInstanceId, Role, StablePartitionSnapshot};

use crate::cluster_api::ClusterApi;
use crate::crd::{
    DurableAddMode, DurableOperationKind, DurableOperationPhase, DurableRemoveMode, EpochStatus,
    KubericSet, KubericSetSpec, KubericSetStatus, MemberStatus, Phase, ReconfigurationPhase,
    StablePartitionSnapshotStatus, StableReplicaRoleStatus, StatusCondition,
};
use crate::durable::{
    Decision, OperationObservations, OperationPodIdentities, RemoveReplicaTarget,
    ReplicaObservation, decide, decide_add_replica, decide_remove_replica, fail_closed,
    operation_condition, record_activity_error, start_add_replica, start_remove_replica,
    start_switchover,
};

/// Shared state across reconciliation loops.
pub struct ReconcilerState {
    /// Per-set partition drivers, keyed by "{namespace}/{name}".
    pub drivers: Mutex<HashMap<String, PartitionDriver>>,
    /// Stable statuses whose first persistence attempt failed after the
    /// corresponding runtime topology had already committed.
    pending_statuses: Mutex<HashMap<String, KubericSetStatus>>,
}

impl Default for ReconcilerState {
    fn default() -> Self {
        Self {
            drivers: Mutex::new(HashMap::new()),
            pending_statuses: Mutex::new(HashMap::new()),
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
    let set_key = format!("{}/{}", namespace, name);

    info!(name, namespace, "reconciling KubericSet");

    // A topology operation is not complete until its stable snapshot is
    // durable. Retry a failed post-commit status write before observing pods
    // or selecting any further health/topology action.
    let pending_status = { state.pending_statuses.lock().await.get(&set_key).cloned() };
    if let Some(pending) = pending_status {
        if set.status.as_ref() == Some(&pending) {
            state.pending_statuses.lock().await.remove(&set_key);
            return Ok(ReconcileAction::Requeue(Duration::from_secs(1)));
        }
        api.patch_set_status(
            &namespace,
            &name,
            &pending,
            set.metadata.resource_version.as_deref(),
        )
        .await?;
        state.pending_statuses.lock().await.remove(&set_key);
        return Ok(ReconcileAction::Requeue(Duration::from_secs(1)));
    }

    let label_selector = format!("kuberic.io/set={}", name);
    let pods = api.list_pods(&namespace, &label_selector).await?;

    let ready_pods: Vec<&Pod> = pods.iter().filter(|p| is_pod_ready(p)).collect();

    let current_phase = set
        .status
        .as_ref()
        .map(|s| s.phase.clone())
        .unwrap_or_default();
    if let Some(operation) = set
        .status
        .as_ref()
        .and_then(|status| status.operation.as_ref())
        && !matches!(
            operation.phase,
            DurableOperationPhase::Completed
                | DurableOperationPhase::Failed
                | DurableOperationPhase::Poisoned
        )
    {
        let expected_phase = match operation.kind {
            DurableOperationKind::Switchover => Phase::Switchover,
            DurableOperationKind::AddReplica => Phase::AddingReplica,
            DurableOperationKind::RemoveReplica => Phase::RemovingReplica,
        };
        if current_phase != expected_phase {
            return Err(format!(
                "active durable operation {} requires {expected_phase:?} phase",
                operation.operation_id
            ));
        }
    }

    match current_phase {
        Phase::Pending => {
            info!(name, "creating partition pods and services");
            create_services(api, set, &namespace).await?;
            create_pods(api, set, &namespace).await?;
            let status = KubericSetStatus {
                phase: Phase::Creating,
                ..Default::default()
            };
            persist_committed_status(
                api,
                state,
                &set_key,
                &namespace,
                &name,
                &status,
                set.metadata.resource_version.as_deref(),
            )
            .await?;
            Ok(ReconcileAction::Requeue(Duration::from_secs(5)))
        }

        Phase::Creating => {
            if state.drivers.lock().await.contains_key(&set_key) {
                // The runtime completed creation and its status was already
                // persisted (or is about to become visible). Never replay
                // creation against open replicas.
                return Ok(ReconcileAction::Requeue(Duration::from_secs(1)));
            }
            let desired = set.spec.replicas as usize;
            if ready_pods.len() < desired {
                info!(name, ready = ready_pods.len(), desired, "waiting for pods");
                return Ok(ReconcileAction::Requeue(Duration::from_secs(5)));
            }

            info!(name, "all pods ready, initializing partition via driver");

            // Create ReplicaHandles
            let current_pods = checked_pods_by_id(&pods)?;
            let mut handles: Vec<Box<dyn ReplicaHandle>> = Vec::new();
            for (replica_id, _, pod) in current_pods {
                match api.create_replica_handle(replica_id, pod, &set.spec).await {
                    Ok(handle) => handles.push(handle),
                    Err(e) => {
                        warn!(pod = pod.name_any(), error = %e, "failed to create handle");
                        return Ok(ReconcileAction::Requeue(Duration::from_secs(5)));
                    }
                }
            }

            // Creating-phase recovery is intentionally unsupported. Probe
            // before any mutation so a process crash after runtime creation
            // but before status persistence cannot replay Open/New.
            for handle in &handles {
                let status = handle.get_status().await.map_err(|error| {
                    format!(
                        "cannot verify pristine runtime for replica {} before creation: {error}",
                        handle.id()
                    )
                })?;
                if status.instance_id != handle.instance_id()
                    || status.role != Role::Unknown
                    || status.epoch != Epoch::default()
                {
                    return Err(format!(
                        "refusing to replay creation for replica {}: runtime already has incarnation {}, role {:?}, epoch {:?}",
                        handle.id(),
                        status.instance_id,
                        status.role,
                        status.epoch
                    ));
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
                stable_snapshot: Some(snapshot_status(&driver)?),
                operation: None,
                conditions: Vec::new(),
                primary_failing_since: None,
            };
            if let Err(error) = persist_committed_status(
                api,
                state,
                &set_key,
                &namespace,
                &name,
                &status,
                set.metadata.resource_version.as_deref(),
            )
            .await
            {
                // Retain the completed driver so a retry cannot reopen the
                // already-created runtimes. A process crash still loses this
                // state and correctly fails closed against the older status.
                state.drivers.lock().await.insert(set_key, driver);
                return Err(error);
            }

            // Preserve the existing status-before-driver insertion ordering
            // on the successful path.
            state.drivers.lock().await.insert(set_key, driver);

            Ok(ReconcileAction::Requeue(Duration::from_secs(30)))
        }

        Phase::Healthy => {
            let desired = set.spec.replicas as usize;
            let actual = pods.len();
            let current_pods = checked_pods_by_id(&pods)?;
            let persisted_snapshot = set
                .status
                .as_ref()
                .and_then(|status| status.stable_snapshot.as_ref())
                .ok_or_else(|| {
                    format!("cannot recover {namespace}/{name}: stable snapshot is absent")
                })?;

            // Rebuild compensation deletes the failed candidate while keeping
            // the old stable identity. Recreate that ordinal so the durable
            // rebuild can be attempted again instead of interpreting the
            // intentional cleanup as permanent eviction.
            if actual < desired
                && set
                    .status
                    .as_ref()
                    .and_then(|status| status.operation.as_ref())
                    .is_some_and(|operation| {
                        operation.kind == DurableOperationKind::AddReplica
                            && operation.add_mode == Some(DurableAddMode::Rebuild)
                            && operation.phase == DurableOperationPhase::Failed
                    })
            {
                for i in 0..desired {
                    ensure_pvc(api, set, &namespace, i as i32).await?;
                    ensure_pod(api, set, &namespace, i as i32).await?;
                }
                return Ok(ReconcileAction::Requeue(Duration::from_secs(5)));
            }

            // A missing stable secondary has no replacement incarnation to
            // rebuild. Evict it durably before normal scale-up recreates
            // desired capacity.
            if let Some(target) = persisted_snapshot
                .members
                .iter()
                .filter(|member| member.role == StableReplicaRoleStatus::ActiveSecondary)
                .find(|member| !current_pods.iter().any(|(id, _, _)| *id == member.id))
            {
                let now = unix_seconds();
                let operation = start_remove_replica(
                    set.metadata.uid.as_deref().unwrap_or(&set_key),
                    persisted_snapshot.clone(),
                    RemoveReplicaTarget {
                        replica_id: target.id,
                        pod_name: format!("{}-{}", name, target.id - 1),
                        pod_uid: target.instance_id.clone(),
                    },
                    DurableRemoveMode::Force,
                    set.spec.min_replicas as usize,
                    now,
                )?;
                let mut status = KubericSetStatus {
                    phase: Phase::RemovingReplica,
                    operation: Some(operation.clone()),
                    ..set.status.clone().unwrap_or_default()
                };
                set_operation_condition(&mut status, operation_condition(&operation, now));
                api.patch_set_status(
                    &namespace,
                    &name,
                    &status,
                    set.metadata.resource_version.as_deref(),
                )
                .await?;
                state.drivers.lock().await.remove(&set_key);
                return Ok(ReconcileAction::Requeue(Duration::from_secs(1)));
            }

            // A replacement incarnation cannot reconstruct the previous
            // stable driver. Rebuild it when ready; otherwise evict the old
            // committed incarnation without deleting the replacement UID.
            if let Some((target_id, target_instance, target_pod, old_instance)) = persisted_snapshot
                .members
                .iter()
                .filter(|member| member.role == StableReplicaRoleStatus::ActiveSecondary)
                .find_map(|member| {
                    current_pods
                        .iter()
                        .find(|(id, instance, _)| {
                            *id == member.id && instance.as_str() != member.instance_id
                        })
                        .map(|(id, instance, pod)| {
                            (*id, instance.clone(), *pod, member.instance_id.clone())
                        })
                })
            {
                if !is_pod_ready(target_pod) {
                    let now = unix_seconds();
                    let operation = start_remove_replica(
                        set.metadata.uid.as_deref().unwrap_or(&set_key),
                        persisted_snapshot.clone(),
                        RemoveReplicaTarget {
                            replica_id: target_id,
                            pod_name: target_pod.name_any(),
                            pod_uid: old_instance,
                        },
                        DurableRemoveMode::Force,
                        set.spec.min_replicas as usize,
                        now,
                    )?;
                    let mut status = KubericSetStatus {
                        phase: Phase::RemovingReplica,
                        operation: Some(operation.clone()),
                        ..set.status.clone().unwrap_or_default()
                    };
                    set_operation_condition(&mut status, operation_condition(&operation, now));
                    api.patch_set_status(
                        &namespace,
                        &name,
                        &status,
                        set.metadata.resource_version.as_deref(),
                    )
                    .await?;
                    state.drivers.lock().await.remove(&set_key);
                    return Ok(ReconcileAction::Requeue(Duration::from_secs(1)));
                }
                let now = unix_seconds();
                let operation = start_add_replica(
                    set.metadata.uid.as_deref().unwrap_or(&set_key),
                    persisted_snapshot.clone(),
                    target_id,
                    target_instance.to_string(),
                    target_pod.name_any(),
                    DurableAddMode::Rebuild,
                    now,
                )?;
                let mut status = KubericSetStatus {
                    phase: Phase::AddingReplica,
                    operation: Some(operation.clone()),
                    ..set.status.clone().unwrap_or_default()
                };
                set_operation_condition(&mut status, operation_condition(&operation, now));
                api.patch_set_status(
                    &namespace,
                    &name,
                    &status,
                    set.metadata.resource_version.as_deref(),
                )
                .await?;
                state.drivers.lock().await.remove(&set_key);
                return Ok(ReconcileAction::Requeue(Duration::from_secs(1)));
            }

            // Ensure every desired ordinal exists after missing committed
            // members have been evicted and replacement incarnations handled.
            if actual < desired {
                info!(name, actual, desired, "scale-up: creating pods");
                for i in 0..desired {
                    ensure_pvc(api, set, &namespace, i as i32).await?;
                    ensure_pod(api, set, &namespace, i as i32).await?;
                }
                return Ok(ReconcileAction::Requeue(Duration::from_secs(5)));
            }

            // Rebuild process-local driver state before making any Healthy
            // health or topology decision. Legacy observational fields are
            // never used as recovery input.
            let needs_recovery = !state.drivers.lock().await.contains_key(&set_key);
            if needs_recovery {
                let snapshot = StablePartitionSnapshot::try_from(persisted_snapshot)
                    .map_err(|error| format!("invalid stable snapshot: {error}"))?;
                let mut handles: Vec<Box<dyn ReplicaHandle>> = Vec::new();
                for (replica_id, _, pod) in &current_pods {
                    if !persisted_snapshot
                        .members
                        .iter()
                        .any(|member| member.id == *replica_id)
                    {
                        continue;
                    }
                    handles.push(
                        api.create_replica_handle(*replica_id, pod, &set.spec)
                            .await
                            .map_err(|error| {
                                format!(
                                    "cannot construct recovery handle for replica {replica_id}: {error}"
                                )
                            })?,
                    );
                }
                let driver = PartitionDriver::recover(snapshot, handles)
                    .await
                    .map_err(|error| format!("stable driver recovery failed: {error}"))?;
                state
                    .drivers
                    .lock()
                    .await
                    .entry(set_key.clone())
                    .or_insert(driver);
            }
            {
                let drivers = state.drivers.lock().await;
                let driver = drivers.get(&set_key).unwrap();
                validate_pod_handle_identities(driver, &current_pods, desired)?;
            }

            let (current_primary_id, current_epoch, replica_ids) = {
                let drivers = state.drivers.lock().await;
                let driver = drivers.get(&set_key).unwrap();
                (driver.primary_id(), driver.epoch(), driver.replica_ids())
            };
            let current_primary =
                current_primary_id.and_then(|id| pod_name_for_id(&current_pods, id));

            // --- Replica health check (primary + secondaries) ---
            // Probe all replicas via get_status to detect crashed/restarted pods.
            // Must run BEFORE switchover — don't switchover to a stale target.
            let mut stale_ids: Vec<ReplicaId> = Vec::new();
            let mut primary_stale = false;
            for &replica_id in &replica_ids {
                let Some((_, expected_instance, pod)) =
                    current_pods.iter().find(|(id, _, _)| *id == replica_id)
                else {
                    if Some(replica_id) == current_primary_id {
                        primary_stale = true;
                    } else {
                        stale_ids.push(replica_id);
                    }
                    continue;
                };
                let is_stale = match api.create_replica_handle(replica_id, pod, &set.spec).await {
                    Ok(handle) => match handle.get_status().await {
                        Ok(s) if s.instance_id != *expected_instance => {
                            warn!(
                                name,
                                replica_id,
                                expected_instance_id = %expected_instance,
                                actual_instance_id = %s.instance_id,
                                "replica incarnation mismatch"
                            );
                            true
                        }
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
                    },
                    Err(error) => {
                        warn!(
                            name,
                            replica_id,
                            error = %error,
                            "failed to construct health-check handle"
                        );
                        true
                    }
                };

                if is_stale {
                    if Some(replica_id) == current_primary_id {
                        primary_stale = true;
                    } else {
                        stale_ids.push(replica_id);
                    }
                }
            }

            // Also check K8s-level readiness for primary
            if !primary_stale && let Some(ref primary_name) = current_primary {
                let primary_ready = pods
                    .iter()
                    .find(|p| p.name_any() == *primary_name)
                    .map(is_pod_ready)
                    .unwrap_or(false);
                if !primary_ready {
                    primary_stale = true;
                }
            }

            // Primary stale → failover (takes priority over everything)
            if primary_stale {
                warn!(name, "primary unhealthy, initiating failover");
                {
                    let mut drivers = state.drivers.lock().await;
                    let driver = drivers.get_mut(&set_key).unwrap();
                    for &id in &stale_ids {
                        info!(
                            name,
                            replica_id = id,
                            "removing stale secondary before failover"
                        );
                        driver.remove_replica_from_driver(id);
                    }
                }
                let status = KubericSetStatus {
                    phase: Phase::FailingOver,
                    ..set.status.clone().unwrap_or_default()
                };
                persist_committed_status(
                    api,
                    state,
                    &set_key,
                    &namespace,
                    &name,
                    &status,
                    set.metadata.resource_version.as_deref(),
                )
                .await?;
                return Ok(ReconcileAction::Requeue(Duration::from_secs(1)));
            }

            if !stale_ids.is_empty() {
                let target_id = *stale_ids.iter().max().unwrap();
                let target_member = persisted_snapshot
                    .members
                    .iter()
                    .find(|member| member.id == target_id)
                    .ok_or_else(|| {
                        format!("stale secondary {target_id} is absent from stable snapshot")
                    })?;
                let target_pod_name = current_pods
                    .iter()
                    .find(|(id, _, _)| *id == target_id)
                    .map(|(_, _, pod)| pod.name_any())
                    .unwrap_or_else(|| format!("{}-{}", name, target_id - 1));
                let now = unix_seconds();
                let operation = start_remove_replica(
                    set.metadata.uid.as_deref().unwrap_or(&set_key),
                    persisted_snapshot.clone(),
                    RemoveReplicaTarget {
                        replica_id: target_id,
                        pod_name: target_pod_name,
                        pod_uid: target_member.instance_id.clone(),
                    },
                    DurableRemoveMode::Force,
                    set.spec.min_replicas as usize,
                    now,
                )?;
                let mut status = KubericSetStatus {
                    phase: Phase::RemovingReplica,
                    operation: Some(operation.clone()),
                    ..set.status.clone().unwrap_or_default()
                };
                set_operation_condition(&mut status, operation_condition(&operation, now));
                api.patch_set_status(
                    &namespace,
                    &name,
                    &status,
                    set.metadata.resource_version.as_deref(),
                )
                .await?;
                state.drivers.lock().await.remove(&set_key);
                return Ok(ReconcileAction::Requeue(Duration::from_secs(1)));
            }

            // --- Switchover check (only when all replicas are healthy) ---
            let target_primary = set.status.as_ref().and_then(|s| s.target_primary.clone());
            if let (Some(current), Some(target)) = (&current_primary, &target_primary)
                && current != target
            {
                let target_id = current_pods
                    .iter()
                    .find(|(_, _, pod)| pod.name_any() == *target)
                    .map(|(id, _, _)| *id)
                    .ok_or_else(|| format!("switchover target pod {target} is not current"))?;
                let drivers = state.drivers.lock().await;
                if drivers
                    .get(&set_key)
                    .and_then(|driver| driver.handle(target_id))
                    .is_none()
                {
                    return Err(format!(
                        "switchover target replica {target_id} is not in the committed driver topology"
                    ));
                }
                let previous_snapshot = snapshot_status(drivers.get(&set_key).unwrap())?;
                drop(drivers);
                info!(name, current = %current, target = %target, "switchover requested");
                let now = unix_seconds();
                let operation = start_switchover(
                    set.metadata.uid.as_deref().unwrap_or(&set_key),
                    previous_snapshot,
                    target_id,
                    now,
                )?;
                let mut status = KubericSetStatus {
                    phase: Phase::Switchover,
                    operation: Some(operation.clone()),
                    ..set.status.clone().unwrap_or_default()
                };
                set_operation_condition(&mut status, operation_condition(&operation, now));
                api.patch_set_status(
                    &namespace,
                    &name,
                    &status,
                    set.metadata.resource_version.as_deref(),
                )
                .await?;
                state.drivers.lock().await.remove(&set_key);
                return Ok(ReconcileAction::Requeue(Duration::from_secs(1)));
            }

            // Scale-up: checkpoint exactly one ready pod before any runtime
            // mutation. Later reconciles operate only from this checkpoint.
            if persisted_snapshot.members.len() < desired
                && let Some((replica_id, instance_id, pod)) =
                    current_pods.iter().find(|(id, _, pod)| {
                        is_pod_ready(pod)
                            && !persisted_snapshot
                                .members
                                .iter()
                                .any(|member| member.id == *id)
                    })
            {
                let now = unix_seconds();
                let operation = start_add_replica(
                    set.metadata.uid.as_deref().unwrap_or(&set_key),
                    persisted_snapshot.clone(),
                    *replica_id,
                    instance_id.to_string(),
                    pod.name_any(),
                    DurableAddMode::ScaleUp,
                    now,
                )?;
                let mut status = KubericSetStatus {
                    phase: Phase::AddingReplica,
                    operation: Some(operation.clone()),
                    ..set.status.clone().unwrap_or_default()
                };
                set_operation_condition(&mut status, operation_condition(&operation, now));
                api.patch_set_status(
                    &namespace,
                    &name,
                    &status,
                    set.metadata.resource_version.as_deref(),
                )
                .await?;
                state.drivers.lock().await.remove(&set_key);
                return Ok(ReconcileAction::Requeue(Duration::from_secs(1)));
            }

            // Scale-down: checkpoint one exact stable secondary before any
            // runtime or Kubernetes mutation.
            if persisted_snapshot.members.len() > desired {
                let target = persisted_snapshot
                    .members
                    .iter()
                    .filter(|member| member.id != persisted_snapshot.primary_id)
                    .max_by_key(|member| member.id)
                    .ok_or_else(|| "scale-down has no removable secondary".to_string())?;
                let pod_name = current_pods
                    .iter()
                    .find(|(id, _, _)| *id == target.id)
                    .map(|(_, _, pod)| pod.name_any())
                    .unwrap_or_else(|| format!("{}-{}", name, target.id - 1));
                let now = unix_seconds();
                let operation = start_remove_replica(
                    set.metadata.uid.as_deref().unwrap_or(&set_key),
                    persisted_snapshot.clone(),
                    RemoveReplicaTarget {
                        replica_id: target.id,
                        pod_name,
                        pod_uid: target.instance_id.clone(),
                    },
                    DurableRemoveMode::ScaleDown,
                    set.spec.min_replicas as usize,
                    now,
                )?;
                let mut status = KubericSetStatus {
                    phase: Phase::RemovingReplica,
                    operation: Some(operation.clone()),
                    ..set.status.clone().unwrap_or_default()
                };
                set_operation_condition(&mut status, operation_condition(&operation, now));
                api.patch_set_status(
                    &namespace,
                    &name,
                    &status,
                    set.metadata.resource_version.as_deref(),
                )
                .await?;
                state.drivers.lock().await.remove(&set_key);
                return Ok(ReconcileAction::Requeue(Duration::from_secs(1)));
            }

            Ok(ReconcileAction::Requeue(Duration::from_secs(30)))
        }

        Phase::FailingOver => {
            let mut drivers = state.drivers.lock().await;
            let current_pods = checked_pods_by_id(&pods)?;

            if let Some(driver) = drivers.get_mut(&set_key) {
                validate_pod_handle_identities(driver, &current_pods, set.spec.replicas as usize)?;
                if let Some(primary_id) = driver.primary_id() {
                    let current_primary_name = pod_name_for_id(&current_pods, primary_id)
                        .ok_or_else(|| {
                            format!("current primary replica {primary_id} has no current pod")
                        })?;
                    info!(name, primary_id, "running driver failover");
                    driver
                        .failover(primary_id)
                        .await
                        .map_err(|e| e.to_string())?;

                    let new_primary_id = driver.primary_id().unwrap();
                    let new_primary_name = pod_name_for_id(&current_pods, new_primary_id)
                        .ok_or_else(|| {
                            format!("new primary replica {new_primary_id} has no current pod")
                        })?;
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
                        stable_snapshot: Some(snapshot_status(driver)?),
                        operation: None,
                        conditions: Vec::new(),
                        primary_failing_since: None,
                    };
                    persist_committed_status(
                        api,
                        state,
                        &set_key,
                        &namespace,
                        &name,
                        &status,
                        set.metadata.resource_version.as_deref(),
                    )
                    .await?;
                }
            } else {
                warn!(name, "no driver state for failover, requeueing");
            }

            Ok(ReconcileAction::Requeue(Duration::from_secs(10)))
        }

        Phase::Switchover | Phase::AddingReplica | Phase::RemovingReplica => {
            reconcile_durable_operation(set, api, state, &pods, &ready_pods, &set_key).await
        }

        Phase::Deleting => Ok(ReconcileAction::Requeue(Duration::from_secs(10))),
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

type CurrentPod<'a> = (ReplicaId, ReplicaInstanceId, &'a Pod);

async fn reconcile_durable_operation(
    set: &KubericSet,
    api: &dyn ClusterApi,
    state: &ReconcilerState,
    pods: &[Pod],
    ready_pods: &[&Pod],
    set_key: &str,
) -> Result<ReconcileAction, String> {
    let name = set.name_any();
    let namespace = set.namespace().unwrap_or_default();
    let operation = set
        .status
        .as_ref()
        .and_then(|status| status.operation.clone())
        .ok_or_else(|| "durable operation phase has no checkpoint".to_string())?;
    let current_pods = checked_pods_by_id(pods)?;

    let identity_members = match operation.kind {
        DurableOperationKind::Switchover => operation.previous_snapshot.members.clone(),
        DurableOperationKind::AddReplica => operation
            .target_snapshot
            .members
            .iter()
            .filter(|member| Some(member.id) != operation.target_replica_id)
            .cloned()
            .collect(),
        DurableOperationKind::RemoveReplica => operation.target_snapshot.members.clone(),
    };
    let identity_error = identity_members.iter().find_map(|member| {
        match current_pods.iter().find(|(id, _, _)| *id == member.id) {
            None => Some(format!(
                "durable operation replica {} has no current pod",
                member.id
            )),
            Some((_, instance_id, _)) if instance_id.as_str() != member.instance_id => {
                Some(format!(
                    "durable operation replica {} incarnation changed",
                    member.id
                ))
            }
            Some(_) => None,
        }
    });
    if let Some(error) = identity_error {
        let now = unix_seconds();
        let failed = fail_closed(&operation, &error);
        let mut status = set.status.clone().unwrap_or_default();
        status.operation = Some(failed.clone());
        set_operation_condition(&mut status, operation_condition(&failed, now));
        api.patch_set_status(
            &namespace,
            &name,
            &status,
            set.metadata.resource_version.as_deref(),
        )
        .await?;
        return Ok(ReconcileAction::Requeue(Duration::from_secs(10)));
    }

    let mut handles: BTreeMap<ReplicaId, Box<dyn ReplicaHandle>> = BTreeMap::new();
    let mut observations = OperationObservations::new();
    let pod_identities: OperationPodIdentities = current_pods
        .iter()
        .map(|(id, instance_id, _)| (*id, instance_id.to_string()))
        .collect();
    for (replica_id, instance_id, pod) in &current_pods {
        let is_target_member = operation
            .target_snapshot
            .members
            .iter()
            .any(|member| member.id == *replica_id);
        let is_exact_remove_target = operation.kind == DurableOperationKind::RemoveReplica
            && operation.target_replica_id == Some(*replica_id)
            && operation.target_instance_id.as_deref() == Some(instance_id.as_str());
        if !is_target_member && !is_exact_remove_target {
            continue;
        }
        let Ok(handle) = api.create_replica_handle(*replica_id, pod, &set.spec).await else {
            continue;
        };
        if let Ok(status) = handle.get_status().await {
            observations.insert(
                *replica_id,
                ReplicaObservation {
                    status,
                    replicator_address: handle.replicator_address(),
                    pod_name: pod.name_any(),
                    pod_role_label: pod
                        .metadata
                        .labels
                        .as_ref()
                        .and_then(|labels| labels.get("kuberic.io/role"))
                        .cloned(),
                },
            );
        }
        handles.insert(*replica_id, handle);
    }

    let now = unix_seconds();
    let decision = match operation.kind {
        DurableOperationKind::Switchover => decide(&operation, &observations, now),
        DurableOperationKind::AddReplica => decide_add_replica(&operation, &observations, now),
        DurableOperationKind::RemoveReplica => {
            decide_remove_replica(&operation, &observations, &pod_identities, now)
        }
    };
    let decision = match decision {
        Ok(decision) => decision,
        Err(error) => {
            let mut status = set.status.clone().unwrap_or_default();
            set_operation_condition(
                &mut status,
                StatusCondition {
                    type_: "DurableOperation".to_string(),
                    status: "True".to_string(),
                    reason: "IncompatibleOrInvalid".to_string(),
                    message: error,
                    last_transition_time: now.to_string(),
                },
            );
            api.patch_set_status(
                &namespace,
                &name,
                &status,
                set.metadata.resource_version.as_deref(),
            )
            .await?;
            return Ok(ReconcileAction::Requeue(Duration::from_secs(10)));
        }
    };

    match decision {
        Decision::Persist(next_operation) => {
            let mut status = set.status.clone().unwrap_or_default();
            status.operation = Some(next_operation.clone());
            set_operation_condition(&mut status, operation_condition(&next_operation, now));
            api.patch_set_status(
                &namespace,
                &name,
                &status,
                set.metadata.resource_version.as_deref(),
            )
            .await?;
        }
        Decision::Execute {
            target_id,
            action_id,
            action,
        } => {
            let result = match handles.get(&target_id) {
                Some(handle) => handle.execute_durable_action(&action_id, action).await,
                None => {
                    return Ok(ReconcileAction::Requeue(Duration::from_secs(1)));
                }
            };
            if let Err(error) = result {
                let next_operation = record_activity_error(&operation, &error.to_string());
                let mut status = set.status.clone().unwrap_or_default();
                status.operation = Some(next_operation.clone());
                set_operation_condition(&mut status, operation_condition(&next_operation, now));
                api.patch_set_status(
                    &namespace,
                    &name,
                    &status,
                    set.metadata.resource_version.as_deref(),
                )
                .await?;
            }
        }
        Decision::PatchPodRole { target_id, role } => {
            let Some(observation) = observations.get(&target_id) else {
                return Ok(ReconcileAction::Requeue(Duration::from_secs(1)));
            };
            let mut labels = BTreeMap::new();
            labels.insert("kuberic.io/role".to_string(), role);
            if let Err(error) = api
                .patch_pod_labels(&namespace, &observation.pod_name, labels)
                .await
            {
                let next_operation = record_activity_error(&operation, &error);
                let mut status = set.status.clone().unwrap_or_default();
                status.operation = Some(next_operation.clone());
                set_operation_condition(&mut status, operation_condition(&next_operation, now));
                api.patch_set_status(
                    &namespace,
                    &name,
                    &status,
                    set.metadata.resource_version.as_deref(),
                )
                .await?;
            }
        }
        Decision::DeletePod {
            pod_name,
            expected_uid,
        } => {
            if let Err(error) = api.delete_pod(&namespace, &pod_name, &expected_uid).await {
                let next_operation = record_activity_error(&operation, &error);
                let mut status = set.status.clone().unwrap_or_default();
                status.operation = Some(next_operation.clone());
                set_operation_condition(&mut status, operation_condition(&next_operation, now));
                api.patch_set_status(
                    &namespace,
                    &name,
                    &status,
                    set.metadata.resource_version.as_deref(),
                )
                .await?;
            }
        }
        Decision::CommitSnapshot {
            operation,
            snapshot,
        } => {
            let mut status = set.status.clone().unwrap_or_default();
            status.epoch = snapshot.epoch.clone();
            status.ready_replicas = ready_pods.len() as i32;
            status.replicas = pods.len() as i32;
            status.members = build_member_status(pods, &set.spec);
            status.stable_snapshot = Some(snapshot);
            status.operation = Some(operation.clone());
            set_operation_condition(&mut status, operation_condition(&operation, now));
            persist_committed_status(
                api,
                state,
                set_key,
                &namespace,
                &name,
                &status,
                set.metadata.resource_version.as_deref(),
            )
            .await?;
        }
        Decision::Wait => {}
        Decision::Complete {
            operation,
            snapshot,
            compensated: _,
        } => {
            let primary_name = pod_name_for_id(&current_pods, snapshot.primary_id)
                .ok_or_else(|| "completed snapshot primary has no current pod".to_string())?;
            let mut status = set.status.clone().unwrap_or_default();
            status.epoch = snapshot.epoch.clone();
            status.current_primary = Some(primary_name.clone());
            status.target_primary = Some(primary_name);
            status.phase = Phase::Healthy;
            status.reconfiguration_phase = ReconfigurationPhase::None;
            status.ready_replicas = ready_pods.len() as i32;
            status.replicas = pods.len() as i32;
            status.members = build_member_status_for_snapshot(pods, &set.spec, &snapshot);
            status.stable_snapshot = Some(snapshot);
            status.operation = Some(operation.clone());
            status.primary_failing_since = None;
            set_operation_condition(&mut status, operation_condition(&operation, now));
            persist_committed_status(
                api,
                state,
                set_key,
                &namespace,
                &name,
                &status,
                set.metadata.resource_version.as_deref(),
            )
            .await?;
            state.drivers.lock().await.remove(set_key);
        }
    }

    Ok(ReconcileAction::Requeue(Duration::from_secs(1)))
}

async fn persist_committed_status(
    api: &dyn ClusterApi,
    state: &ReconcilerState,
    set_key: &str,
    namespace: &str,
    name: &str,
    status: &KubericSetStatus,
    expected_resource_version: Option<&str>,
) -> Result<(), String> {
    match api
        .patch_set_status(namespace, name, status, expected_resource_version)
        .await
    {
        Ok(()) => {
            state.pending_statuses.lock().await.remove(set_key);
            Ok(())
        }
        Err(error) => {
            state
                .pending_statuses
                .lock()
                .await
                .insert(set_key.to_string(), status.clone());
            Err(error)
        }
    }
}

/// Build the current logical/incarnation identity view from required pod
/// metadata. Kubernetes list position is never an identity source.
fn checked_pods_by_id(pods: &[Pod]) -> Result<Vec<CurrentPod<'_>>, String> {
    let mut result = Vec::with_capacity(pods.len());
    let mut ids = HashSet::new();
    let mut instances = HashSet::new();
    for pod in pods {
        let pod_name = pod.name_any();
        let index = pod
            .metadata
            .labels
            .as_ref()
            .and_then(|labels| labels.get("kuberic.io/pod-index"))
            .ok_or_else(|| format!("pod {pod_name} has no kuberic.io/pod-index label"))?
            .parse::<ReplicaId>()
            .map_err(|error| {
                format!("pod {pod_name} has invalid kuberic.io/pod-index label: {error}")
            })?;
        if index < 0 {
            return Err(format!(
                "pod {pod_name} has negative kuberic.io/pod-index label"
            ));
        }
        let replica_id = index
            .checked_add(1)
            .ok_or_else(|| format!("pod {pod_name} has overflowing kuberic.io/pod-index label"))?;
        if !ids.insert(replica_id) {
            return Err(format!("duplicate pod logical replica ID {replica_id}"));
        }
        let instance_id = pod
            .metadata
            .uid
            .as_ref()
            .filter(|uid| !uid.is_empty())
            .cloned()
            .map(ReplicaInstanceId::new)
            .ok_or_else(|| format!("pod {pod_name} has no UID"))?;
        if !instances.insert(instance_id.clone()) {
            return Err(format!("duplicate pod incarnation {instance_id}"));
        }
        result.push((replica_id, instance_id, pod));
    }
    result.sort_by_key(|(id, _, _)| *id);
    Ok(result)
}

/// Before every Healthy topology decision, ensure current pod identities still
/// attest the process-local handles. Extra identities are allowed only while
/// the desired replica count is larger than the committed driver membership.
fn validate_pod_handle_identities(
    driver: &PartitionDriver,
    current_pods: &[CurrentPod<'_>],
    desired: usize,
) -> Result<(), String> {
    let driver_ids = driver.replica_ids();
    for replica_id in &driver_ids {
        let (_, current_instance, _) = current_pods
            .iter()
            .find(|(id, _, _)| id == replica_id)
            .ok_or_else(|| format!("current pod for driver replica {replica_id} is missing"))?;
        let handle = driver
            .handle(*replica_id)
            .ok_or_else(|| format!("driver handle {replica_id} is missing"))?;
        let handle_instance = handle.instance_id();
        if *current_instance != handle_instance {
            return Err(format!(
                "current pod incarnation drift for replica {replica_id}: driver {handle_instance}, pod {current_instance}"
            ));
        }
    }

    let extras: Vec<_> = current_pods
        .iter()
        .filter(|(id, _, _)| driver.handle(*id).is_none())
        .map(|(id, _, _)| *id)
        .collect();
    if !extras.is_empty() && driver_ids.len() >= desired {
        return Err(format!(
            "current pod logical identities are not a bijection with driver handles; extra replicas: {extras:?}"
        ));
    }
    Ok(())
}

fn pod_name_for_id(current_pods: &[CurrentPod<'_>], id: ReplicaId) -> Option<String> {
    current_pods
        .iter()
        .find(|(current_id, _, _)| *current_id == id)
        .map(|(_, _, pod)| pod.name_any())
}

fn snapshot_status(driver: &PartitionDriver) -> Result<StablePartitionSnapshotStatus, String> {
    let snapshot = driver
        .stable_snapshot()
        .map_err(|error| format!("cannot persist stable snapshot: {error}"))?;
    StablePartitionSnapshotStatus::try_from(&snapshot)
}

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
                .unwrap_or(0)
                + 1;
            let pod_ip = pod
                .status
                .as_ref()
                .and_then(|s| s.pod_ip.as_ref())
                .cloned()
                .unwrap_or_default();

            MemberStatus {
                name,
                id,
                instance_id: pod.metadata.uid.clone().unwrap_or_default(),
                role,
                current_progress: 0,
                healthy: is_pod_ready(pod),
                control_address: format!("http://{}:{}", pod_ip, spec.control_port),
                data_address: format!("http://{}:{}", pod_ip, spec.data_port),
            }
        })
        .collect()
}

fn build_member_status_for_snapshot(
    pods: &[Pod],
    spec: &KubericSetSpec,
    snapshot: &StablePartitionSnapshotStatus,
) -> Vec<MemberStatus> {
    let mut members = build_member_status(pods, spec);
    for member in &mut members {
        if let Some(stable) = snapshot
            .members
            .iter()
            .find(|stable| stable.id == member.id)
        {
            member.role = match stable.role {
                crate::crd::StableReplicaRoleStatus::Primary => "primary",
                crate::crd::StableReplicaRoleStatus::ActiveSecondary => "secondary",
            }
            .to_string();
        }
    }
    members
}

fn set_operation_condition(status: &mut KubericSetStatus, condition: StatusCondition) {
    status
        .conditions
        .retain(|existing| existing.type_ != condition.type_);
    status.conditions.push(condition);
}

fn unix_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
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
            name: "KUBERIC_REPLICA_INSTANCE_ID".into(),
            value_from: Some(EnvVarSource {
                field_ref: Some(ObjectFieldSelector {
                    api_version: Some("v1".into()),
                    field_path: "metadata.uid".into(),
                }),
                ..Default::default()
            }),
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
