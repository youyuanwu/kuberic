use std::collections::BTreeMap;
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
    ProcessSessionRenewalRequired, PublicSessionRenewalStage, PublicV1AcceptedStatus,
    PublicV1AgentReport,
};
use kuberic_runtime::protocol::types::{
    ConfigurationDescriptor, ConfigurationMember, EffectivePolicy, Epoch, OperationId, PodUid,
    ProcessSessionEndpoint, ProcessSessionRenewalReason, PvcUid, ReplicaId, ReplicaIdentity,
    ReplicaInstanceId, ReplicaRole, derive_agent_generation, derive_initialization_id,
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
    if let Some(pod) = raw.pods.iter().find(|pod| {
        let replica_id = pod
            .labels()
            .get(crate::crd::REPLICA_ID_LABEL)
            .and_then(|value| value.parse::<i64>().ok())
            .filter(|replica_id| *replica_id > i64::from(raw.set.spec.replicas));
        replica_id.is_some_and(|replica_id| {
            !current
                .members
                .iter()
                .any(|member| member.identity.replica_id.value() == replica_id)
        })
    }) {
        let pod_uid = pod.uid().map(PodUid::new);
        let replica_id = pod
            .labels()
            .get(crate::crd::REPLICA_ID_LABEL)
            .and_then(|value| value.parse::<i64>().ok())
            .expect("selected extra Pod has replica ID");
        let pvc_name = format!("{}-{replica_id}-data", raw.set.name_any());
        let pvc_uid = raw
            .pvcs
            .iter()
            .find(|pvc| pvc.name_any() == pvc_name)
            .and_then(kube::ResourceExt::uid)
            .map(PvcUid::new);
        api.delete_replica_scaffolding(
            raw,
            Some(&pod.name_any()),
            pod_uid.as_ref(),
            Some(&pvc_name),
            pvc_uid.as_ref(),
        )
        .await?;
        return Ok(true);
    }

    for member in &current.members {
        let canonical_pvc_name = format!(
            "{}-{}-data",
            raw.set.name_any(),
            member.identity.replica_id.value()
        );
        let canonical_pvc = raw
            .pvcs
            .iter()
            .find(|pvc| pvc.name_any() == canonical_pvc_name);
        let current_pod_present = raw
            .pods
            .iter()
            .any(|pod| pod.uid().as_deref() == Some(member.identity.instance_id.as_str()));
        if canonical_pvc.is_some() && !current_pod_present {
            let pvc_uid = canonical_pvc
                .and_then(kube::ResourceExt::uid)
                .map(PvcUid::new);
            api.delete_replica_scaffolding(
                raw,
                None,
                None,
                Some(&canonical_pvc_name),
                pvc_uid.as_ref(),
            )
            .await?;
            return Ok(true);
        }
    }
    Ok(false)
}

