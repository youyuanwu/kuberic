use std::collections::BTreeMap;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use crate::evaluator::EvaluationConfig;
use kube::ResourceExt;
use kuberic_runtime::protocol::types::ResourceUid;
use tokio::sync::Mutex;

use crate::cluster_api::{ProductionClusterApi, PublicV1Scope};
use crate::evaluator::public_v1_history::{
    PublicV1HistoryPlan, plan_bootstrap_history, plan_recovery_report,
};
use crate::evaluator::public_v1_session_renewal::{accept_session_renewal, plan_session_renewal};
use crate::evaluator::public_v1_topology::{
    PublicV1TopologyPlan, PublicV1TopologyReplica, plan_public_v1_topology,
};
use crate::executor::ExecutionKind;
use crate::observation::RawAgentObservation;
use crate::{ControllerError, Result};
use kuberic_runtime::protocol::command::{InitializeAgentStore, ProtocolCommand};
use kuberic_runtime::protocol::public_operations::{
    ProcessSessionRenewalRequired, PublicProgressRole, PublicSessionRenewalStage,
    PublicV1AgentReport,
};
use kuberic_runtime::protocol::types::{
    AccessStatus, ConfigurationDescriptor, ConfigurationMember, EffectivePolicy, Epoch,
    OperationId, PodUid, ProcessSessionEndpoint, ProcessSessionRenewalReason, PvcUid, ReplicaId,
    ReplicaIdentity, ReplicaInstanceId, ReplicaRole, derive_agent_generation,
    derive_initialization_id,
};

pub struct Reconciler {
    api: Arc<dyn ProductionClusterApi>,
    evaluation: EvaluationConfig,
    locks: Mutex<BTreeMap<String, Arc<Mutex<()>>>>,
}

async fn cleanup_stale_scaffolding(
    raw: &crate::observation::RawObservation,
    current: Option<&ConfigurationDescriptor>,
    api: &dyn ProductionClusterApi,
) -> Result<bool> {
    let Some(current) = current else {
        return Ok(false);
    };
    let extra_replica_id = raw
        .pods
        .iter()
        .map(|resource| resource.labels())
        .chain(raw.pvcs.iter().map(|resource| resource.labels()))
        .chain(raw.services.iter().map(|resource| resource.labels()))
        .filter_map(|labels| {
            labels
                .get(crate::crd::REPLICA_ID_LABEL)
                .and_then(|value| value.parse::<i64>().ok())
        })
        .find(|replica_id| {
            *replica_id > i64::from(raw.set.spec.replicas)
                && !current
                    .members
                    .iter()
                    .any(|member| member.identity.replica_id.value() == *replica_id)
        });
    if let Some(replica_id) = extra_replica_id {
        let matches_replica = |labels: &std::collections::BTreeMap<String, String>| {
            labels
                .get(crate::crd::REPLICA_ID_LABEL)
                .is_some_and(|value| value == &replica_id.to_string())
        };
        let pod = raw.pods.iter().find(|pod| matches_replica(pod.labels()));
        let pvc = raw.pvcs.iter().find(|pvc| matches_replica(pvc.labels()));
        let service = raw
            .services
            .iter()
            .find(|service| matches_replica(service.labels()));
        let pod_uid = pod
            .and_then(kube::ResourceExt::uid)
            .map(PodUid::new)
            .or_else(|| {
                service
                    .and_then(|service| service.spec.as_ref())
                    .and_then(|spec| spec.selector.as_ref())
                    .and_then(|selector| selector.get(crate::crd::INSTANCE_LABEL))
                    .map(PodUid::new)
            });
        let pvc_uid = pvc.and_then(kube::ResourceExt::uid).map(PvcUid::new);
        let pod_name = pod.map(ResourceExt::name_any);
        let pvc_name = pvc.map(ResourceExt::name_any);
        api.delete_replica_scaffolding(
            raw,
            pod_name.as_deref(),
            pod_uid.as_ref(),
            pvc_name.as_deref(),
            pvc_uid.as_ref(),
        )
        .await?;
        return Ok(true);
    }

    Ok(false)
}

pub(crate) async fn reconcile_replacement_cleanup(
    raw: &crate::observation::RawObservation,
    current: Option<&ConfigurationDescriptor>,
    scope: &PublicV1Scope,
    api: &dyn ProductionClusterApi,
) -> Result<bool> {
    let Some(status) = raw.set.status.as_ref() else {
        return Ok(false);
    };
    if status.public_v1_transition.is_some() || status.public_v1_operation.is_some() {
        return Ok(false);
    }
    let Some(replacement) = status.public_v1_replacement.as_ref() else {
        return Ok(false);
    };
    replacement
        .validate()
        .map_err(ControllerError::InvalidAgentEvidence)?;
    let Some(target) = replacement.replacement.as_ref() else {
        return Ok(false);
    };
    let admitted = current.is_some_and(|configuration| {
        configuration
            .members
            .iter()
            .any(|member| member.identity == target.replica)
    });
    if !admitted {
        return Ok(false);
    }
    let target_is_primary =
        current.is_some_and(|configuration| configuration.primary_id == target.replica.replica_id);
    if target_is_primary {
        let routed = raw
            .services
            .iter()
            .find(|service| service.name_any() == format!("{}-write", raw.set.name_any()))
            .and_then(|service| service.spec.as_ref())
            .and_then(|spec| spec.selector.as_ref())
            .and_then(|selector| selector.get(crate::crd::INSTANCE_LABEL))
            .is_some_and(|instance| instance == target.replica.instance_id.as_str());
        if !routed {
            return Ok(false);
        }
    }
    let (pod_uid, pvc_uid) = match &replacement.predecessor.storage {
        kuberic_runtime::protocol::types::ReplicaStorageIdentity::Persistent {
            pod_uid,
            pvc_uid,
        } => (Some(pod_uid), Some(pvc_uid)),
        kuberic_runtime::protocol::types::ReplicaStorageIdentity::Volatile { .. } => (None, None),
    };
    let pod_name = pod_uid.and_then(|uid| {
        raw.pods
            .iter()
            .find(|pod| pod.uid().as_deref() == Some(uid.as_str()))
            .map(ResourceExt::name_any)
    });
    let pvc_name = pvc_uid.and_then(|uid| {
        raw.pvcs
            .iter()
            .find(|pvc| pvc.uid().as_deref() == Some(uid.as_str()))
            .map(ResourceExt::name_any)
    });
    if crate::cluster_api::replica_storage_incarnation_remains(raw, &replacement.predecessor) {
        api.delete_replica_scaffolding(
            raw,
            pod_name.as_deref(),
            pod_uid,
            pvc_name.as_deref(),
            pvc_uid,
        )
        .await?;
        return Ok(true);
    }
    api.complete_public_v1_replacement(scope, &replacement.operation_id)
        .await
        .map_err(ControllerError::InvalidAgentEvidence)?;
    Ok(true)
}

