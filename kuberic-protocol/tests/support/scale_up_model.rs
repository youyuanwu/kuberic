use std::collections::BTreeMap;

use kuberic_protocol::command::{KubernetesChange, ProtocolCommand, ScaleDownResource};
use kuberic_protocol::evaluator::{EvaluationConfig, evaluate};
use kuberic_protocol::observation::*;
use kuberic_protocol::plan::Plan;
use kuberic_protocol::types::*;

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
}

impl Model {
    pub fn new(accepted: u32, desired: u32) -> Self {
        let resource_uid = ResourceUid::new("scale-up-model");
        let policy = EffectivePolicy::fixed(accepted, 10).unwrap();
        let members = (1..=accepted)
            .map(|id| ConfigurationMember {
                identity: ReplicaIdentity {
                    replica_id: ReplicaId::new(i64::from(id)),
                    instance_id: ReplicaInstanceId::new(format!("accepted-pod-{id}")),
                    agent_generation: AgentGeneration::new(format!("accepted-generation-{id}")),
                },
                role: if id == 1 {
                    ReplicaRole::Primary
                } else {
                    ReplicaRole::ActiveSecondary
                },
            })
            .collect::<Vec<_>>();
        let configuration = ConfigurationDescriptor::new(
            Epoch::new(0, 1),
            ReplicaId::new(1),
            members.clone(),
            policy.write_quorum,
        );
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
        }
    }

    pub fn plan(&self) -> Plan {
        let plan = evaluate(&self.snapshot, &config());
        assert_eq!(plan, evaluate(&self.snapshot, &config()));
        plan
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
        match self.plan() {
            Plan::Stable { status, .. } => {
                self.snapshot.status = status;
                true
            }
            Plan::Wait { status, .. } => {
                self.snapshot.status = status;
                false
            }
            Plan::Unsafe { reason, .. } => panic!(
                "unexpected unsafe plan: {reason:?}; transition={:?}; reports={:?}",
                self.snapshot.status.transition,
                self.snapshot
                    .replicas
                    .values()
                    .filter_map(|observation| match &observation.agent {
                        AgentObservation::Report(report) =>
                            Some((report.identity.replica_id, report.scale_up_intent.clone(),)),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
            ),
            Plan::Apply { changes } => {
                for change in changes {
                    self.apply(change);
                }
                false
            }
            Plan::Execute { command } => {
                self.execute(command);
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
                self.snapshot.status = *status;
                let new = self.accepted_count();
                if new != old {
                    assert_eq!(new, old + 1);
                    self.accepted_history.push(new);
                }
            }
            KubernetesChange::EnsureReplicaScaffolding { replica_ids } => {
                assert_eq!(replica_ids.len(), 1);
                self.add_candidate(replica_ids[0]);
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
                    .unwrap()
                    .target
                    .clone();
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
            }
            other => panic!("unexpected scale-up model change: {other:?}"),
        }
    }

    fn add_candidate(&mut self, replica_id: ReplicaId) {
        if self
            .snapshot
            .replicas
            .keys()
            .any(|key| key.replica_id == replica_id)
        {
            return;
        }
        let pod_uid = PodUid::new(format!("candidate-pod-{replica_id}"));
        let pvc_uid = PvcUid::new(format!("candidate-pvc-{replica_id}"));
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
                pvc: ExactResourceObservation::FrozenUidPresent {
                    resource_version: "1".into(),
                },
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
                let boundary = 10;
                let build = AgentBuildReport {
                    build_id: command.operation_id,
                    target: command.target,
                    last_sequence: 3,
                    replication_boundary_lsn: boundary,
                    durable_lsn: boundary,
                    completed: true,
                    catch_up_boundary_lsn: Some(boundary),
                };
                for key in [source_key, target_key] {
                    let AgentObservation::Report(report) =
                        &mut self.snapshot.replicas.get_mut(&key).unwrap().agent
                    else {
                        panic!("build participant report")
                    };
                    report.builds = vec![build.clone()];
                    if key.replica_id != command.local_replica_id {
                        report.role = ReplicaRole::IdleSecondary;
                        report.current_progress = boundary;
                        report.committed_lsn = boundary;
                    }
                    report.report_sequence += 1;
                }
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
                report.current_progress = 10;
                report.verified_replication_lsn = Some(10);
                report.committed_lsn = 10;
                report.current_configuration_quorum_progress = 10;
                report.catch_up_boundary = Some(10);
                report.catch_up_complete = true;
                report.pending_operation_id = None;
                report.retained_operation_id = Some(command.operation_id);
                report.scale_up_intent = command
                    .scale_up_evidence
                    .map(|evidence| Box::new(evidence.intent().clone()));
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
}
