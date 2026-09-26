use std::collections::{BTreeMap, BTreeSet};

use kuberic_protocol::command::{KubernetesChange, ProtocolCommand, ScaleDownResource};
use kuberic_protocol::evaluator::{EvaluationConfig, evaluate};
use kuberic_protocol::observation::*;
use kuberic_protocol::plan::Plan;
use kuberic_protocol::types::*;
use kuberic_protocol::validation::validate_status;

pub fn config() -> EvaluationConfig {
    EvaluationConfig {
        enable_secondary_scale_down: true,
        allow_scale_up: true,
        ..Default::default()
    }
}

pub struct Model {
    pub snapshot: ObservationSnapshot,
    pub accepted_history: Vec<u32>,
    pending_build: Option<PendingBuild>,
    pub durable_source: BTreeMap<i64, String>,
    pub durable_receivers: BTreeMap<i64, BTreeMap<i64, String>>,
    pub acknowledged_writes: BTreeMap<i64, String>,
    durable_incarnations: BTreeMap<String, BTreeMap<i64, String>>,
    accepted_epochs: Vec<Epoch>,
    frozen_boundaries: BTreeMap<OperationId, i64>,
    committed_incarnations: BTreeSet<String>,
    next_incarnation: u64,
}

struct PendingBuild {
    build: AgentBuildReport,
    source: ReplicaObservationKey,
    target: ReplicaObservationKey,
    catch_up_boundary: i64,
    phase: u8,
}

impl Model {
    pub fn new(accepted: u32, desired: u32) -> Self {
        let resource_uid = ResourceUid::new("scale-up-model");
        let policy = EffectivePolicy::fixed(accepted, 10).unwrap();
        let members = (1..=accepted)
            .map(|id| {
                let replica_id = ReplicaId::new(i64::from(id));
                let pod_uid = PodUid::new(format!("accepted-pod-{id}"));
                let pvc_uid = PvcUid::new(format!("accepted-pvc-{id}"));
                ConfigurationMember {
                    identity: ReplicaIdentity {
                        replica_id,
                        instance_id: ReplicaInstanceId::new(pod_uid.as_str()),
                        agent_generation: derive_agent_generation(&derive_initialization_id(
                            &resource_uid,
                            replica_id,
                            &pod_uid,
                            &pvc_uid,
                        )),
                    },
                    role: if id == 1 {
                        ReplicaRole::Primary
                    } else {
                        ReplicaRole::ActiveSecondary
                    },
                }
            })
            .collect::<Vec<_>>();
        let configuration = ConfigurationDescriptor::new(
            Epoch::new(0, 1),
            ReplicaId::new(1),
            members.clone(),
            policy.write_quorum,
        );
        let accepted_epoch = configuration.epoch;
        let replicas = members
            .iter()
            .map(|member| {
                (
                    ReplicaObservationKey::new(
                        member.identity.replica_id,
                        member.identity.instance_id.clone(),
                    ),
                    ReplicaObservation {
                        kubernetes: Some(KubernetesReplicaObservation {
                            replica_id: member.identity.replica_id,
                            pod_name: member.identity.instance_id.to_string(),
                            pod_uid: Some(PodUid::new(member.identity.instance_id.as_str())),
                            pvc_name: format!("data-{}", member.identity.replica_id),
                            pvc_uid: Some(PvcUid::new(format!(
                                "accepted-pvc-{}",
                                member.identity.replica_id
                            ))),
                            image: Some("example:v1".into()),
                            pod_ready: true,
                            peer_endpoint_ready: true,
                        }),
                        agent: AgentObservation::Report(Box::new(AgentReport {
                            protocol_version: kuberic_protocol::PROTOCOL_VERSION,
                            resource_uid: resource_uid.clone(),
                            identity: member.identity.clone(),
                            process_session_id: ProcessSessionId::new(format!(
                                "accepted-session-{}",
                                member.identity.replica_id
                            )),
                            report_sequence: 1,
                            role: member.role,
                            read_status: AccessStatus::Granted,
                            write_status: if member.role == ReplicaRole::Primary {
                                AccessStatus::Granted
                            } else {
                                AccessStatus::NotPrimary
                            },
                            healthy: true,
                            epoch: configuration.epoch,
                            previous_configuration: None,
                            current_configuration: Some(configuration.clone()),
                            current_progress: 10,
                            verified_replication_lsn: Some(10),
                            committed_lsn: 10,
                            catch_up_capability: Some(1),
                            current_configuration_quorum_progress: 10,
                            catch_up_boundary: None,
                            catch_up_complete: true,
                            ..Default::default()
                        })),
                    },
                )
            })
            .collect();
        let durable_source = (1..=10)
            .map(|lsn| (lsn, format!("value-{lsn}")))
            .collect::<BTreeMap<_, _>>();
        Self {
            snapshot: ObservationSnapshot {
                resource_uid,
                resource_version: "1".into(),
                desired: DesiredState {
                    generation: 2,
                    replicas: desired,
                    image: "example:v1".into(),
                    failover_delay_seconds: 10,
                    switchover: None,
                },
                status: AcceptedStatus {
                    initialized: true,
                    observed_generation: 1,
                    effective_policy: Some(policy),
                    topology: Some(AcceptedTopology { configuration }),
                    ..Default::default()
                },
                replicas,
                secondary_scale_down_resources: Vec::new(),
                previous_report_watermarks: BTreeMap::new(),
                durable_storage_evidence: true,
                supporting_resources_ready: true,
                routing: RoutingObservation {
                    service_present: true,
                    unresolved_write_target: false,
                    write_target: Some(members[0].identity.clone()),
                },
                observation_failures: Vec::new(),
                now_unix_seconds: 100,
            },
            accepted_history: vec![accepted],
            pending_build: None,
            durable_source: durable_source.clone(),
            durable_receivers: BTreeMap::new(),
            acknowledged_writes: durable_source.clone(),
            durable_incarnations: members
                .iter()
                .map(|member| {
                    (
                        member.identity.instance_id.to_string(),
                        durable_source.clone(),
                    )
                })
                .collect(),
            accepted_epochs: vec![accepted_epoch],
            frozen_boundaries: BTreeMap::new(),
            committed_incarnations: members
                .iter()
                .map(|member| member.identity.instance_id.to_string())
                .collect(),
            next_incarnation: 1,
        }
    }