pub(crate) async fn reconcile_unbound_replacement_scaffolding(
    raw: &crate::observation::RawObservation,
    api: &dyn ProductionClusterApi,
) -> Result<bool> {
    let Some(replacement) = raw
        .set
        .status
        .as_ref()
        .and_then(|status| status.public_v1_replacement.as_ref())
    else {
        return Ok(false);
    };
    if replacement.replacement.is_some() || replacement.retiring_replacement.is_some() {
        return Ok(false);
    }
    let predecessor = &replacement.predecessor.replica;
    if !raw.pods.iter().any(|pod| {
        pod.uid().as_deref() == Some(predecessor.instance_id.as_str())
            && pod.metadata.deletion_timestamp.is_none()
    }) {
        return Ok(false);
    }
    let replacement_ready = raw.pods.iter().any(|pod| {
        pod.labels()
            .get(crate::crd::REPLICA_ID_LABEL)
            .is_some_and(|value| value == &predecessor.replica_id.to_string())
            && pod.uid().as_deref() != Some(predecessor.instance_id.as_str())
            && pod.uid().is_some_and(|pod_uid| {
                raw.services.iter().any(|service| {
                    service.spec.as_ref().is_some_and(|spec| {
                        spec.selector.as_ref().is_some_and(|selector| {
                            selector
                                .get(crate::crd::INSTANCE_LABEL)
                                .is_some_and(|selected| selected == &pod_uid)
                        })
                    })
                })
            })
    });
    if replacement_ready {
        return Ok(false);
    }
    api.ensure_replacement_scaffolding(raw, predecessor.replica_id, predecessor)
        .await?;
    Ok(true)
}

fn required_session_renewal(
    topology: &crate::crd::PublicV1TopologyState,
    reports: &[PublicV1AgentReport],
    accepted: &kuberic_runtime::protocol::public_operations::PublicV1AcceptedStatus,
) -> Result<Option<(ProcessSessionRenewalRequired, PublicV1AgentReport)>> {
    let configuration = topology
        .configuration()
        .map_err(ControllerError::InvalidAgentEvidence)?;
    let mut changed = None;
    for member in &configuration.members {
        let Some(report) = reports.iter().find(|report| {
            report.replica == member.identity
                && topology.sessions.iter().any(|session| {
                    session.identity == member.identity
                        && session.storage_incarnation == report.storage_incarnation
                })
        }) else {
            return Ok(None);
        };
        let Some(session) = topology
            .sessions
            .iter()
            .find(|session| session.identity == member.identity)
        else {
            return Err(ControllerError::InvalidAgentEvidence(
                "public-v1 topology member lacks its accepted session".into(),
            ));
        };
        let session_changed = session.process_session_id != report.process_session_id;
        let creation_changed = session.replicator_creation_id != report.replicator_creation_id;
        if session_changed != creation_changed {
            return Err(ControllerError::InvalidAgentEvidence(
                "public-v1 process replacement changed only one session identity component".into(),
            ));
        }
        let admitted = report.role == member.role
            && match member.role {
                ReplicaRole::Primary => report.write_status == AccessStatus::Granted,
                ReplicaRole::ActiveSecondary => report.read_status == AccessStatus::Granted,
                _ => false,
            };
        let contained_restart = report.role == ReplicaRole::None
            && report.read_status == AccessStatus::NotPrimary
            && report.write_status == AccessStatus::NotPrimary
            && report.progress.role == PublicProgressRole::Recovery
            && report.service_location.is_none();
        if session_changed && !admitted && !contained_restart {
            return Err(ControllerError::InvalidAgentEvidence(
                "public-v1 process replacement is neither admitted nor fully contained".into(),
            ));
        }
        if session_changed
            && contained_restart
            && report
                .replicator_creation_id
                .as_str()
                .starts_with("pending-")
        {
            return Ok(None);
        }
        if !session_changed && !admitted {
            return Ok(None);
        }
        if session.storage_incarnation != report.storage_incarnation
            || session.process_session_id != report.process_session_id
            || session.replicator_creation_id != report.replicator_creation_id
            || session.role != report.role
        {
            if changed.is_some() {
                return Err(ControllerError::InvalidAgentEvidence(
                    "public-v1 topology changed multiple process sessions at once".into(),
                ));
            }
            changed = Some((session.clone(), report.clone()));
        }
    }
    let Some((predecessor, report)) = changed else {
        return Ok(None);
    };
    let renewal_epoch = Epoch::new(
        configuration.epoch.data_loss_number,
        configuration.epoch.configuration_number + 1,
    );
    let required = ProcessSessionRenewalRequired {
        operation_id: OperationId::new(format!(
            "session-renewal-{}-{}-{}",
            configuration.configuration_id, report.replica.replica_id, report.process_session_id
        )),
        reason: match predecessor.role {
            ReplicaRole::Primary => ProcessSessionRenewalReason::PrimaryReplacement,
            ReplicaRole::ActiveSecondary => ProcessSessionRenewalReason::ActiveSecondaryReplacement,
            _ => {
                return Err(ControllerError::InvalidAgentEvidence(
                    "public-v1 session predecessor is not active".into(),
                ));
            }
        },
        resource_uid: accepted.history.resource_uid.clone(),
        history: accepted.history.clone(),
        previous_epoch: configuration.epoch,
        renewal_epoch,
        predecessor: predecessor.clone(),
        replacement: Some(ProcessSessionEndpoint {
            identity: report.replica.clone(),
            storage_incarnation: report.storage_incarnation.clone(),
            process_session_id: report.process_session_id.clone(),
            replicator_creation_id: report.replicator_creation_id.clone(),
            role: ReplicaRole::None,
        }),
        surviving_sessions: topology
            .sessions
            .iter()
            .filter(|session| session.identity.replica_id != predecessor.identity.replica_id)
            .cloned()
            .collect(),
        fresh_configuration: ConfigurationDescriptor::new(
            renewal_epoch,
            configuration.primary_id,
            configuration.members.clone(),
            configuration.write_quorum,
        ),
        predecessor_contained: true,
        data_loss: None,
    };
    let mut admission_report = report;
    admission_report.role = ReplicaRole::None;
    admission_report.read_status = AccessStatus::NotPrimary;
    admission_report.write_status = AccessStatus::NotPrimary;
    admission_report.progress.role = PublicProgressRole::Recovery;
    admission_report.service_location = None;
    required
        .validate_replacement_report(&admission_report)
        .map_err(|message| ControllerError::InvalidAgentEvidence(message.into()))?;
    Ok(Some((required, admission_report)))
}

