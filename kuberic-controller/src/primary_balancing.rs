use std::collections::BTreeMap;

use k8s_openapi::api::core::v1::{Node, Pod};
use kube::ResourceExt;
use kuberic_runtime::protocol::observation::{AgentObservation, ObservationSnapshot};
use kuberic_runtime::protocol::types::{
    AccessStatus, ConfigurationDescriptor, ConfigurationMember, PlannedSwitchoverOutcome,
    ReplicaIdentity, ReplicaRole,
};

use crate::crd::{KubericSet, PlannedSwitchoverRequestSpec, PrimaryBalancingMode};
use crate::observation::RawObservation;

const AUTO_BALANCE_PREFIX: &str = "auto-balance";

#[derive(Debug, Clone, PartialEq, Eq)]
struct ReplicaPlacement {
    node: String,
    domain: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PrimaryLoad {
    domains: BTreeMap<String, usize>,
    nodes: BTreeMap<String, usize>,
}

pub fn plan_primary_balance(
    raw: &RawObservation,
    snapshot: &ObservationSnapshot,
) -> Option<PlannedSwitchoverRequestSpec> {
    let policy = raw.set.spec.primary_balancing.as_ref()?;
    match policy.mode {
        PrimaryBalancingMode::Automatic => {}
    }
    if raw.failures.iter().any(|failure| {
        matches!(
            failure.source.as_str(),
            "nodes" | "cluster-sets" | "cluster-pods"
        )
    }) {
        return None;
    }
    if !stable_for_automatic_balance(&raw.set, snapshot) {
        return None;
    }
    if recent_auto_balance(snapshot, policy.cooldown_seconds) {
        return None;
    }
    let topology_key = policy.topology_key.as_str();
    let nodes = nodes_by_name(&raw.nodes);
    let pods = pods_by_uid(raw);
    let load = primary_load(raw, topology_key, &pods, &nodes)?;
    let configuration = &snapshot.status.topology.as_ref()?.configuration;
    let primary = configuration_primary(configuration)?;
    let source = member_placement(&primary.identity, topology_key, &pods, &nodes)?;
    let source_count = *load.domains.get(&source.domain)?;
    let minimum_improvement = i64::from(policy.minimum_improvement);
    let mut candidates = Vec::new();
    for member in configuration
        .members
        .iter()
        .filter(|member| member.role == ReplicaRole::ActiveSecondary)
    {
        let placement = member_placement(&member.identity, topology_key, &pods, &nodes)?;
        if placement.domain == source.domain {
            continue;
        }
        let domain_count = *load.domains.get(&placement.domain).unwrap_or(&0);
        let improvement = source_count as i64 - domain_count as i64 - 1;
        if improvement < minimum_improvement {
            continue;
        }
        let node_count = *load.nodes.get(&placement.node).unwrap_or(&0);
        candidates.push((
            domain_count,
            node_count,
            member.identity.replica_id,
            member.identity.replica_id,
        ));
    }
    candidates.sort_by_key(|(domain_count, node_count, replica_id, _)| {
        (*domain_count, *node_count, replica_id.value())
    });
    let (_, _, _, target) = candidates.into_iter().next()?;
    let target_replica_id = u32::try_from(target.value()).ok()?;
    Some(PlannedSwitchoverRequestSpec {
        request_id: auto_balance_request_id(snapshot, configuration),
        target_replica_id,
    })
}

fn stable_for_automatic_balance(set: &KubericSet, snapshot: &ObservationSnapshot) -> bool {
    let status = &snapshot.status;
    let Some(topology) = &status.topology else {
        return false;
    };
    if !status.initialized
        || status.transition.is_some()
        || status.provisioning.is_some()
        || status.scale_up_allocation.is_some()
        || status.primary_failure.is_some()
        || status.quorum_loss.is_some()
        || status.pending_replacement_cleanup.is_some()
        || status.secondary_scale_down_cleanup.is_some()
        || status.scale_up_cleanup.is_some()
    {
        return false;
    }
    if let Some(request) = set.spec.switchover.as_ref() {
        let Some(receipt) = status.last_switchover.as_ref() else {
            return false;
        };
        if receipt.request_id.as_str() != request.request_id
            || receipt.requested_target_replica_id.value() != i64::from(request.target_replica_id)
            || !matches!(
                receipt.outcome,
                PlannedSwitchoverOutcome::RequestedTargetCompleted
                    | PlannedSwitchoverOutcome::OldPrimaryRestored
                    | PlannedSwitchoverOutcome::OldPrimaryCompensated
                    | PlannedSwitchoverOutcome::Rejected
                    | PlannedSwitchoverOutcome::Unsafe
            )
        {
            return false;
        }
    }
    topology
        .configuration
        .members
        .iter()
        .all(|member| stable_report(snapshot, member, &topology.configuration))
}

fn stable_report(
    snapshot: &ObservationSnapshot,
    member: &ConfigurationMember,
    configuration: &ConfigurationDescriptor,
) -> bool {
    let Some(report) = snapshot
        .observation_for_identity(&member.identity)
        .and_then(|observation| match &observation.agent {
            AgentObservation::Report(report) => Some(report.as_ref()),
            _ => None,
        })
    else {
        return false;
    };
    report.healthy
        && report.identity == member.identity
        && report.role == member.role
        && report.epoch == configuration.epoch
        && report.previous_configuration.is_none()
        && report.current_configuration.as_ref() == Some(configuration)
        && report.pending_operation_id.is_none()
        && report.prepared_switchover.is_none()
        && (member.role != ReplicaRole::Primary || report.write_status == AccessStatus::Granted)
}

fn recent_auto_balance(snapshot: &ObservationSnapshot, cooldown_seconds: u64) -> bool {
    let Some(receipt) = snapshot.status.last_switchover.as_ref() else {
        return false;
    };
    let Some(timestamp) = auto_balance_timestamp(receipt.request_id.as_str()) else {
        return false;
    };
    snapshot.now_unix_seconds < timestamp
        || snapshot.now_unix_seconds - timestamp < cooldown_seconds as i64
}

fn auto_balance_timestamp(request_id: &str) -> Option<i64> {
    let (prefix, timestamp) = request_id.rsplit_once('-')?;
    if !prefix.starts_with(AUTO_BALANCE_PREFIX) {
        return None;
    }
    timestamp.parse().ok()
}

fn auto_balance_request_id(
    snapshot: &ObservationSnapshot,
    configuration: &ConfigurationDescriptor,
) -> String {
    let uid_prefix = snapshot
        .resource_uid
        .as_str()
        .chars()
        .take(8)
        .collect::<String>();
    format!(
        "{AUTO_BALANCE_PREFIX}-{uid_prefix}-{}-{}-{}",
        configuration.epoch.data_loss_number,
        configuration.epoch.configuration_number,
        snapshot.now_unix_seconds
    )
}

fn primary_load(
    raw: &RawObservation,
    topology_key: &str,
    pods: &BTreeMap<String, &Pod>,
    nodes: &BTreeMap<String, &Node>,
) -> Option<PrimaryLoad> {
    let mut sets = raw.cluster_sets.clone();
    if let Some(uid) = raw.set.uid()
        && !sets
            .iter()
            .any(|set| set.uid().as_deref() == Some(uid.as_str()))
    {
        sets.push(raw.set.clone());
    }
    let mut load = PrimaryLoad {
        domains: BTreeMap::new(),
        nodes: BTreeMap::new(),
    };
    for set in sets {
        let Some(configuration) = set
            .status
            .as_ref()
            .and_then(|status| status.authority.topology.as_ref())
            .map(|topology| &topology.configuration)
        else {
            continue;
        };
        let primary = configuration_primary(configuration)?;
        let placement = member_placement(&primary.identity, topology_key, pods, nodes)?;
        *load.domains.entry(placement.domain).or_default() += 1;
        *load.nodes.entry(placement.node).or_default() += 1;
    }
    Some(load)
}

fn nodes_by_name(nodes: &[Node]) -> BTreeMap<String, &Node> {
    nodes
        .iter()
        .filter_map(|node| Some((node.metadata.name.clone()?, node)))
        .collect()
}

fn pods_by_uid(raw: &RawObservation) -> BTreeMap<String, &Pod> {
    raw.cluster_pods
        .iter()
        .chain(raw.pods.iter())
        .filter_map(|pod| Some((pod.uid()?, pod)))
        .collect()
}

fn member_placement(
    identity: &ReplicaIdentity,
    topology_key: &str,
    pods: &BTreeMap<String, &Pod>,
    nodes: &BTreeMap<String, &Node>,
) -> Option<ReplicaPlacement> {
    let pod = pods.get(identity.instance_id.as_str())?;
    let node_name = pod.spec.as_ref()?.node_name.as_ref()?;
    let domain = nodes
        .get(node_name)?
        .metadata
        .labels
        .as_ref()?
        .get(topology_key)?
        .clone();
    if domain.is_empty() {
        return None;
    }
    Some(ReplicaPlacement {
        node: node_name.clone(),
        domain,
    })
}

fn configuration_primary(configuration: &ConfigurationDescriptor) -> Option<&ConfigurationMember> {
    configuration
        .members
        .iter()
        .find(|member| member.identity.replica_id == configuration.primary_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crd::{
        KubericSetSpec, KubericSetStatus, LabelKey, PrimaryBalancingMode, PrimaryBalancingSpec,
    };
    use crate::observation::RawObservation;
    use k8s_openapi::api::core::v1::{NodeSpec, PodSpec};
    use kuberic_runtime::protocol::observation::{
        AgentReport, DesiredState, ReplicaObservation, ReplicaObservationKey, RoutingObservation,
    };
    use kuberic_runtime::protocol::types::{
        AcceptedStatus, AcceptedTopology, AgentGeneration, EffectivePolicy, Epoch,
        PlannedSwitchoverReceipt, ReplicaId, ReplicaInstanceId, ResourceUid, SwitchoverRequestId,
    };

    const UID: &str = "resource-uid-0001";

    fn identity(replica_id: i64) -> ReplicaIdentity {
        identity_with_uid(replica_id, &format!("pod-{replica_id}"))
    }

    fn identity_with_uid(replica_id: i64, pod_uid: &str) -> ReplicaIdentity {
        ReplicaIdentity {
            replica_id: ReplicaId::new(replica_id),
            instance_id: ReplicaInstanceId::new(pod_uid),
            agent_generation: AgentGeneration::new(format!("generation-{replica_id}")),
        }
    }

    fn configuration(primary_id: i64, replica_ids: &[i64]) -> ConfigurationDescriptor {
        let members = replica_ids
            .iter()
            .map(|replica_id| {
                let id = identity(*replica_id);
                ConfigurationMember {
                    role: if *replica_id == primary_id {
                        ReplicaRole::Primary
                    } else {
                        ReplicaRole::ActiveSecondary
                    },
                    identity: id,
                }
            })
            .collect::<Vec<_>>();
        let write_quorum = (replica_ids.len() as u32) / 2 + 1;
        ConfigurationDescriptor::new(
            Epoch::new(0, 1),
            ReplicaId::new(primary_id),
            members,
            write_quorum,
        )
    }

    fn singleton_status(pod_uid: &str) -> AcceptedStatus {
        let identity = identity_with_uid(1, pod_uid);
        let configuration = ConfigurationDescriptor::new(
            Epoch::new(0, 1),
            ReplicaId::new(1),
            vec![ConfigurationMember {
                identity,
                role: ReplicaRole::Primary,
            }],
            1,
        );
        status(configuration)
    }

    fn status(configuration: ConfigurationDescriptor) -> AcceptedStatus {
        AcceptedStatus {
            initialized: true,
            observed_generation: 1,
            effective_policy: EffectivePolicy::fixed(configuration.members.len() as u32, 30),
            topology: Some(AcceptedTopology { configuration }),
            ..Default::default()
        }
    }

    fn set(name: &str, status: AcceptedStatus, balancing: bool) -> KubericSet {
        let mut set = KubericSet::new(
            name,
            KubericSetSpec {
                replicas: 3,
                image: "example/db:latest".to_string(),
                failover_delay_seconds: 30,
                placement: None,
                primary_balancing: balancing.then(|| PrimaryBalancingSpec {
                    mode: PrimaryBalancingMode::Automatic,
                    topology_key: LabelKey::hostname(),
                    cooldown_seconds: 300,
                    minimum_improvement: 1,
                }),
                switchover: None,
            },
        );
        set.metadata.namespace = Some("tests".to_string());
        set.metadata.uid = Some(format!("{name}-uid"));
        set.metadata.resource_version = Some("1".to_string());
        set.metadata.generation = Some(1);
        set.status = Some(KubericSetStatus { authority: status });
        set
    }

    fn pod(replica_id: i64, node: &str) -> Pod {
        Pod {
            metadata: kube::core::ObjectMeta {
                name: Some(format!("db-{replica_id}")),
                uid: Some(format!("pod-{replica_id}")),
                labels: Some(BTreeMap::from([(
                    crate::crd::SET_UID_LABEL.to_string(),
                    UID.to_string(),
                )])),
                ..Default::default()
            },
            spec: Some(PodSpec {
                node_name: Some(node.to_string()),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn pod_with_uid(name: &str, uid: &str, node: &str) -> Pod {
        Pod {
            metadata: kube::core::ObjectMeta {
                name: Some(name.to_string()),
                uid: Some(uid.to_string()),
                ..Default::default()
            },
            spec: Some(PodSpec {
                node_name: Some(node.to_string()),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn node(name: &str, domain: Option<&str>) -> Node {
        Node {
            metadata: kube::core::ObjectMeta {
                name: Some(name.to_string()),
                labels: domain.map(|domain| {
                    BTreeMap::from([(
                        crate::crd::DEFAULT_TOPOLOGY_KEY.to_string(),
                        domain.to_string(),
                    )])
                }),
                ..Default::default()
            },
            spec: Some(NodeSpec::default()),
            ..Default::default()
        }
    }

    fn snapshot(set: &KubericSet, mut reports: BTreeMap<ReplicaId, bool>) -> ObservationSnapshot {
        let status = set.status.as_ref().unwrap().authority.clone();
        let configuration = &status.topology.as_ref().unwrap().configuration;
        let replicas = configuration
            .members
            .iter()
            .map(|member| {
                let healthy = reports.remove(&member.identity.replica_id).unwrap_or(true);
                let report = AgentReport {
                    protocol_version: kuberic_runtime::protocol::PROTOCOL_VERSION,
                    resource_uid: ResourceUid::new(UID),
                    identity: member.identity.clone(),
                    process_session_id: kuberic_runtime::protocol::types::ProcessSessionId::new(
                        format!("session-{}", member.identity.replica_id),
                    ),
                    report_sequence: 1,
                    role: member.role,
                    healthy,
                    epoch: configuration.epoch,
                    current_configuration: Some(configuration.clone()),
                    write_status: if member.role == ReplicaRole::Primary {
                        AccessStatus::Granted
                    } else {
                        AccessStatus::NotPrimary
                    },
                    ..Default::default()
                };
                (
                    ReplicaObservationKey::new(
                        member.identity.replica_id,
                        member.identity.instance_id.clone(),
                    ),
                    ReplicaObservation {
                        kubernetes: None,
                        agent: AgentObservation::Report(Box::new(report)),
                    },
                )
            })
            .collect();
        ObservationSnapshot {
            resource_uid: ResourceUid::new(UID),
            resource_version: "1".to_string(),
            desired: DesiredState {
                generation: 1,
                replicas: set.spec.replicas,
                image: set.spec.image.clone(),
                failover_delay_seconds: set.spec.failover_delay_seconds,
                switchover: None,
            },
            status,
            replicas,
            secondary_scale_down_resources: Vec::new(),
            previous_report_watermarks: BTreeMap::new(),
            durable_storage_evidence: false,
            supporting_resources_ready: true,
            routing: RoutingObservation::default(),
            observation_failures: Vec::new(),
            now_unix_seconds: 1_000,
        }
    }

    fn raw(
        set: KubericSet,
        pods: Vec<Pod>,
        nodes: Vec<Node>,
        mut cluster_sets: Vec<KubericSet>,
        mut cluster_pods: Vec<Pod>,
    ) -> RawObservation {
        if cluster_sets.is_empty() {
            cluster_sets.push(set.clone());
        }
        if cluster_pods.is_empty() {
            cluster_pods = pods.clone();
        }
        RawObservation {
            set,
            pods,
            pvcs: Vec::new(),
            services: Vec::new(),
            secrets: Vec::new(),
            nodes,
            cluster_sets,
            cluster_pods,
            agents: BTreeMap::new(),
            exact_resources: Vec::new(),
            failures: Vec::new(),
            now_unix_seconds: 1_000,
        }
    }

    #[test]
    fn two_to_zero_domain_improvement_moves() {
        let current_status = status(configuration(1, &[1, 2, 3]));
        let current = set("db", current_status, true);
        let other = set("other", singleton_status("other-pod-1"), false);
        let observation = raw(
            current.clone(),
            vec![pod(1, "node-a"), pod(2, "node-b"), pod(3, "node-c")],
            vec![
                node("node-a", Some("a")),
                node("node-b", Some("b")),
                node("node-c", Some("c")),
                node("node-a2", Some("a")),
            ],
            vec![current.clone(), other],
            vec![
                pod(1, "node-a"),
                pod(2, "node-b"),
                pod(3, "node-c"),
                pod_with_uid("other-1", "other-pod-1", "node-a2"),
            ],
        );
        let decision = plan_primary_balance(&observation, &snapshot(&current, BTreeMap::new()))
            .expect("2->0 should move");
        assert_eq!(decision.target_replica_id, 2);
        assert!(decision.request_id.starts_with("auto-balance-"));
    }

    #[test]
    fn one_to_zero_domain_improvement_is_not_enough() {
        let current_status = status(configuration(1, &[1, 2]));
        let current = set("db", current_status, true);
        let observation = raw(
            current.clone(),
            vec![pod(1, "node-a"), pod(2, "node-b")],
            vec![node("node-a", Some("a")), node("node-b", Some("b"))],
            Vec::new(),
            Vec::new(),
        );
        assert!(plan_primary_balance(&observation, &snapshot(&current, BTreeMap::new())).is_none());
    }

    #[test]
    fn cooldown_blocks_recent_auto_receipt() {
        let mut current_status = status(configuration(1, &[1, 2, 3]));
        current_status.last_switchover = Some(PlannedSwitchoverReceipt {
            request_id: SwitchoverRequestId::new("auto-balance-resource-0-1-900"),
            requested_target_replica_id: ReplicaId::new(2),
            accepted_target: Some(identity(2)),
            resulting_primary: Some(identity(2)),
            outcome: PlannedSwitchoverOutcome::RequestedTargetCompleted,
        });
        let current = set("db", current_status, true);
        let other = set("other", singleton_status("other-pod-1"), false);
        let observation = raw(
            current.clone(),
            vec![pod(1, "node-a"), pod(2, "node-b"), pod(3, "node-c")],
            vec![
                node("node-a", Some("a")),
                node("node-b", Some("b")),
                node("node-c", Some("c")),
                node("node-a2", Some("a")),
            ],
            vec![current.clone(), other],
            vec![pod(1, "node-a"), pod(2, "node-b"), pod(3, "node-c")],
        );
        assert!(plan_primary_balance(&observation, &snapshot(&current, BTreeMap::new())).is_none());
    }

    #[test]
    fn active_switchover_or_unhealthy_member_blocks() {
        let mut current = set("db", status(configuration(1, &[1, 2, 3])), true);
        current.spec.switchover = Some(PlannedSwitchoverRequestSpec {
            request_id: "manual".to_string(),
            target_replica_id: 2,
        });
        let observation = raw(
            current.clone(),
            vec![pod(1, "node-a"), pod(2, "node-b"), pod(3, "node-c")],
            vec![
                node("node-a", Some("a")),
                node("node-b", Some("b")),
                node("node-c", Some("c")),
            ],
            Vec::new(),
            Vec::new(),
        );
        assert!(plan_primary_balance(&observation, &snapshot(&current, BTreeMap::new())).is_none());

        let current = set("db", status(configuration(1, &[1, 2])), true);
        let other = set("other", status(configuration(1, &[1])), false);
        let observation = raw(
            current.clone(),
            vec![pod(1, "node-a"), pod(2, "node-b")],
            vec![
                node("node-a", Some("a")),
                node("node-b", Some("b")),
                node("node-c", Some("c")),
                node("node-a2", Some("a")),
            ],
            vec![current.clone(), other],
            Vec::new(),
        );
        assert!(
            plan_primary_balance(
                &observation,
                &snapshot(&current, BTreeMap::from([(ReplicaId::new(2), false)]))
            )
            .is_none()
        );
    }

    #[test]
    fn cross_set_primary_counts_can_remove_improvement() {
        let current = set("db", status(configuration(1, &[1, 2, 3])), true);
        let source_peer = set("source-peer", singleton_status("source-pod-1"), false);
        let target_peer = set("target-peer", singleton_status("target-pod-1"), false);
        let observation = raw(
            current.clone(),
            vec![pod(1, "node-a"), pod(2, "node-b"), pod(3, "node-c")],
            vec![
                node("node-a", Some("a")),
                node("node-b", Some("b")),
                node("node-a2", Some("a")),
                node("node-b2", Some("b")),
            ],
            vec![current.clone(), source_peer, target_peer],
            vec![
                pod(1, "node-a"),
                pod(2, "node-b"),
                pod_with_uid("source-peer-1", "source-pod-1", "node-a2"),
                pod_with_uid("target-peer-1", "target-pod-1", "node-b2"),
            ],
        );
        assert!(plan_primary_balance(&observation, &snapshot(&current, BTreeMap::new())).is_none());
    }

    #[test]
    fn tie_break_uses_lowest_replica_id_after_equal_loads() {
        let current = set("db", status(configuration(1, &[1, 2, 3])), true);
        let source_peer = set("source-peer", singleton_status("source-pod-1"), false);
        let observation = raw(
            current.clone(),
            vec![
                pod(1, "node-a"),
                pod(2, "node-b"),
                pod(3, "node-c"),
                pod_with_uid("source-peer-1", "source-pod-1", "node-a2"),
            ],
            vec![
                node("node-a", Some("a")),
                node("node-b", Some("b")),
                node("node-c", Some("c")),
                node("node-a2", Some("a")),
            ],
            vec![current.clone(), source_peer],
            vec![pod(1, "node-a"), pod(2, "node-b"), pod(3, "node-c")],
        );
        let decision = plan_primary_balance(&observation, &snapshot(&current, BTreeMap::new()))
            .expect("equal target loads should tie-break by replica ID");
        assert_eq!(decision.target_replica_id, 2);
    }

    #[test]
    fn missing_node_topology_label_suppresses_decision() {
        let current = set("db", status(configuration(1, &[1, 2, 3])), true);
        let source_peer = set("source-peer", singleton_status("source-pod-1"), false);
        let observation = raw(
            current.clone(),
            vec![pod(1, "node-a"), pod(2, "node-b"), pod(3, "node-c")],
            vec![
                node("node-a", Some("a")),
                node("node-b", None),
                node("node-c", Some("c")),
                node("node-a2", Some("a")),
            ],
            vec![current.clone(), source_peer],
            Vec::new(),
        );
        assert!(plan_primary_balance(&observation, &snapshot(&current, BTreeMap::new())).is_none());
    }
}