    pub fn plan(&self) -> Plan {
        let plan = evaluate(&self.snapshot, &config());
        assert_eq!(plan, evaluate(&self.snapshot, &config()));
        plan
    }

    pub fn from_snapshot(snapshot: ObservationSnapshot, accepted_history: Vec<u32>) -> Self {
        let configuration = &snapshot.status.topology.as_ref().unwrap().configuration;
        let durable_source = (1..=10)
            .map(|lsn| (lsn, format!("value-{lsn}")))
            .collect::<BTreeMap<_, _>>();
        Self {
            accepted_epochs: vec![configuration.epoch],
            committed_incarnations: configuration
                .members
                .iter()
                .map(|member| member.identity.instance_id.to_string())
                .collect(),
            durable_incarnations: configuration
                .members
                .iter()
                .map(|member| {
                    (
                        member.identity.instance_id.to_string(),
                        durable_source.clone(),
                    )
                })
                .collect(),
            snapshot,
            accepted_history,
            pending_build: None,
            durable_source: durable_source.clone(),
            durable_receivers: BTreeMap::new(),
            acknowledged_writes: durable_source,
            frozen_boundaries: BTreeMap::new(),
            next_incarnation: 1,
        }
    }

    pub fn fork(&self) -> Self {
        Self {
            snapshot: self.snapshot.clone(),
            accepted_history: self.accepted_history.clone(),
            pending_build: self.pending_build.as_ref().map(|pending| PendingBuild {
                build: pending.build.clone(),
                source: pending.source.clone(),
                target: pending.target.clone(),
                catch_up_boundary: pending.catch_up_boundary,
                phase: pending.phase,
            }),
            durable_source: self.durable_source.clone(),
            durable_receivers: self.durable_receivers.clone(),
            acknowledged_writes: self.acknowledged_writes.clone(),
            durable_incarnations: self.durable_incarnations.clone(),
            accepted_epochs: self.accepted_epochs.clone(),
            frozen_boundaries: self.frozen_boundaries.clone(),
            committed_incarnations: self.committed_incarnations.clone(),
            next_incarnation: self.next_incarnation,
        }
    }

    pub fn accepted_count(&self) -> u32 {
        self.snapshot
            .status
            .effective_policy
            .as_ref()
            .unwrap()
            .replica_set_size
    }