impl Reconciler {
    pub fn new(api: Arc<dyn ProductionClusterApi>, evaluation: EvaluationConfig) -> Self {
        Self {
            api,
            evaluation,
            locks: Mutex::new(BTreeMap::new()),
        }
    }

    pub async fn reconcile(&self, namespace: &str, name: &str) -> Result<ReconcileAction> {
        let key = format!("{namespace}/{name}");
        let lock = {
            let mut locks = self.locks.lock().await;
            locks
                .entry(key.clone())
                .or_insert_with(|| Arc::new(Mutex::new(())))
                .clone()
        };
        let _guard = lock.lock().await;
        let raw = self.api.observe(namespace, name).await?;
        if !raw.failures.is_empty() {
            return Err(ControllerError::TransientObservation(
                raw.failures
                    .iter()
                    .map(|failure| format!("{}: {}", failure.source, failure.message))
                    .collect::<Vec<_>>()
                    .join("; "),
            ));
        }
        let resource_uid = raw
            .set
            .uid()
            .map(ResourceUid::new)
            .ok_or_else(|| ControllerError::Observation("KubericSet has no UID".into()))?;
        let scope = PublicV1Scope {
            namespace: namespace.to_string(),
            name: name.to_string(),
            resource_uid: resource_uid.clone(),
        };
        let accepted = self
            .api
            .observe_public_v1_status(&scope)
            .await
            .map_err(ControllerError::InvalidAgentEvidence)?;
        let current_topology = self
            .api
            .observe_public_v1_topology(&scope)
            .await
            .map_err(ControllerError::InvalidAgentEvidence)?;
        if reconcile_replacement_cleanup(&raw, current_topology.as_ref(), &scope, self.api.as_ref())
            .await?
        {
            return Ok(ReconcileAction {
                requeue_after: Duration::ZERO,
                kind: ReconcileKind::Executed,
            });
        }
        if raw
            .set
            .status
            .as_ref()
            .and_then(|status| status.public_v1_transition.as_ref())
            .is_some()
        {
            let command = raw
                .set
                .status
                .as_ref()
                .and_then(|status| status.public_v1_operation.clone())
                .ok_or_else(|| {
                    ControllerError::InvalidAgentEvidence(
                        "public-v1 transition lacks its operation command".into(),
                    )
                })?
                .command()
                .map_err(ControllerError::InvalidAgentEvidence)?;
            crate::public_v1_reconciliation::reconcile_public_v1_operation(
                self.api.as_ref(),
                &scope,
                kuberic_runtime::protocol::command::ProtocolCommand::PublicV1Operation(Box::new(
                    command,
                )),
            )
            .await
            .map_err(ControllerError::InvalidAgentEvidence)?;
            return Ok(ReconcileAction {
                requeue_after: Duration::ZERO,
                kind: ReconcileKind::Executed,
            });
        }
        let frozen_topology = raw
            .set
            .status
            .as_ref()
            .and_then(|status| status.public_v1_topology.as_ref())
            .filter(|topology| topology.pending_switchover.is_some());
        let desired_replicas = frozen_topology.map_or(raw.set.spec.replicas, |topology| {
            topology
                .sessions
                .iter()
                .map(|session| session.identity.replica_id.value() as u32)
                .max()
                .unwrap_or(raw.set.spec.replicas)
        });
        if frozen_topology.is_none()
            && cleanup_stale_scaffolding(&raw, current_topology.as_ref(), self.api.as_ref()).await?
        {
            return Ok(ReconcileAction {
                requeue_after: Duration::ZERO,
                kind: ReconcileKind::Executed,
            });
        }
        let replica_ids = (1..=desired_replicas)
            .map(|replica_id| {
                kuberic_runtime::protocol::types::ReplicaId::new(i64::from(replica_id))
            })
            .collect::<Vec<_>>();
        if reconcile_unbound_replacement_scaffolding(&raw, self.api.as_ref()).await? {
            return Ok(ReconcileAction {
                requeue_after: Duration::ZERO,
                kind: ReconcileKind::Executed,
            });
        }
        if frozen_topology.is_none()
            && bootstrap_resources_missing(&raw, &replica_ids, current_topology.as_ref())
        {
            self.api.ensure_replica_support(&raw).await?;
            self.api
                .ensure_replica_scaffolding(&raw, &replica_ids)
                .await?;
            return Ok(ReconcileAction {
                requeue_after: Duration::ZERO,
                kind: ReconcileKind::Executed,
            });
        }
        let missing_primary_has_surviving_quorum =
            current_topology.as_ref().is_some_and(|configuration| {
                raw.agents
                    .values()
                    .filter(|observation| {
                        matches!(
                            observation,
                            RawAgentObservation::PublicV1Report(report)
                                if report.replica.replica_id != configuration.primary_id
                                    && configuration
                                        .members
                                        .iter()
                                        .any(|member| member.identity == report.replica)
                        )
                    })
                    .count()
                    >= configuration.write_quorum as usize
            });
        let replacement_active = raw
            .set
            .status
            .as_ref()
            .and_then(|status| status.public_v1_replacement.as_ref())
            .is_some();
        if let Some(configuration) = current_topology.as_ref()
            && let Some(member) = configuration.members.iter().find(|member| {
                if member.identity.replica_id.value() > i64::from(desired_replicas) {
                    return false;
                }
                if member.identity.replica_id == configuration.primary_id
                    && missing_primary_has_surviving_quorum
                    && !replacement_active
                {
                    return false;
                }
                !raw.pods.iter().any(|pod| {
                    pod.uid().as_deref() == Some(member.identity.instance_id.as_str())
                        && pod.metadata.deletion_timestamp.is_none()
                })
            })
        {
            let replacement_state = raw
                .set
                .status
                .as_ref()
                .and_then(|status| status.public_v1_replacement.as_ref());
            if replacement_state.is_none() {
                let predecessor = raw
                    .set
                    .status
                    .as_ref()
                    .and_then(|status| status.public_v1_topology.as_ref())
                    .and_then(|topology| {
                        topology
                            .sessions
                            .iter()
                            .find(|session| session.identity == member.identity)
                    })
                    .ok_or_else(|| {
                        ControllerError::InvalidAgentEvidence(
                            "admitted replacement loss lacks predecessor session".into(),
                        )
                    })?;
                self.api
                    .begin_public_v1_replacement(
                        &scope,
                        &configuration.configuration_id,
                        &predecessor.storage_incarnation,
                    )
                    .await
                    .map_err(ControllerError::InvalidAgentEvidence)?;
                return Ok(ReconcileAction {
                    requeue_after: Duration::ZERO,
                    kind: ReconcileKind::Applied,
                });
            }
            let replacement_ready = raw.pods.iter().any(|pod| {
                pod.labels()
                    .get(crate::crd::REPLICA_ID_LABEL)
                    .is_some_and(|value| value == &member.identity.replica_id.to_string())
                    && pod.uid().as_deref() != Some(member.identity.instance_id.as_str())
                    && pod.uid().is_some_and(|pod_uid| {
                        raw.services.iter().any(|service| {
                            service.spec.as_ref().is_some_and(|spec| {
                                spec.selector.as_ref().is_some_and(|selector| {
                                    selector
                                        .get(crate::crd::INSTANCE_LABEL)
                                        .is_some_and(|selected| selected == &pod_uid)
                                })
                            })
                        })
                    })
            });
            if !replacement_ready {
                self.api
                    .ensure_replacement_scaffolding(
                        &raw,
                        member.identity.replica_id,
                        &member.identity,
                    )
                    .await?;
                return Ok(ReconcileAction {
                    requeue_after: Duration::ZERO,
                    kind: ReconcileKind::Executed,
                });
            }
        }
        if let Some(topology) = frozen_topology
            && !topology.sessions.iter().all(|session| {
                raw.agents.values().any(|observation| {
                    matches!(observation, RawAgentObservation::PublicV1Report(report)
                    if report.replica == session.identity
                        && report.storage_incarnation == session.storage_incarnation
                        && report.process_session_id == session.process_session_id
                        && report.replicator_creation_id == session.replicator_creation_id)
                })
            })
        {
            return Ok(ReconcileAction {
                requeue_after: Duration::from_secs(self.evaluation.wait_requeue_seconds),
                kind: ReconcileKind::Waiting,
            });
        }
        let replacement_initialization_blocked_for_failover = raw
            .set
            .status
            .as_ref()
            .and_then(|status| status.public_v1_replacement.as_ref())
            .is_some_and(|replacement| {
                replacement.replacement.is_none()
                    && replacement.retiring_replacement.is_none()
                    && !raw.pods.iter().any(|pod| {
                        pod.uid().as_deref()
                            == Some(replacement.predecessor.replica.instance_id.as_str())
                            && pod.metadata.deletion_timestamp.is_none()
                    })
                    && current_topology.as_ref().is_some_and(|configuration| {
                        configuration.primary_id == replacement.predecessor.replica.replica_id
                    })
            });
        if let Some(replacement) = raw
            .set
            .status
            .as_ref()
            .and_then(|status| status.public_v1_replacement.as_ref())
            && replacement.replacement.is_none()
        {
            let target_observed = raw.agents.iter().any(|(key, observation)| {
                key.replica_id == replacement.predecessor.replica.replica_id
                    && key.instance_id != replacement.predecessor.replica.instance_id
                    && matches!(
                        observation,
                        RawAgentObservation::Uninitialized(_)
                            | RawAgentObservation::PublicV1Report(_)
                    )
            });
            if !target_observed && !replacement_initialization_blocked_for_failover {
                return Ok(ReconcileAction {
                    requeue_after: Duration::from_secs(self.evaluation.wait_requeue_seconds),
                    kind: ReconcileKind::Waiting,
                });
            }
        }
        if !replacement_initialization_blocked_for_failover
            && let Some(command) = next_initialization_command(
                &raw,
                &resource_uid,
                &replica_ids,
                current_topology.as_ref(),
            )?
        {
            self.api
                .execute_command(
                    &raw,
                    &ProtocolCommand::InitializeAgentStore(Box::new(command)),
                )
                .await?;
            return Ok(ReconcileAction {
                requeue_after: Duration::ZERO,
                kind: ReconcileKind::Executed,
            });
        }
        if let Some(message) = raw
            .agents
            .values()
            .find_map(|observation| match observation {
                RawAgentObservation::Invalid { message } => Some(message.as_str()),
                _ => None,
            })
        {
            return Err(ControllerError::InvalidAgentEvidence(message.to_string()));
        }
        let reports = raw
            .agents
            .values()
            .filter_map(|observation| match observation {
                RawAgentObservation::PublicV1Report(report) => Some((**report).clone()),
                _ => None,
            })
            .collect::<Vec<_>>();
        let status = raw.set.status.as_ref();
        if let Some(replacement) = status.and_then(|status| status.public_v1_replacement.as_ref())
            && replacement.replacement.is_none()
            && let Some(target) = reports.iter().find(|report| {
                report.replica.replica_id == replacement.predecessor.replica.replica_id
                    && report.replica != replacement.predecessor.replica
                    && report.storage_incarnation != replacement.predecessor
            })
        {
            self.api
                .bind_public_v1_replacement(
                    &scope,
                    &replacement.operation_id,
                    &target.storage_incarnation,
                )
                .await
                .map_err(ControllerError::InvalidAgentEvidence)?;
            return Ok(ReconcileAction {
                requeue_after: Duration::ZERO,
                kind: ReconcileKind::Applied,
            });
        }
        if let Some(replacement) = status.and_then(|status| status.public_v1_replacement.as_ref())
            && replacement.replacement.as_ref().is_none_or(|target| {
                !reports
                    .iter()
                    .any(|report| report.storage_incarnation == *target)
            })
            && !replacement_initialization_blocked_for_failover
        {
            return Ok(ReconcileAction {
                requeue_after: Duration::from_secs(self.evaluation.wait_requeue_seconds),
                kind: ReconcileKind::Waiting,
            });
        }
        if let Some(renewal) = status
            .and_then(|status| status.public_v1_session_renewal.as_ref())
            .map(crate::crd::PublicV1SessionRenewalState::renewal)
            .transpose()
            .map_err(ControllerError::InvalidAgentEvidence)?
        {
            if renewal.stage == PublicSessionRenewalStage::Authorized {
                self.api
                    .complete_public_v1_session_renewal(&scope, &renewal)
                    .await
                    .map_err(ControllerError::InvalidAgentEvidence)?;
                return Ok(ReconcileAction {
                    requeue_after: Duration::ZERO,
                    kind: ReconcileKind::Applied,
                });
            }
            let unreachable = Vec::new();
            let plan = plan_session_renewal(&renewal, &unreachable)
                .map_err(ControllerError::InvalidAgentEvidence)?;
            crate::executor::execute_public_v1_session_renewal(self.api.as_ref(), &scope, plan)
                .await
                .map_err(ControllerError::InvalidAgentEvidence)?;
            return Ok(ReconcileAction {
                requeue_after: Duration::ZERO,
                kind: ReconcileKind::Executed,
            });
        }
        let Some(report) = reports
            .iter()
            .find(|report| report.resource_uid == resource_uid)
        else {
            return Ok(ReconcileAction {
                requeue_after: Duration::from_secs(self.evaluation.wait_requeue_seconds),
                kind: ReconcileKind::Waiting,
            });
        };
        if let Some(accepted) = accepted.as_ref()
            && report.identity == accepted.identity
            && report.history == accepted.history
            && report.data_loss_recovery.is_some()
        {
            let plan = plan_recovery_report(accepted, report)
                .map_err(ControllerError::InvalidAgentEvidence)?;
            crate::executor::execute_public_v1_history(self.api.as_ref(), &scope, plan)
                .await
                .map_err(ControllerError::InvalidAgentEvidence)?;
            return Ok(ReconcileAction {
                requeue_after: Duration::ZERO,
                kind: ReconcileKind::Applied,
            });
        }
        let Some(accepted) = accepted.as_ref() else {
            let plan = plan_bootstrap_history(
                None,
                &resource_uid,
                &report.identity,
                report.storage_incarnation.clone(),
            )
            .map_err(ControllerError::InvalidAgentEvidence)?;
            let kind = match plan_kind_for_history(&plan) {
                ExecutionKind::Applied => ReconcileKind::Applied,
                ExecutionKind::Stable => ReconcileKind::Stable,
                _ => ReconcileKind::Executed,
            };
            crate::executor::execute_public_v1_history(self.api.as_ref(), &scope, plan)
                .await
                .map_err(ControllerError::InvalidAgentEvidence)?;
            return Ok(ReconcileAction {
                requeue_after: Duration::ZERO,
                kind,
            });
        };
        if reports.iter().any(|report| {
            report.identity != accepted.identity
                || report.resource_uid != resource_uid
                || report.history != accepted.history
        }) {
            return Err(ControllerError::InvalidAgentEvidence(
                "public-v1 report differs from accepted history".into(),
            ));
        }
        let current = current_topology;
        if let Some(topology) = raw
            .set
            .status
            .as_ref()
            .and_then(|status| status.public_v1_topology.as_ref())
            && let Some((required, replacement_report)) =
                required_session_renewal(topology, &reports, accepted)?
        {
            let plan = accept_session_renewal(None, required, Some(&replacement_report))
                .map_err(ControllerError::InvalidAgentEvidence)?;
            crate::executor::execute_public_v1_session_renewal(self.api.as_ref(), &scope, plan)
                .await
                .map_err(ControllerError::InvalidAgentEvidence)?;
            return Ok(ReconcileAction {
                requeue_after: Duration::ZERO,
                kind: ReconcileKind::Applied,
            });
        }
        if let Some(configuration) = current.as_ref() {
            let primary_identity = configuration
                .members
                .iter()
                .find(|member| member.role == ReplicaRole::Primary)
                .map(|member| &member.identity)
                .ok_or_else(|| {
                    ControllerError::InvalidAgentEvidence(
                        "accepted topology lacks exact primary identity".into(),
                    )
                })?;
            let primary_present = reports
                .iter()
                .any(|report| &report.replica == primary_identity);
            let topology_state = raw
                .set
                .status
                .as_ref()
                .and_then(|status| status.public_v1_topology.as_ref())
                .ok_or_else(|| {
                    ControllerError::InvalidAgentEvidence(
                        "accepted topology lacks typed state".into(),
                    )
                })?;
            if !primary_present {
                let since = match (
                    topology_state.unavailable_primary_id,
                    topology_state.unavailable_since_unix_seconds,
                ) {
                    (Some(primary), Some(since)) if primary == configuration.primary_id => since,
                    _ => {
                        self.api
                            .persist_public_v1_primary_unavailable(
                                &scope,
                                &configuration.configuration_id,
                                Some((configuration.primary_id, raw.now_unix_seconds)),
                            )
                            .await
                            .map_err(ControllerError::InvalidAgentEvidence)?;
                        return Ok(ReconcileAction {
                            requeue_after: Duration::from_secs(
                                self.evaluation.wait_requeue_seconds,
                            ),
                            kind: ReconcileKind::Applied,
                        });
                    }
                };
                if raw.now_unix_seconds.saturating_sub(since)
                    < raw.set.spec.failover_delay_seconds as i64
                {
                    return Ok(ReconcileAction {
                        requeue_after: Duration::from_secs(self.evaluation.wait_requeue_seconds),
                        kind: ReconcileKind::Waiting,
                    });
                }
            } else if topology_state.unavailable_primary_id.is_some() {
                self.api
                    .persist_public_v1_primary_unavailable(
                        &scope,
                        &configuration.configuration_id,
                        None,
                    )
                    .await
                    .map_err(ControllerError::InvalidAgentEvidence)?;
                return Ok(ReconcileAction {
                    requeue_after: Duration::ZERO,
                    kind: ReconcileKind::Applied,
                });
            }
        }
        let mut topology_replicas = topology_replicas(&raw, current.as_ref(), &reports)?;
        if let Some(topology) = frozen_topology {
            topology_replicas.retain(|replica| {
                topology
                    .sessions
                    .iter()
                    .any(|session| session.identity == replica.report.replica)
            });
        }
        let completed_switchover_request_id = raw
            .set
            .status
            .as_ref()
            .and_then(|status| status.public_v1_topology.as_ref())
            .and_then(|topology| topology.completed_switchover_request_id.as_deref());
        let active_switchover = frozen_topology
            .and_then(|topology| topology.pending_switchover.as_ref())
            .or_else(|| {
                raw.set.spec.switchover.as_ref().filter(|request| {
                    completed_switchover_request_id != Some(request.request_id.as_str())
                })
            });
        let current_primary_available = current.as_ref().is_some_and(|configuration| {
            topology_replicas.iter().any(|replica| {
                configuration.members.iter().any(|member| {
                    member.role == ReplicaRole::Primary && member.identity == replica.report.replica
                })
            })
        });
        if frozen_topology.is_none()
            && current_primary_available
            && active_switchover.is_some_and(|request| {
                current.as_ref().is_some_and(|configuration| {
                    i64::from(request.target_replica_id) != configuration.primary_id.value()
                        && configuration.members.iter().any(|member| {
                            member.identity.replica_id.value()
                                == i64::from(request.target_replica_id)
                        })
                })
            })
        {
            let configuration = current.as_ref().expect("checked current topology");
            self.api
                .persist_public_v1_topology(
                    &scope,
                    Some(&configuration.configuration_id),
                    configuration,
                )
                .await
                .map_err(ControllerError::InvalidAgentEvidence)?;
            return Ok(ReconcileAction {
                requeue_after: Duration::ZERO,
                kind: ReconcileKind::Applied,
            });
        }
        let requested_switchover_target =
            active_switchover.map(|request| ReplicaId::new(i64::from(request.target_replica_id)));
        let switchover_target = requested_switchover_target.filter(|target| {
            current_primary_available
                || topology_replicas
                    .iter()
                    .any(|replica| replica.report.replica.replica_id == *target)
        });
        let routed_primary = raw
            .services
            .iter()
            .find(|service| service.name_any() == format!("{}-write", raw.set.name_any()))
            .and_then(|service| service.spec.as_ref())
            .and_then(|spec| spec.selector.as_ref())
            .and_then(|selector| selector.get(crate::crd::INSTANCE_LABEL))
            .and_then(|instance| {
                reports
                    .iter()
                    .find(|report| report.replica.instance_id.as_str() == instance)
            })
            .map(|report| report.replica.replica_id);
        let resource_version = raw
            .set
            .resource_version()
            .ok_or(ControllerError::ObservationStale)?;
        match plan_public_v1_topology(
            accepted,
            current.as_ref(),
            &topology_replicas,
            desired_replicas,
            switchover_target,
            routed_primary,
            &resource_version,
        )
        .map_err(ControllerError::InvalidAgentEvidence)?
        {
            PublicV1TopologyPlan::Begin {
                transition,
                command,
            } => {
                self.api
                    .begin_public_v1_operation(&scope, accepted, &transition, &command)
                    .await
                    .map_err(ControllerError::InvalidAgentEvidence)?;
                Ok(ReconcileAction {
                    requeue_after: Duration::ZERO,
                    kind: ReconcileKind::Applied,
                })
            }
            PublicV1TopologyPlan::PersistTopology { expected, next } => {
                if let Some(current) = current.as_ref()
                    && let Some(removed) = current.members.iter().find(|member| {
                        member.identity.replica_id.value() <= i64::from(desired_replicas)
                            && !next
                                .members
                                .iter()
                                .any(|candidate| candidate.identity == member.identity)
                            && !raw.pods.iter().any(|pod| {
                                pod.uid().as_deref() == Some(member.identity.instance_id.as_str())
                                    && pod.metadata.deletion_timestamp.is_none()
                            })
                    })
                    && raw
                        .set
                        .status
                        .as_ref()
                        .and_then(|status| status.public_v1_replacement.as_ref())
                        .is_none()
                {
                    let predecessor = raw
                        .set
                        .status
                        .as_ref()
                        .and_then(|status| status.public_v1_topology.as_ref())
                        .and_then(|topology| {
                            topology
                                .sessions
                                .iter()
                                .find(|session| session.identity == removed.identity)
                        })
                        .ok_or_else(|| {
                            ControllerError::InvalidAgentEvidence(
                                "lost topology member lacks predecessor session".into(),
                            )
                        })?;
                    self.api
                        .begin_public_v1_replacement(
                            &scope,
                            &current.configuration_id,
                            &predecessor.storage_incarnation,
                        )
                        .await
                        .map_err(ControllerError::InvalidAgentEvidence)?;
                    return Ok(ReconcileAction {
                        requeue_after: Duration::ZERO,
                        kind: ReconcileKind::Applied,
                    });
                }
                self.api
                    .persist_public_v1_topology(&scope, expected.as_ref(), &next)
                    .await
                    .map_err(ControllerError::InvalidAgentEvidence)?;
                Ok(ReconcileAction {
                    requeue_after: Duration::ZERO,
                    kind: ReconcileKind::Applied,
                })
            }
            PublicV1TopologyPlan::RoutePrimary { primary } => {
                self.api
                    .converge_public_v1_write_service(&raw, Some(&primary))
                    .await?;
                Ok(ReconcileAction {
                    requeue_after: Duration::ZERO,
                    kind: ReconcileKind::Applied,
                })
            }
            PublicV1TopologyPlan::Wait => Ok(ReconcileAction {
                requeue_after: Duration::from_secs(self.evaluation.wait_requeue_seconds),
                kind: ReconcileKind::Waiting,
            }),
            PublicV1TopologyPlan::Stable => {
                if active_switchover.is_some()
                    && current.as_ref().is_some_and(|configuration| {
                        Some(configuration.primary_id) == switchover_target
                    })
                    && routed_primary == switchover_target
                {
                    let configuration = current.as_ref().expect("checked current topology");
                    self.api
                        .persist_public_v1_topology(
                            &scope,
                            Some(&configuration.configuration_id),
                            configuration,
                        )
                        .await
                        .map_err(ControllerError::InvalidAgentEvidence)?;
                    return Ok(ReconcileAction {
                        requeue_after: Duration::ZERO,
                        kind: ReconcileKind::Applied,
                    });
                }
                let primary = current
                    .as_ref()
                    .and_then(|configuration| {
                        configuration
                            .members
                            .iter()
                            .find(|member| member.role == ReplicaRole::Primary)
                    })
                    .map(|member| &member.identity);
                self.api
                    .converge_public_v1_write_service(&raw, primary)
                    .await?;
                Ok(ReconcileAction {
                    requeue_after: Duration::from_secs(self.evaluation.stable_resync_seconds),
                    kind: ReconcileKind::Stable,
                })
            }
        }
    }
}

