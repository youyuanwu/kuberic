use std::collections::BTreeMap;

use k8s_openapi::api::core::v1::{
    Affinity, NodeAffinity, NodeSelector, NodeSelectorRequirement, NodeSelectorTerm, Pod,
    PodAffinityTerm, PodAntiAffinity, PodSpec, Toleration, WeightedPodAffinityTerm,
};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::LabelSelector;
use kube::ResourceExt;
use kuberic_runtime::protocol::types::{AcceptedStatus, ConditionStatus, StatusCondition};

use crate::crd::{
    KubericSet, PlacementMode, PlacementSpec, SET_UID_LABEL, TolerationEffect, TolerationOperator,
};
use crate::observation::RawObservation;

const UNSCHEDULABLE_CONDITION: &str = "ReplicaPodUnschedulable";
const REQUIRED_VIOLATION_CONDITION: &str = "RequiredTopologyViolation";
const REQUIRED_UNVERIFIED_CONDITION: &str = "RequiredTopologyUnverified";

pub fn apply_pod_spec_placement(set: &KubericSet, set_uid: &str, spec: &mut PodSpec) {
    let placement = effective_placement(set.spec.placement.as_ref());
    if !placement.node_selector.is_empty() {
        spec.node_selector = Some(
            placement
                .node_selector
                .iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect(),
        );
    }
    if !placement.tolerations.is_empty() {
        spec.tolerations = Some(
            placement
                .tolerations
                .iter()
                .map(|toleration| Toleration {
                    key: toleration.key.clone(),
                    operator: Some(match toleration.operator {
                        TolerationOperator::Exists => "Exists".to_string(),
                        TolerationOperator::Equal => "Equal".to_string(),
                    }),
                    value: toleration.value.clone(),
                    effect: toleration.effect.map(|effect| match effect {
                        TolerationEffect::NoSchedule => "NoSchedule".to_string(),
                        TolerationEffect::PreferNoSchedule => "PreferNoSchedule".to_string(),
                        TolerationEffect::NoExecute => "NoExecute".to_string(),
                    }),
                    ..Default::default()
                })
                .collect(),
        );
    }

    let topology_key = placement.topology_key.as_str();
    let term = self_set_pod_affinity_term(set_uid, topology_key);
    let affinity = spec.affinity.get_or_insert_with(Affinity::default);
    let anti_affinity = affinity
        .pod_anti_affinity
        .get_or_insert_with(PodAntiAffinity::default);
    match placement.mode {
        PlacementMode::Preferred => {
            anti_affinity
                .preferred_during_scheduling_ignored_during_execution
                .get_or_insert_with(Vec::new)
                .push(WeightedPodAffinityTerm {
                    weight: 100,
                    pod_affinity_term: term,
                });
        }
        PlacementMode::Required => {
            anti_affinity
                .required_during_scheduling_ignored_during_execution
                .get_or_insert_with(Vec::new)
                .push(term);
            require_node_topology_label(affinity, topology_key);
        }
    }
}

pub fn project_conditions(raw: &RawObservation, status: AcceptedStatus) -> AcceptedStatus {
    let status = status
        .without_condition(UNSCHEDULABLE_CONDITION)
        .without_condition(REQUIRED_VIOLATION_CONDITION)
        .without_condition(REQUIRED_UNVERIFIED_CONDITION);
    let status = project_unschedulable_condition(raw, status);
    project_required_topology_conditions(raw, status)
}

fn effective_placement(placement: Option<&PlacementSpec>) -> PlacementSpec {
    placement.cloned().unwrap_or_default()
}

fn self_set_pod_affinity_term(set_uid: &str, topology_key: &str) -> PodAffinityTerm {
    PodAffinityTerm {
        label_selector: Some(LabelSelector {
            match_labels: Some(BTreeMap::from([(
                SET_UID_LABEL.to_string(),
                set_uid.to_string(),
            )])),
            ..Default::default()
        }),
        topology_key: topology_key.to_string(),
        ..Default::default()
    }
}