fn required_session_renewal(
    topology: &crate::crd::PublicV1TopologyState,
    accepted: &PublicV1AcceptedStatus,
    reports: &[PublicV1AgentReport],
) -> Result<Option<(ProcessSessionRenewalRequired, PublicV1AgentReport)>> {
    let configuration = topology
        .configuration()
        .map_err(ControllerError::InvalidAgentEvidence)?;
    for predecessor in &topology.sessions {
        let Some(replacement_report) = reports.iter().find(|report| {
            report.replica == predecessor.identity
                && report.storage_incarnation == predecessor.storage_incarnation
                && (report.process_session_id != predecessor.process_session_id
                    || report.replicator_creation_id != predecessor.replicator_creation_id)
        }) else {
            continue;
        };
        let mut surviving_sessions = Vec::with_capacity(topology.sessions.len().saturating_sub(1));
        for survivor in topology
            .sessions
            .iter()
            .filter(|session| session.identity != predecessor.identity)
        {
            let Some(report) = reports.iter().find(|report| {
                report.replica == survivor.identity
                    && report.storage_incarnation == survivor.storage_incarnation
                    && report.process_session_id == survivor.process_session_id
                    && report.replicator_creation_id == survivor.replicator_creation_id
                    && report.role == survivor.role
            }) else {
                return Ok(None);
            };
            surviving_sessions.push(ProcessSessionEndpoint {
                identity: report.replica.clone(),
                storage_incarnation: report.storage_incarnation.clone(),
                process_session_id: report.process_session_id.clone(),
                replicator_creation_id: report.replicator_creation_id.clone(),
                role: report.role,
            });
        }
        let renewal_epoch = Epoch::new(
            configuration.epoch.data_loss_number,
            configuration
                .epoch
                .configuration_number
                .checked_add(1)
                .ok_or_else(|| {
                    ControllerError::InvalidAgentEvidence(
                        "public-v1 session-renewal epoch overflow".into(),
                    )
                })?,
        );
        let fresh_configuration = ConfigurationDescriptor::new(
            renewal_epoch,
            configuration.primary_id,
            configuration.members.clone(),
            configuration.write_quorum,
        );
        let replacement = ProcessSessionEndpoint {
            identity: replacement_report.replica.clone(),
            storage_incarnation: replacement_report.storage_incarnation.clone(),
            process_session_id: replacement_report.process_session_id.clone(),
            replicator_creation_id: replacement_report.replicator_creation_id.clone(),
            role: ReplicaRole::None,
        };
        let reason = match predecessor.role {
            ReplicaRole::Primary => ProcessSessionRenewalReason::PrimaryReplacement,
            ReplicaRole::ActiveSecondary => ProcessSessionRenewalReason::ActiveSecondaryReplacement,
            _ => {
                return Err(ControllerError::InvalidAgentEvidence(
                    "public-v1 topology session is not active".into(),
                ));
            }
        };
        return Ok(Some((
            ProcessSessionRenewalRequired {
                operation_id: OperationId::new(format!(
                    "session-renewal-{}-{}-{}",
                    configuration.configuration_id,
                    predecessor.identity.replica_id,
                    replacement.process_session_id.as_str()
                )),
                reason,
                resource_uid: accepted.history.resource_uid.clone(),
                history: accepted.history.clone(),
                previous_epoch: configuration.epoch,
                renewal_epoch,
                predecessor: predecessor.clone(),
                replacement: Some(replacement),
                surviving_sessions,
                fresh_configuration,
                predecessor_contained: true,
                data_loss: None,
            },
            replacement_report.clone(),
        )));
    }
    Ok(None)
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
        if cleanup_stale_scaffolding(&raw, current_topology.as_ref(), self.api.as_ref()).await? {
            return Ok(ReconcileAction {
                requeue_after: Duration::ZERO,
                kind: ReconcileKind::Executed,
            });
        }
        let replica_ids = (1..=raw.set.spec.replicas)
            .map(|replica_id| {
                kuberic_runtime::protocol::types::ReplicaId::new(i64::from(replica_id))
            })
            .collect::<Vec<_>>();
        if bootstrap_resources_missing(&raw, &replica_ids, current_topology.as_ref()) {
            self.api.ensure_replica_support(&raw).await?;
            self.api
                .ensure_replica_scaffolding(&raw, &replica_ids)
                .await?;
            return Ok(ReconcileAction {
                requeue_after: Duration::ZERO,
                kind: ReconcileKind::Executed,
            });
        }
        if let Some(configuration) = current_topology.as_ref()
            && let Some(member) = configuration.members.iter().find(|member| {
                if member.identity.replica_id.value() > i64::from(raw.set.spec.replicas) {
                    return false;
                }
                let old_missing = !raw
                    .pods
                    .iter()
                    .any(|pod| pod.uid().as_deref() == Some(member.identity.instance_id.as_str()));
                let replacement = raw.pods.iter().find(|pod| {
                    pod.labels()
                        .get(crate::crd::REPLICA_ID_LABEL)
                        .is_some_and(|value| value == &member.identity.replica_id.to_string())
                        && pod.uid().as_deref() != Some(member.identity.instance_id.as_str())
                });
                let replacement_ready = replacement.is_some_and(|pod| {
                    pod.uid().is_some_and(|pod_uid| {
                        raw.services.iter().any(|service| {
                            service.spec.as_ref().is_some_and(|spec| {
                                spec.selector.as_ref().is_some_and(|selector| {
                                    selector
                                        .get(crate::crd::INSTANCE_LABEL)
                                        .is_some_and(|selected| selected == &pod_uid)
                                }) && spec.ports.as_ref().is_some_and(|ports| {
                                    ports
                                        .iter()
                                        .any(|port| port.name.as_deref() == Some("replication"))
                                })
                            })
                        })
                    })
                });
                old_missing && !replacement_ready
            })
        {
            self.api
                .ensure_replacement_scaffolding(&raw, member.identity.replica_id, &member.identity)
                .await?;
            return Ok(ReconcileAction {
                requeue_after: Duration::ZERO,
                kind: ReconcileKind::Executed,
            });
        }
        if let Some(command) = next_initialization_command(&raw, &resource_uid, &replica_ids)? {
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
        if status
            .and_then(|status| status.public_v1_transition.as_ref())
            .is_some()
        {
            let command = status
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
                required_session_renewal(topology, accepted, &reports)?
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
            let primary_present = reports
                .iter()
                .any(|report| report.replica.replica_id == configuration.primary_id);
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
        let topology_replicas = topology_replicas(&raw, &reports)?;
        let switchover_target = raw
            .set
            .spec
            .switchover
            .as_ref()
            .map(|request| ReplicaId::new(i64::from(request.target_replica_id)));
        let resource_version = raw
            .set
            .resource_version()
            .ok_or(ControllerError::ObservationStale)?;
        match plan_public_v1_topology(
            accepted,
            current.as_ref(),
            &topology_replicas,
            raw.set.spec.replicas,
            switchover_target,
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
                self.api
                    .persist_public_v1_topology(&scope, expected.as_ref(), &next)
                    .await
                    .map_err(ControllerError::InvalidAgentEvidence)?;
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

fn next_initialization_command(
    raw: &crate::observation::RawObservation,
    resource_uid: &ResourceUid,
    replica_ids: &[ReplicaId],
) -> Result<Option<InitializeAgentStore>> {
    let policy = EffectivePolicy::fixed(raw.set.spec.replicas, raw.set.spec.failover_delay_seconds)
        .ok_or_else(|| ControllerError::InvalidAgentEvidence("invalid replica policy".into()))?;
    let mut members = Vec::with_capacity(replica_ids.len());
    for replica_id in replica_ids {
        let identity = observed_or_derived_identity(raw, resource_uid, *replica_id)?;
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

fn topology_replicas(
    raw: &crate::observation::RawObservation,
    reports: &[kuberic_runtime::protocol::public_operations::PublicV1AgentReport],
) -> Result<Vec<PublicV1TopologyReplica>> {
    let namespace = raw
        .set
        .namespace()
        .ok_or(ControllerError::ObservationStale)?;
    reports
        .iter()
        .map(|report| {
            let service = raw
                .services
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
            Ok(PublicV1TopologyReplica {
                report: report.clone(),
                replication_address: format!("http://{}.{namespace}.svc:50052", service.name_any()),
            })
        })
        .collect()
}

fn bootstrap_resources_missing(
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