pub(crate) fn next_initialization_command(
    raw: &crate::observation::RawObservation,
    resource_uid: &ResourceUid,
    replica_ids: &[ReplicaId],
    current: Option<&ConfigurationDescriptor>,
) -> Result<Option<InitializeAgentStore>> {
    let policy = EffectivePolicy::fixed(raw.set.spec.replicas, raw.set.spec.failover_delay_seconds)
        .ok_or_else(|| ControllerError::InvalidAgentEvidence("invalid replica policy".into()))?;
    let mut members = Vec::with_capacity(replica_ids.len());
    for replica_id in replica_ids {
        let replacement_requires_fresh_identity = raw
            .set
            .status
            .as_ref()
            .and_then(|status| status.public_v1_replacement.as_ref())
            .is_some_and(|replacement| {
                replacement.replacement.is_none()
                    && replacement.retiring_replacement.is_none()
                    && replacement.predecessor.replica.replica_id == *replica_id
                    && (raw.pods.iter().any(|pod| {
                        pod.uid().as_deref()
                            == Some(replacement.predecessor.replica.instance_id.as_str())
                            && pod.metadata.deletion_timestamp.is_none()
                    }) || current.is_some_and(|configuration| {
                        configuration.primary_id != replacement.predecessor.replica.replica_id
                    }))
            });
        let fresh_identity = replacement_requires_fresh_identity.then(|| {
            raw.agents
                .values()
                .find_map(|observation| match observation {
                    RawAgentObservation::Uninitialized(report)
                        if report.replica_id == *replica_id =>
                    {
                        let initialization_id = derive_initialization_id(
                            resource_uid,
                            *replica_id,
                            &report.pod_uid,
                            &report.pvc_uid,
                        );
                        Some(ReplicaIdentity {
                            replica_id: *replica_id,
                            instance_id: ReplicaInstanceId::new(report.pod_uid.as_str()),
                            agent_generation: derive_agent_generation(&initialization_id),
                        })
                    }
                    _ => None,
                })
        });
        let identity = if let Some(identity) = fresh_identity.flatten() {
            identity
        } else if raw.agents.keys().any(|key| key.replica_id == *replica_id) {
            observed_or_derived_identity(raw, resource_uid, *replica_id)?
        } else if let Some(identity) = current.and_then(|configuration| {
            configuration
                .members
                .iter()
                .find(|member| member.identity.replica_id == *replica_id)
                .map(|member| member.identity.clone())
        }) {
            identity
        } else {
            observed_or_derived_identity(raw, resource_uid, *replica_id)?
        };
        members.push(ConfigurationMember {
            role: if *replica_id == replica_ids[0] {
                ReplicaRole::Primary
            } else {
                ReplicaRole::ActiveSecondary
            },
            identity,
        });
    }
    let configuration = ConfigurationDescriptor::new(
        Epoch::new(0, 1),
        replica_ids[0],
        members.clone(),
        policy.write_quorum,
    );
    for member in members {
        let key = raw.agents.iter().find_map(|(key, observation)| {
            (key.replica_id == member.identity.replica_id
                && matches!(observation, RawAgentObservation::Uninitialized(_)))
            .then_some(key)
        });
        let Some(key) = key else {
            continue;
        };
        let RawAgentObservation::Uninitialized(report) =
            raw.agents.get(key).expect("selected fresh observation")
        else {
            unreachable!()
        };
        let initialization_id = derive_initialization_id(
            resource_uid,
            report.replica_id,
            &report.pod_uid,
            &report.pvc_uid,
        );
        if member.identity.instance_id != ReplicaInstanceId::new(report.pod_uid.as_str())
            || member.identity.agent_generation != derive_agent_generation(&initialization_id)
        {
            return Err(ControllerError::InvalidAgentEvidence(
                "fresh agent identity differs from desired bootstrap member".into(),
            ));
        }
        return Ok(Some(InitializeAgentStore {
            initialization_id,
            resource_uid: resource_uid.clone(),
            local_replica_id: report.replica_id,
            expected_instance_id: member.identity.instance_id,
            expected_pod_uid: report.pod_uid.clone(),
            expected_pvc_uid: report.pvc_uid.clone(),
            assigned_agent_generation: member.identity.agent_generation,
            effective_policy: policy,
            bootstrap_configuration: configuration,
            provisioning: None,
        }));
    }
    Ok(None)
}