    pub fn step(&mut self) -> bool {
        self.assert_safety_invariants();
        match self.plan() {
            Plan::Stable { status, .. } => {
                self.snapshot.status = status;
                self.assert_safety_invariants();
                true
            }
            Plan::Wait { status, .. } => {
                self.snapshot.status = status;
                self.advance_async_build();
                self.assert_safety_invariants();
                false
            }
            Plan::Unsafe { reason, .. } => panic!(
                "unexpected unsafe plan: {reason:?}; transition={:?}; reports={:?}",
                self.snapshot.status.transition,
                self.snapshot
                    .replicas
                    .values()
                    .filter_map(|observation| match &observation.agent {
                        AgentObservation::Report(report) => Some((
                            report.identity.replica_id,
                            report.role,
                            report.epoch,
                            report
                                .current_configuration
                                .as_ref()
                                .map(|configuration| configuration.epoch),
                            report.current_progress,
                            report.verified_replication_lsn,
                            report.scale_up_intent.clone(),
                        )),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
            ),
            Plan::Apply { changes } => {
                for change in changes {
                    self.apply(change);
                }
                self.assert_safety_invariants();
                false
            }
            Plan::Execute { command } => {
                self.execute(command);
                self.assert_safety_invariants();
                false
            }
        }
    }

    pub fn run(&mut self, limit: usize) {
        for _ in 0..limit {
            if self.step() && self.accepted_count() == self.snapshot.desired.replicas {
                return;
            }
        }
        panic!("scale-up model did not converge: {:?}", self.plan());
    }

    pub fn apply(&mut self, change: KubernetesChange) {
        match change {
            KubernetesChange::PersistStatus { status } => {
                let old = self.accepted_count();
                let old_epoch = self
                    .snapshot
                    .status
                    .topology
                    .as_ref()
                    .unwrap()
                    .configuration
                    .epoch;
                self.snapshot.status = *status;
                self.refresh_allocation_observation();
                let new = self.accepted_count();
                if new != old {
                    assert_eq!(new, old + 1);
                    self.accepted_history.push(new);
                    let accepted = &self
                        .snapshot
                        .status
                        .topology
                        .as_ref()
                        .unwrap()
                        .configuration;
                    assert!(accepted.epoch > old_epoch);
                    self.accepted_epochs.push(accepted.epoch);
                    self.committed_incarnations.extend(
                        accepted
                            .members
                            .iter()
                            .map(|member| member.identity.instance_id.to_string()),
                    );
                }
            }
            KubernetesChange::EnsureReplicaScaffolding { replica_ids } => {
                assert_eq!(replica_ids.len(), 1);
                self.add_candidate(replica_ids[0]);
                self.refresh_allocation_observation();
            }
            KubernetesChange::EnsureWriteRoutingService => {
                self.snapshot.routing.service_present = true;
            }
            KubernetesChange::PublishWriteRouting { primary } => {
                self.snapshot.routing.write_target = Some(primary);
            }
            KubernetesChange::RemoveWriteRouting => {
                self.snapshot.routing.write_target = None;
            }
            KubernetesChange::DeleteScaleDownResource { resource, .. } => {
                let target = self
                    .snapshot
                    .status
                    .scale_up_cleanup
                    .as_ref()
                    .map(|cleanup| cleanup.target.clone())
                    .or_else(|| {
                        self.snapshot
                            .status
                            .scale_up_allocation
                            .as_ref()
                            .map(ScaleUpAllocation::observation_target)
                    })
                    .unwrap();
                let exact = self
                    .snapshot
                    .secondary_scale_down_resources
                    .iter_mut()
                    .find(|observation| observation.target == target)
                    .unwrap();
                match resource {
                    ScaleDownResource::Endpoint => {
                        exact.endpoint = ExactResourceObservation::NotFound
                    }
                    ScaleDownResource::Pod => exact.pod = ExactResourceObservation::NotFound,
                    ScaleDownResource::Pvc => exact.pvc = ExactResourceObservation::NotFound,
                }
                let fully_absent = matches!(exact.endpoint, ExactResourceObservation::NotFound)
                    && matches!(exact.pod, ExactResourceObservation::NotFound)
                    && matches!(exact.pvc, ExactResourceObservation::NotFound);
                if fully_absent {
                    self.snapshot.replicas.remove(&ReplicaObservationKey::new(
                        target.replica_id,
                        target.instance_id.clone(),
                    ));
                    self.durable_incarnations
                        .remove(target.instance_id.as_str());
                }
            }
            other => panic!("unexpected scale-up model change: {other:?}"),
        }
    }

    fn refresh_allocation_observation(&mut self) {
        self.snapshot
            .secondary_scale_down_resources
            .retain(|exact| {
                !exact
                    .target
                    .instance_id
                    .as_str()
                    .starts_with("scale-up-allocation-")
            });
        let Some(allocation) = self.snapshot.status.scale_up_allocation.as_ref() else {
            return;
        };
        let target = allocation.observation_target();
        let replica_id = allocation.target_replica_id;
        let kubernetes = self
            .snapshot
            .replicas
            .iter()
            .find(|(key, observation)| {
                key.replica_id == replica_id && observation.kubernetes.is_some()
            })
            .and_then(|(_, observation)| observation.kubernetes.as_ref());
        let pod_name = format!("candidate-pod-{replica_id}");
        let pvc_name = format!("candidate-data-{replica_id}");
        let (pod_identity, pod) = match (&allocation.pod_uid, kubernetes) {
            (Some(uid), Some(observed)) if observed.pod_uid.as_ref() == Some(uid) => (
                CleanupResourceIdentity::Present {
                    name: pod_name,
                    uid: uid.to_string(),
                },
                ExactResourceObservation::FrozenUidPresent {
                    resource_version: "1".into(),
                },
            ),
            (None, Some(observed)) if observed.pod_uid.is_some() => (
                CleanupResourceIdentity::Absent { name: pod_name },
                ExactResourceObservation::ReplacementPresent {
                    uid: observed.pod_uid.as_ref().unwrap().to_string(),
                    resource_version: "1".into(),
                },
            ),
            _ => (
                CleanupResourceIdentity::Absent { name: pod_name },
                ExactResourceObservation::NotFound,
            ),
        };
        let (pvc_identity, pvc) = match (&allocation.pvc_uid, kubernetes) {
            (Some(uid), Some(observed)) if observed.pvc_uid.as_ref() == Some(uid) => (
                CleanupResourceIdentity::Present {
                    name: pvc_name,
                    uid: uid.to_string(),
                },
                ExactResourceObservation::FrozenUidPresent {
                    resource_version: "1".into(),
                },
            ),
            (None, Some(observed)) if observed.pvc_uid.is_some() => (
                CleanupResourceIdentity::Absent { name: pvc_name },
                ExactResourceObservation::ReplacementPresent {
                    uid: observed.pvc_uid.as_ref().unwrap().to_string(),
                    resource_version: "1".into(),
                },
            ),
            _ => (
                CleanupResourceIdentity::Absent { name: pvc_name },
                ExactResourceObservation::NotFound,
            ),
        };
        self.snapshot
            .secondary_scale_down_resources
            .push(SecondaryScaleDownResourceObservation {
                resource_uid: self.snapshot.resource_uid.clone(),
                target,
                identity: ReplicaCleanupIdentity {
                    pod: pod_identity,
                    pvc: pvc_identity,
                    endpoint: CleanupResourceIdentity::Absent {
                        name: format!("candidate-allocation-{replica_id}"),
                    },
                },
                pod,
                pod_allocation_operation_id: Some(allocation.operation_id.clone()),
                pod_matches_allocation_metadata: true,
                pvc,
                pvc_allocation_operation_id: Some(allocation.operation_id.clone()),
                endpoint: ExactResourceObservation::NotFound,
            });
    }

    fn add_candidate(&mut self, replica_id: ReplicaId) {
        if self.snapshot.replicas.iter().any(|(key, observation)| {
            key.replica_id == replica_id
                && observation
                    .kubernetes
                    .as_ref()
                    .is_some_and(KubernetesReplicaObservation::has_exact_scaffolding)
        }) {
            return;
        }
        let incarnation = self.next_incarnation;
        self.next_incarnation += 1;
        let pod_uid = PodUid::new(if incarnation == 1 {
            format!("candidate-pod-{replica_id}")
        } else {
            format!("candidate-pod-{replica_id}-{incarnation}")
        });
        let pvc_uid = PvcUid::new(if incarnation == 1 {
            format!("candidate-pvc-{replica_id}")
        } else {
            format!("candidate-pvc-{replica_id}-{incarnation}")
        });
        let instance_id = ReplicaInstanceId::new(pod_uid.as_str());
        self.snapshot.replicas.insert(
            ReplicaObservationKey::new(replica_id, instance_id.clone()),
            ReplicaObservation {
                kubernetes: Some(KubernetesReplicaObservation {
                    replica_id,
                    pod_name: instance_id.to_string(),
                    pod_uid: Some(pod_uid.clone()),
                    pvc_name: format!("candidate-data-{replica_id}"),
                    pvc_uid: Some(pvc_uid.clone()),
                    image: Some("example:v1".into()),
                    pod_ready: true,
                    peer_endpoint_ready: false,
                }),
                agent: AgentObservation::Uninitialized(UninitializedAgentObservation {
                    protocol_version: kuberic_protocol::PROTOCOL_VERSION,
                    resource_uid: self.snapshot.resource_uid.clone(),
                    replica_id,
                    pod_uid: pod_uid.clone(),
                    pvc_uid: pvc_uid.clone(),
                    process_session_id: ProcessSessionId::new(format!(
                        "candidate-session-{replica_id}"
                    )),
                    report_sequence: 1,
                }),
            },
        );
        let target = ReplicaIdentity {
            replica_id,
            instance_id,
            agent_generation: derive_agent_generation(&derive_initialization_id(
                &self.snapshot.resource_uid,
                replica_id,
                &pod_uid,
                &pvc_uid,
            )),
        };
        self.snapshot
            .secondary_scale_down_resources
            .push(SecondaryScaleDownResourceObservation {
                resource_uid: self.snapshot.resource_uid.clone(),
                target: target.clone(),
                identity: ReplicaCleanupIdentity {
                    pod: CleanupResourceIdentity::Present {
                        name: target.instance_id.to_string(),
                        uid: pod_uid.to_string(),
                    },
                    pvc: CleanupResourceIdentity::Present {
                        name: format!("candidate-data-{replica_id}"),
                        uid: pvc_uid.to_string(),
                    },
                    endpoint: CleanupResourceIdentity::Present {
                        name: derive_replica_endpoint_name(&self.snapshot.resource_uid, &target),
                        uid: format!("candidate-endpoint-{replica_id}"),
                    },
                },
                pod: ExactResourceObservation::FrozenUidPresent {
                    resource_version: "1".into(),
                },
                pod_allocation_operation_id: None,
                pod_matches_allocation_metadata: false,
                pvc: ExactResourceObservation::FrozenUidPresent {
                    resource_version: "1".into(),
                },
                pvc_allocation_operation_id: None,
                endpoint: ExactResourceObservation::FrozenUidPresent {
                    resource_version: "1".into(),
                },
            });
    }

    pub fn execute(&mut self, command: ProtocolCommand) {
        match command {
            ProtocolCommand::InitializeAgentStore(command) => {
                let target = ReplicaIdentity {
                    replica_id: command.local_replica_id,
                    instance_id: command.expected_instance_id.clone(),
                    agent_generation: command.assigned_agent_generation.clone(),
                };
                let observation = self
                    .snapshot
                    .replicas
                    .get_mut(&ReplicaObservationKey::new(
                        target.replica_id,
                        target.instance_id.clone(),
                    ))
                    .unwrap();
                observation.kubernetes.as_mut().unwrap().peer_endpoint_ready = true;
                observation.agent = AgentObservation::Report(Box::new(AgentReport {
                    protocol_version: kuberic_protocol::PROTOCOL_VERSION,
                    resource_uid: self.snapshot.resource_uid.clone(),
                    identity: target,
                    process_session_id: ProcessSessionId::new("candidate-initialized"),
                    report_sequence: 2,
                    role: ReplicaRole::None,
                    read_status: AccessStatus::NotPrimary,
                    write_status: AccessStatus::NotPrimary,
                    healthy: true,
                    epoch: Epoch::default(),
                    current_progress: 0,
                    committed_lsn: 0,
                    ..Default::default()
                }));
            }
            ProtocolCommand::EnsureReplicaBuild(command) => {
                if self.pending_build.is_some() {
                    self.advance_async_build();
                    return;
                }
                let source_key = self
                    .snapshot
                    .replicas
                    .iter()
                    .find_map(|(key, observation)| match &observation.agent {
                        AgentObservation::Report(report)
                            if report.identity.replica_id == command.local_replica_id =>
                        {
                            Some(key.clone())
                        }
                        _ => None,
                    })
                    .unwrap();
                let target_key = ReplicaObservationKey::new(
                    command.target.replica_id,
                    command.target.instance_id.clone(),
                );
                let snapshot_boundary = match &self.snapshot.replicas[&source_key].agent {
                    AgentObservation::Report(report) => report.current_progress,
                    _ => unreachable!(),
                };
                let catch_up_boundary = snapshot_boundary + 2;
                let build = AgentBuildReport {
                    build_id: command.operation_id,
                    target: command.target,
                    last_sequence: 0,
                    replication_boundary_lsn: snapshot_boundary,
                    durable_lsn: snapshot_boundary,
                    completed: false,
                    catch_up_boundary_lsn: None,
                };
                let AgentObservation::Report(source) =
                    &mut self.snapshot.replicas.get_mut(&source_key).unwrap().agent
                else {
                    panic!("build source report")
                };
                source.builds = vec![build.clone()];
                source.report_sequence += 1;
                self.pending_build = Some(PendingBuild {
                    build,
                    source: source_key,
                    target: target_key,
                    catch_up_boundary,
                    phase: 0,
                });
            }
            ProtocolCommand::EnsureConfiguration(command) => {
                let identity = ReplicaIdentity {
                    replica_id: command.local_replica_id,
                    instance_id: command.expected_instance_id.clone(),
                    agent_generation: command.expected_agent_generation.clone(),
                };
                let observation = self
                    .snapshot
                    .replicas
                    .get_mut(&ReplicaObservationKey::new(
                        identity.replica_id,
                        identity.instance_id.clone(),
                    ))
                    .unwrap();
                let AgentObservation::Report(report) = &mut observation.agent else {
                    panic!("configuration target report")
                };
                let member = command
                    .current_configuration
                    .members
                    .iter()
                    .find(|member| member.identity == identity)
                    .unwrap();
                if let Some(evidence) = command.scale_up_evidence.as_deref()
                    && identity == evidence.intent().target
                {
                    let copied = self
                        .durable_receivers
                        .get(&identity.replica_id.value())
                        .expect("candidate durable copied state");
                    for lsn in 1..=evidence.intent().catch_up_boundary_lsn {
                        assert_eq!(copied.get(&lsn), self.durable_source.get(&lsn));
                    }
                }
                if command.scale_up_evidence.is_some() {
                    for member in &command.current_configuration.members {
                        self.durable_incarnations
                            .entry(member.identity.instance_id.to_string())
                            .or_default()
                            .extend(self.durable_source.clone());
                    }
                }
                report.role = member.role;
                report.read_status = AccessStatus::Granted;
                report.write_status = if member.role == ReplicaRole::Primary {
                    command.primary_write_status
                } else {
                    AccessStatus::NotPrimary
                };
                report.epoch = command.current_epoch;
                report.previous_configuration = command.previous_configuration.clone();
                report.current_configuration = Some(command.current_configuration.clone());
                let boundary = command
                    .scale_up_evidence
                    .as_deref()
                    .map_or(report.current_progress, |evidence| {
                        evidence.intent().catch_up_boundary_lsn
                    });
                report.current_progress = report.current_progress.max(boundary);
                report.verified_replication_lsn = Some(boundary);
                report.committed_lsn = report.committed_lsn.max(boundary);
                report.current_configuration_quorum_progress = boundary;
                report.catch_up_boundary = Some(boundary);
                report.catch_up_complete = true;
                if command.transition_kind == TransitionKind::Failover {
                    report.deactivation_epoch = Some(command.current_epoch);
                    report.deactivated_lsn = Some(boundary);
                }
                report.pending_operation_id = None;
                report.retained_operation_id = Some(command.operation_id);
                report.scale_up_intent = command
                    .scale_up_evidence
                    .map(|evidence| Box::new(evidence.intent().clone()));
                if report.scale_up_intent.is_some() {
                    report.prepared_secondary_removal = None;
                    report.secondary_removal_evidence = None;
                    report.retired_replica = None;
                    report.accepted_secondary_removal = None;
                }
                report.report_sequence += 1;
            }
            other => panic!("unexpected scale-up command: {other:?}"),
        }
    }

    pub fn report_mut(&mut self, replica_id: i64) -> &mut AgentReport {
        self.snapshot
            .replicas
            .values_mut()
            .find_map(|observation| match &mut observation.agent {
                AgentObservation::Report(report)
                    if report.identity.replica_id == ReplicaId::new(replica_id) =>
                {
                    Some(report.as_mut())
                }
                _ => None,
            })
            .unwrap()
    }

    pub fn restart(&mut self, replica_id: i64) {
        let bytes = serde_json::to_vec(&(
            self.durable_source.clone(),
            self.durable_receivers.clone(),
            self.acknowledged_writes.clone(),
            self.durable_incarnations.clone(),
        ))
        .unwrap();
        let (source, receivers, acknowledged, incarnations) =
            serde_json::from_slice(&bytes).unwrap();
        self.durable_source = source;
        self.durable_receivers = receivers;
        self.acknowledged_writes = acknowledged;
        self.durable_incarnations = incarnations;
        let report = self.report_mut(replica_id);
        report.process_session_id =
            ProcessSessionId::new(format!("{}-restart", report.process_session_id));
        report.report_sequence = 1;
    }

    pub fn apply_wait(&mut self, status: AcceptedStatus) {
        self.snapshot.status = status;
        self.advance_async_build();
    }

    fn advance_async_build(&mut self) {
        let Some(pending) = self.pending_build.as_mut() else {
            return;
        };
        match pending.phase {
            0 => {
                let AgentObservation::Report(source) = &mut self
                    .snapshot
                    .replicas
                    .get_mut(&pending.source)
                    .unwrap()
                    .agent
                else {
                    panic!("build source report")
                };
                source.current_progress += 1;
                source.committed_lsn = source.current_progress;
                self.durable_source.insert(
                    source.current_progress,
                    format!("value-{}", source.current_progress),
                );
                source.report_sequence += 1;
                let AgentObservation::Report(target) = &mut self
                    .snapshot
                    .replicas
                    .get_mut(&pending.target)
                    .unwrap()
                    .agent
                else {
                    panic!("build target report")
                };
                target.role = ReplicaRole::IdleSecondary;
                target.builds = vec![pending.build.clone()];
                target.report_sequence += 1;
                pending.phase = 1;
            }
            1 => {
                let AgentObservation::Report(source) = &mut self
                    .snapshot
                    .replicas
                    .get_mut(&pending.source)
                    .unwrap()
                    .agent
                else {
                    panic!("build source report")
                };
                source.current_progress = source.current_progress.max(pending.catch_up_boundary);
                source.committed_lsn = source.committed_lsn.max(pending.catch_up_boundary);
                self.durable_source
                    .entry(pending.catch_up_boundary)
                    .or_insert_with(|| format!("value-{}", pending.catch_up_boundary));
                source.builds[0].last_sequence = 2;
                source.builds[0].durable_lsn = pending.catch_up_boundary;
                source.builds[0].completed = true;
                source.builds[0].catch_up_boundary_lsn = Some(pending.catch_up_boundary);
                source.report_sequence += 1;
                pending.phase = 2;
            }
            2 => {
                let AgentObservation::Report(target) = &mut self
                    .snapshot
                    .replicas
                    .get_mut(&pending.target)
                    .unwrap()
                    .agent
                else {
                    panic!("build target report")
                };
                target.current_progress = pending.catch_up_boundary;
                target.committed_lsn = pending.catch_up_boundary;
                target.builds[0].last_sequence = 2;
                target.builds[0].durable_lsn = pending.catch_up_boundary;
                target.builds[0].completed = true;
                target.builds[0].catch_up_boundary_lsn = Some(pending.catch_up_boundary);
                target.report_sequence += 1;
                self.durable_receivers.insert(
                    pending.target.replica_id.value(),
                    self.durable_source
                        .range(..=pending.catch_up_boundary)
                        .map(|(lsn, value)| (*lsn, value.clone()))
                        .collect(),
                );
                self.durable_incarnations.insert(
                    pending.target.instance_id.to_string(),
                    self.durable_source
                        .range(..=pending.catch_up_boundary)
                        .map(|(lsn, value)| (*lsn, value.clone()))
                        .collect(),
                );
                self.pending_build = None;
            }
            _ => unreachable!(),
        }
    }

    pub fn remove_candidate_report(&mut self) {
        let target = self
            .snapshot
            .status
            .transition
            .as_ref()
            .and_then(|transition| transition.scale_up.as_deref())
            .map(|intent| intent.target.clone())
            .or_else(|| {
                self.snapshot
                    .status
                    .last_scale_up
                    .as_deref()
                    .map(|receipt| receipt.intent.target.clone())
            })
            .unwrap();
        self.snapshot
            .replicas
            .get_mut(&ReplicaObservationKey::new(
                target.replica_id,
                target.instance_id,
            ))
            .unwrap()
            .agent = AgentObservation::Absent;
    }

    pub fn acknowledge_write(&mut self, case: u64, step: u64) -> i64 {
        let primary = self
            .snapshot
            .status
            .topology
            .as_ref()
            .unwrap()
            .configuration
            .members
            .iter()
            .find(|member| member.role == ReplicaRole::Primary)
            .unwrap()
            .identity
            .clone();
        let writable = self.snapshot.replicas.values().any(|observation| {
            matches!(
                &observation.agent,
                AgentObservation::Report(report)
                    if report.identity == primary
                        && report.write_status == AccessStatus::Granted
            )
        });
        assert!(
            writable,
            "model may acknowledge only through the accepted writer"
        );
        let lsn = self.durable_source.keys().next_back().copied().unwrap_or(0) + 1;
        let value = format!("ack-{case}-{step}-{lsn}");
        self.durable_source.insert(lsn, value.clone());
        self.acknowledged_writes.insert(lsn, value.clone());

        let authority = self.snapshot.status.transition.as_ref().map_or_else(
            || {
                self.snapshot
                    .status
                    .topology
                    .as_ref()
                    .unwrap()
                    .configuration
                    .clone()
            },
            |transition| transition.current_configuration.clone(),
        );
        for member in &authority.members {
            self.durable_incarnations
                .entry(member.identity.instance_id.to_string())
                .or_default()
                .insert(lsn, value.clone());
            if let Some(report) = self.snapshot.replicas.values_mut().find_map(|observation| {
                match &mut observation.agent {
                    AgentObservation::Report(report)
                        if report.identity == member.identity
                            && matches!(
                                report.role,
                                ReplicaRole::Primary | ReplicaRole::ActiveSecondary
                            ) =>
                    {
                        Some(report.as_mut())
                    }
                    _ => None,
                }
            }) {
                report.current_progress = report.current_progress.max(lsn);
                report.committed_lsn = report.committed_lsn.max(lsn);
                if report.current_configuration.as_ref() == Some(&authority) {
                    report.verified_replication_lsn =
                        Some(report.verified_replication_lsn.unwrap_or_default().max(lsn));
                    report.current_configuration_quorum_progress =
                        report.current_configuration_quorum_progress.max(lsn);
                }
                report.report_sequence += 1;
            }
        }
        lsn
    }

    pub fn assert_acknowledged_write_oracle(&self) {
        let status = &self.snapshot.status;
        let configuration = &status.topology.as_ref().unwrap().configuration;
        let policy = status.effective_policy.as_ref().unwrap();
        for (lsn, value) in &self.acknowledged_writes {
            let recoverable = configuration
                .members
                .iter()
                .filter(|member| {
                    self.durable_incarnations
                        .get(member.identity.instance_id.as_str())
                        .and_then(|history| history.get(lsn))
                        == Some(value)
                })
                .count();
            assert!(
                recoverable >= policy.read_quorum as usize,
                "acknowledged write {lsn}={value:?} is present on only {recoverable} \
                 accepted members; read quorum is {}",
                policy.read_quorum
            );
        }
        assert!(
            configuration.members.len() < usize::BITS as usize,
            "model recovery-mask width exceeded"
        );
        for mask in 0_usize..(1_usize << configuration.members.len()) {
            if mask.count_ones() < policy.read_quorum {
                continue;
            }
            for (lsn, value) in &self.acknowledged_writes {
                assert!(
                    configuration
                        .members
                        .iter()
                        .enumerate()
                        .filter(|(index, _)| mask & (1 << index) != 0)
                        .any(|(_, member)| {
                            self.durable_incarnations
                                .get(member.identity.instance_id.as_str())
                                .and_then(|history| history.get(lsn))
                                == Some(value)
                        }),
                    "valid terminal recovery mask {mask:#b} lost acknowledged \
                     write {lsn}={value:?}"
                );
            }
        }
    }

    pub fn assert_safety_invariants(&mut self) {
        validate_status(&self.snapshot.status).unwrap();
        let accepted = &self
            .snapshot
            .status
            .topology
            .as_ref()
            .unwrap()
            .configuration;
        if let Some(previous) = self.accepted_epochs.last() {
            assert!(
                accepted.epoch >= *previous,
                "accepted epoch regressed from {previous:?} to {:?}",
                accepted.epoch
            );
        }
        let policy = self.snapshot.status.effective_policy.as_ref().unwrap();
        assert_eq!(
            accepted.members.len(),
            policy.replica_set_size as usize,
            "accepted topology and policy cardinality diverged"
        );

        let mut active_targets = BTreeSet::new();
        if let Some(allocation) = &self.snapshot.status.scale_up_allocation {
            active_targets.insert(allocation.target_replica_id);
        }
        if let Some(provisioning) = &self.snapshot.status.provisioning
            && provisioning.scale_up().is_some()
        {
            active_targets.insert(provisioning.replica_id());
        }
        if let Some(transition) = &self.snapshot.status.transition
            && let Some(intent) = transition.scale_up.as_deref().or_else(|| {
                transition
                    .scale_up_failover
                    .as_deref()
                    .map(|evidence| &evidence.intent)
            })
        {
            active_targets.insert(intent.target.replica_id);
        }
        if let Some(cleanup) = &self.snapshot.status.scale_up_cleanup {
            active_targets.insert(cleanup.target.replica_id);
            assert!(
                !accepted
                    .members
                    .iter()
                    .any(|member| member.identity == cleanup.target),
                "committed member acquired pre-admission cleanup authority"
            );
            assert!(
                !self
                    .committed_incarnations
                    .contains(cleanup.target.instance_id.as_str()),
                "historically committed incarnation acquired candidate cleanup authority"
            );
        }
        assert!(
            active_targets.len() <= 1,
            "more than one scale-up candidate is active: {active_targets:?}"
        );

        for observation in self.snapshot.replicas.values() {
            let AgentObservation::Report(report) = &observation.agent else {
                continue;
            };
            for build in &report.builds {
                if let Some(boundary) = build.catch_up_boundary_lsn {
                    match self.frozen_boundaries.get(&build.build_id) {
                        Some(existing) => assert_eq!(
                            *existing, boundary,
                            "catch-up boundary mutated for {}",
                            build.build_id
                        ),
                        None => {
                            self.frozen_boundaries
                                .insert(build.build_id.clone(), boundary);
                        }
                    }
                }
            }
            if !accepted
                .members
                .iter()
                .any(|member| member.identity == report.identity)
            {
                assert_ne!(
                    report.write_status,
                    AccessStatus::Granted,
                    "unadmitted candidate received write-quorum authority"
                );
            }
        }
    }
}