fn require_node_topology_label(affinity: &mut Affinity, topology_key: &str) {
    let requirement = NodeSelectorRequirement {
        key: topology_key.to_string(),
        operator: "Exists".to_string(),
        values: None,
    };
    let node_affinity = affinity
        .node_affinity
        .get_or_insert_with(NodeAffinity::default);
    let Some(required) = node_affinity
        .required_during_scheduling_ignored_during_execution
        .as_mut()
    else {
        node_affinity.required_during_scheduling_ignored_during_execution = Some(NodeSelector {
            node_selector_terms: vec![NodeSelectorTerm {
                match_expressions: Some(vec![requirement]),
                ..Default::default()
            }],
        });
        return;
    };
    for term in &mut required.node_selector_terms {
        term.match_expressions
            .get_or_insert_with(Vec::new)
            .push(requirement.clone());
    }
}

fn project_unschedulable_condition(raw: &RawObservation, status: AcceptedStatus) -> AcceptedStatus {
    let failures = raw
        .pods
        .iter()
        .filter_map(unschedulable_message)
        .collect::<Vec<_>>();
    if failures.is_empty() {
        return status;
    }
    status.with_condition(StatusCondition {
        type_: UNSCHEDULABLE_CONDITION.to_string(),
        status: ConditionStatus::True,
        reason: "PodScheduledUnschedulable".to_string(),
        message: failures.join("; "),
    })
}

fn project_required_topology_conditions(
    raw: &RawObservation,
    status: AcceptedStatus,
) -> AcceptedStatus {
    let placement = effective_placement(raw.set.spec.placement.as_ref());
    if placement.mode != PlacementMode::Required {
        return status;
    }
    let topology_key = placement.topology_key.as_str();
    let nodes = raw
        .nodes
        .iter()
        .filter_map(|node| Some((node.metadata.name.as_deref()?, node)))
        .collect::<BTreeMap<_, _>>();
    let node_inventory_failed = raw.failures.iter().any(|failure| failure.source == "nodes");
    let mut unverified = Vec::new();
    let mut missing = Vec::new();
    let mut by_domain: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for pod in raw
        .pods
        .iter()
        .filter(|pod| pod.metadata.deletion_timestamp.is_none())
    {
        let Some(node_name) = pod.spec.as_ref().and_then(|spec| spec.node_name.as_deref()) else {
            continue;
        };
        let pod_name = pod.name_any();
        if node_inventory_failed {
            unverified.push(format!("{pod_name} on {node_name}"));
            continue;
        }
        let Some(node) = nodes.get(node_name) else {
            unverified.push(format!("{pod_name} on {node_name}"));
            continue;
        };
        let Some(domain) = node
            .metadata
            .labels
            .as_ref()
            .and_then(|labels| labels.get(topology_key))
            .filter(|domain| !domain.is_empty())
        else {
            missing.push(format!("{pod_name} on {node_name}"));
            continue;
        };
        by_domain.entry(domain.clone()).or_default().push(pod_name);
    }
    let mut violations = missing;
    for (domain, pods) in by_domain {
        if pods.len() > 1 {
            violations.push(format!("domain {domain}: {}", pods.join(", ")));
        }
    }
    let mut status = status;
    if !violations.is_empty() {
        status = status.with_condition(StatusCondition {
            type_: REQUIRED_VIOLATION_CONDITION.to_string(),
            status: ConditionStatus::True,
            reason: "RequiredTopologyViolation".to_string(),
            message: format!("{topology_key}: {}", violations.join("; ")),
        });
    }
    if !unverified.is_empty() {
        status = status.with_condition(StatusCondition {
            type_: REQUIRED_UNVERIFIED_CONDITION.to_string(),
            status: ConditionStatus::Unknown,
            reason: if node_inventory_failed {
                "NodeInventoryUnavailable".to_string()
            } else {
                "NodeMissingFromInventory".to_string()
            },
            message: format!("{topology_key}: {}", unverified.join("; ")),
        });
    }
    status
}