fn observed_or_derived_identity(
    raw: &crate::observation::RawObservation,
    resource_uid: &ResourceUid,
    replica_id: ReplicaId,
) -> Result<ReplicaIdentity> {
    if let Some(report) = raw
        .agents
        .values()
        .find_map(|observation| match observation {
            RawAgentObservation::PublicV1Report(report)
                if report.replica.replica_id == replica_id =>
            {
                Some(report.replica.clone())
            }
            _ => None,
        })
    {
        return Ok(report);
    }
    if let Some(fresh) = raw
        .agents
        .values()
        .find_map(|observation| match observation {
            RawAgentObservation::Uninitialized(report) if report.replica_id == replica_id => {
                Some(report)
            }
            _ => None,
        })
    {
        let initialization_id =
            derive_initialization_id(resource_uid, replica_id, &fresh.pod_uid, &fresh.pvc_uid);
        return Ok(ReplicaIdentity {
            replica_id,
            instance_id: ReplicaInstanceId::new(fresh.pod_uid.as_str()),
            agent_generation: derive_agent_generation(&initialization_id),
        });
    }
    let pod_name = format!("{}-{}", raw.set.name_any(), replica_id.value());
    let pvc_name = format!("{pod_name}-data");
    let pod_uid = raw
        .pods
        .iter()
        .find(|pod| pod.name_any() == pod_name)
        .and_then(kube::ResourceExt::uid)
        .map(PodUid::new)
        .ok_or(ControllerError::ObservationStale)?;
    let pvc_uid = raw
        .pvcs
        .iter()
        .find(|pvc| pvc.name_any() == pvc_name)
        .and_then(kube::ResourceExt::uid)
        .map(PvcUid::new)
        .ok_or(ControllerError::ObservationStale)?;
    let initialization_id = derive_initialization_id(resource_uid, replica_id, &pod_uid, &pvc_uid);
    Ok(ReplicaIdentity {
        replica_id,
        instance_id: ReplicaInstanceId::new(pod_uid.as_str()),
        agent_generation: derive_agent_generation(&initialization_id),
    })
}

pub(crate) fn topology_replicas(
    raw: &crate::observation::RawObservation,
    current: Option<&ConfigurationDescriptor>,
    reports: &[kuberic_runtime::protocol::public_operations::PublicV1AgentReport],
) -> Result<Vec<PublicV1TopologyReplica>> {
    let replacement = raw
        .set
        .status
        .as_ref()
        .and_then(|status| status.public_v1_replacement.as_ref())
        .and_then(|replacement| replacement.replacement.as_ref());
    reports
        .iter()
        .filter(|report| {
            if let Some(replacement) = replacement
                && replacement.replica.replica_id == report.replica.replica_id
            {
                return report.storage_incarnation == *replacement;
            }
            current
                .and_then(|configuration| {
                    configuration
                        .members
                        .iter()
                        .find(|member| member.identity.replica_id == report.replica.replica_id)
                })
                .is_none_or(|member| member.identity == report.replica)
        })
        .map(|report| {
            raw.services
                .iter()
                .find(|service| {
                    service.spec.as_ref().is_some_and(|spec| {
                        spec.selector.as_ref().is_some_and(|selector| {
                            selector
                                .get(crate::crd::INSTANCE_LABEL)
                                .is_some_and(|value| value == report.replica.instance_id.as_str())
                        })
                    }) && service.spec.as_ref().is_some_and(|spec| {
                        spec.ports.as_ref().is_some_and(|ports| {
                            ports
                                .iter()
                                .any(|port| port.name.as_deref() == Some("replication"))
                        })
                    })
                })
                .ok_or(ControllerError::ObservationStale)?;
            let pod = raw
                .pods
                .iter()
                .find(|pod| pod.uid().as_deref() == Some(report.replica.instance_id.as_str()))
                .ok_or(ControllerError::ObservationStale)?;
            pod.status
                .as_ref()
                .and_then(|status| status.pod_ip.as_deref())
                .ok_or(ControllerError::ObservationStale)?
                .parse::<IpAddr>()
                .map_err(|_| ControllerError::ObservationStale)?;
            Ok(PublicV1TopologyReplica {
                report: report.clone(),
                replication_address: report.replication_address.clone().unwrap_or_default(),
            })
        })
        .collect()
}