fn unschedulable_message(pod: &Pod) -> Option<String> {
    let condition = pod
        .status
        .as_ref()?
        .conditions
        .as_ref()?
        .iter()
        .find(|condition| {
            condition.type_ == "PodScheduled"
                && condition.status == "False"
                && condition.reason.as_deref() == Some("Unschedulable")
        })?;
    let mut message = format!("{} is Unschedulable", pod.name_any());
    if let Some(scheduler_message) = condition.message.as_deref() {
        message.push_str(": ");
        message.push_str(scheduler_message);
    }
    Some(message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crd::{DEFAULT_TOPOLOGY_KEY, KubericSetSpec, PlacementMode, PlacementSpec};
    use k8s_openapi::api::core::v1::{Node, NodeStatus, PodCondition, PodStatus};

    fn set(required: bool) -> KubericSet {
        let mut set = KubericSet::new(
            "db",
            KubericSetSpec {
                replicas: 3,
                image: "example/db:latest".to_string(),
                failover_delay_seconds: 30,
                placement: required.then(|| PlacementSpec {
                    mode: PlacementMode::Required,
                    ..Default::default()
                }),
                primary_balancing: None,
                switchover: None,
            },
        );
        set.metadata.uid = Some("set-uid".to_string());
        set
    }

    fn pod(name: &str, node: Option<&str>) -> Pod {
        Pod {
            metadata: kube::core::ObjectMeta {
                name: Some(name.to_string()),
                labels: Some(BTreeMap::from([(
                    SET_UID_LABEL.to_string(),
                    "set-uid".to_string(),
                )])),
                ..Default::default()
            },
            spec: Some(PodSpec {
                node_name: node.map(str::to_string),
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
                    BTreeMap::from([(DEFAULT_TOPOLOGY_KEY.to_string(), domain.to_string())])
                }),
                ..Default::default()
            },
            status: Some(NodeStatus::default()),
            ..Default::default()
        }
    }

    fn raw(set: KubericSet, pods: Vec<Pod>, nodes: Vec<Node>) -> RawObservation {
        RawObservation {
            set,
            pods,
            pvcs: Vec::new(),
            services: Vec::new(),
            secrets: Vec::new(),
            nodes,
            cluster_sets: Vec::new(),
            cluster_pods: Vec::new(),
            agents: BTreeMap::new(),
            exact_resources: Vec::new(),
            failures: Vec::new(),
            now_unix_seconds: 0,
        }
    }

    #[test]
    fn projects_scheduler_and_required_topology_diagnostics() {
        let mut unschedulable = pod("db-4", None);
        unschedulable.status = Some(PodStatus {
            conditions: Some(vec![PodCondition {
                type_: "PodScheduled".to_string(),
                status: "False".to_string(),
                reason: Some("Unschedulable".to_string()),
                message: Some("insufficient topology domains".to_string()),
                ..Default::default()
            }]),
            ..Default::default()
        });
        let observation = raw(
            set(true),
            vec![
                pod("db-1", Some("node-a")),
                pod("db-2", Some("node-a")),
                pod("db-3", Some("node-missing-label")),
                pod("db-5", Some("node-not-observed")),
                unschedulable,
            ],
            vec![node("node-a", Some("a")), node("node-missing-label", None)],
        );
        let status = project_conditions(&observation, AcceptedStatus::default());
        let condition = |type_: &str| {
            status
                .conditions
                .iter()
                .find(|condition| condition.type_ == type_)
                .expect(type_)
        };
        assert_eq!(
            condition(UNSCHEDULABLE_CONDITION).reason,
            "PodScheduledUnschedulable"
        );
        assert!(
            condition(REQUIRED_VIOLATION_CONDITION)
                .message
                .contains("domain a: db-1, db-2")
        );
        assert!(
            condition(REQUIRED_VIOLATION_CONDITION)
                .message
                .contains("db-3 on node-missing-label")
        );
        assert_eq!(
            condition(REQUIRED_UNVERIFIED_CONDITION).status,
            ConditionStatus::Unknown
        );
    }
}