pub(crate) fn bootstrap_resources_missing(
    raw: &crate::observation::RawObservation,
    replica_ids: &[kuberic_runtime::protocol::types::ReplicaId],
    current: Option<&ConfigurationDescriptor>,
) -> bool {
    let set_name = raw.set.name_any();
    if !raw
        .secrets
        .iter()
        .any(|secret| secret.name_any() == format!("{set_name}-agent-credentials"))
        || !raw
            .services
            .iter()
            .any(|service| service.name_any() == format!("{set_name}-peer"))
        || !raw
            .services
            .iter()
            .any(|service| service.name_any() == format!("{set_name}-write"))
    {
        return true;
    }
    replica_ids.iter().any(|replica_id| {
        if current.is_some_and(|configuration| {
            configuration
                .members
                .iter()
                .any(|member| member.identity.replica_id == *replica_id)
        }) {
            return false;
        }
        let pod_name = format!("{set_name}-{}", replica_id.value());
        let pvc_name = format!("{pod_name}-data");
        let pod = raw.pods.iter().find(|pod| pod.name_any() == pod_name);
        let Some(pod) = pod else {
            return true;
        };
        if !raw.pvcs.iter().any(|pvc| pvc.name_any() == pvc_name) {
            return true;
        }
        let Some(pod_uid) = pod.uid() else {
            return true;
        };
        !raw.services.iter().any(|service| {
            service.spec.as_ref().is_some_and(|spec| {
                spec.selector.as_ref().is_some_and(|selector| {
                    selector
                        .get(crate::crd::INSTANCE_LABEL)
                        .is_some_and(|selected| selected == &pod_uid)
                })
            })
        })
    })
}

fn plan_kind_for_history(plan: &PublicV1HistoryPlan) -> ExecutionKind {
    match plan {
        PublicV1HistoryPlan::PersistStatus { .. } => ExecutionKind::Applied,
        PublicV1HistoryPlan::Dispatch { .. } => ExecutionKind::Executed,
        PublicV1HistoryPlan::Stable => ExecutionKind::Stable,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReconcileKind {
    Stable,
    Applied,
    Executed,
    Waiting,
    Unsafe,
    ObservationStale,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReconcileAction {
    pub requeue_after: Duration,
    pub kind: ReconcileKind,
}
