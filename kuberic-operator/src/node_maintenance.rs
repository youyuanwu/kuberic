use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::crd::StatusCondition;

pub const PREPARED_CONDITION_TYPE: &str = "KubericPrepared";

#[derive(CustomResource, Serialize, Deserialize, Debug, PartialEq, Clone, JsonSchema)]
#[kube(
    group = "kuberic.io",
    version = "v1alpha1",
    kind = "NodeMaintenanceRequest",
    plural = "nodemaintenancerequests",
    shortname = "nmr",
    derive = "PartialEq",
    status = "NodeMaintenanceRequestStatus",
    printcolumn = r#"{"name":"Node","type":"string","jsonPath":".spec.nodeName"}"#,
    printcolumn = r#"{"name":"Operation","type":"string","jsonPath":".spec.operation"}"#,
    printcolumn = r#"{"name":"Phase","type":"string","jsonPath":".status.phase"}"#,
    printcolumn = r#"{"name":"Deadline","type":"string","jsonPath":".spec.deadline"}"#,
    printcolumn = r#"{"name":"Age","type":"date","jsonPath":".metadata.creationTimestamp"}"#
)]
#[serde(rename_all = "camelCase")]
pub struct NodeMaintenanceRequestSpec {
    pub node_name: String,

    #[serde(default)]
    pub operation: MaintenanceOperation,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_event_id: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub not_before: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct NodeMaintenanceRequestStatus {
    #[serde(default)]
    pub phase: MaintenancePhase,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub observed_generation: Option<i64>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub node_uid: Option<String>,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub affected_sets: Vec<AffectedKubericSetStatus>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub blocked_reason: Option<MaintenanceBlockedReason>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub prepared_at: Option<String>,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conditions: Vec<StatusCondition>,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AffectedKubericSetStatus {
    pub namespace: String,

    pub name: String,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub replicas: Vec<String>,

    #[serde(default)]
    pub hosts_primary: bool,

    #[serde(default)]
    pub primary_moved: bool,

    #[serde(default)]
    pub quorum_without_node: bool,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone, Copy, JsonSchema, Default)]
pub enum MaintenanceOperation {
    #[default]
    Reboot,
    Reimage,
    OsUpgrade,
    Replace,
    Shutdown,
}

impl MaintenanceOperation {
    pub fn discards_local_state(self) -> bool {
        matches!(self, Self::Reimage | Self::Replace)
    }
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone, Copy, JsonSchema, Default)]
pub enum MaintenancePhase {
    #[default]
    Requested,
    Preparing,
    Prepared,
    Draining,
    Executing,
    Restoring,
    Completed,
    Blocked,
    Failed,
    Expired,
}

impl MaintenancePhase {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Expired)
    }

    pub fn is_safe_to_drain(self) -> bool {
        matches!(self, Self::Prepared | Self::Draining | Self::Executing)
    }

    pub fn requires_reason(self) -> bool {
        matches!(self, Self::Blocked | Self::Failed | Self::Expired)
    }

    pub fn can_transition_to(self, next: Self) -> bool {
        if self == next {
            return !self.is_terminal();
        }
        match self {
            Self::Requested => matches!(
                next,
                Self::Preparing | Self::Blocked | Self::Failed | Self::Expired
            ),
            Self::Preparing => matches!(
                next,
                Self::Prepared | Self::Blocked | Self::Failed | Self::Expired
            ),
            Self::Prepared => matches!(
                next,
                Self::Draining | Self::Blocked | Self::Failed | Self::Expired
            ),
            Self::Draining => matches!(next, Self::Executing | Self::Failed | Self::Expired),
            Self::Executing => matches!(next, Self::Restoring | Self::Failed),
            Self::Restoring => matches!(next, Self::Completed | Self::Failed),
            Self::Blocked => matches!(next, Self::Preparing | Self::Failed | Self::Expired),
            Self::Completed | Self::Failed | Self::Expired => false,
        }
    }
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone, Copy, JsonSchema)]
pub enum MaintenanceBlockedReason {
    BlockedByQuorum,
    NoEligibleTarget,
    SwitchoverFailed,
    DeadlineExceeded,
    NodeNotFound,
    NodeIncarnationChanged,
    ConflictingOperation,
    ApplicationCloseIncomplete,
}

#[derive(Debug, PartialEq, Clone, Copy, Default)]
pub struct PreparationChecks {
    pub replicas_discovered: bool,
    pub no_primary_on_node: bool,
    pub switchovers_committed: bool,
    pub primary_serving_elsewhere: bool,
    pub quorum_without_node: bool,
    pub node_excluded_from_placement: bool,
    pub application_close_completed: bool,
}

impl PreparationChecks {
    pub fn satisfied(&self) -> bool {
        self.unmet().is_empty()
    }

    pub fn unmet(&self) -> Vec<&'static str> {
        let mut unmet = Vec::new();
        if !self.replicas_discovered {
            unmet.push("replicasDiscovered");
        }
        if !self.no_primary_on_node {
            unmet.push("noPrimaryOnNode");
        }
        if !self.switchovers_committed {
            unmet.push("switchoversCommitted");
        }
        if !self.primary_serving_elsewhere {
            unmet.push("primaryServingElsewhere");
        }
        if !self.quorum_without_node {
            unmet.push("quorumWithoutNode");
        }
        if !self.node_excluded_from_placement {
            unmet.push("nodeExcludedFromPlacement");
        }
        if !self.application_close_completed {
            unmet.push("applicationCloseCompleted");
        }
        unmet
    }
}

pub fn evaluate_preparation(
    checks: &PreparationChecks,
    deadline_exceeded: bool,
) -> (MaintenancePhase, Option<MaintenanceBlockedReason>) {
    if checks.satisfied() {
        return (MaintenancePhase::Prepared, None);
    }
    if deadline_exceeded {
        return (
            MaintenancePhase::Expired,
            Some(MaintenanceBlockedReason::DeadlineExceeded),
        );
    }
    let reason = if !checks.quorum_without_node {
        MaintenanceBlockedReason::BlockedByQuorum
    } else if !checks.primary_serving_elsewhere || !checks.no_primary_on_node {
        MaintenanceBlockedReason::NoEligibleTarget
    } else if !checks.switchovers_committed {
        MaintenanceBlockedReason::SwitchoverFailed
    } else if !checks.application_close_completed {
        MaintenanceBlockedReason::ApplicationCloseIncomplete
    } else {
        MaintenanceBlockedReason::NoEligibleTarget
    };
    (MaintenancePhase::Blocked, Some(reason))
}

#[cfg(test)]
mod tests {
    use super::*;
    use kube::CustomResourceExt;

    fn all_phases() -> [MaintenancePhase; 10] {
        [
            MaintenancePhase::Requested,
            MaintenancePhase::Preparing,
            MaintenancePhase::Prepared,
            MaintenancePhase::Draining,
            MaintenancePhase::Executing,
            MaintenancePhase::Restoring,
            MaintenancePhase::Completed,
            MaintenancePhase::Blocked,
            MaintenancePhase::Failed,
            MaintenancePhase::Expired,
        ]
    }

    fn satisfied_checks() -> PreparationChecks {
        PreparationChecks {
            replicas_discovered: true,
            no_primary_on_node: true,
            switchovers_committed: true,
            primary_serving_elsewhere: true,
            quorum_without_node: true,
            node_excluded_from_placement: true,
            application_close_completed: true,
        }
    }

    #[test]
    fn default_phase_is_requested() {
        assert_eq!(MaintenancePhase::default(), MaintenancePhase::Requested);
        assert_eq!(
            NodeMaintenanceRequestStatus::default().phase,
            MaintenancePhase::Requested
        );
    }

    #[test]
    fn happy_path_transitions_are_allowed() {
        let path = [
            MaintenancePhase::Requested,
            MaintenancePhase::Preparing,
            MaintenancePhase::Prepared,
            MaintenancePhase::Draining,
            MaintenancePhase::Executing,
            MaintenancePhase::Restoring,
            MaintenancePhase::Completed,
        ];
        for pair in path.windows(2) {
            assert!(
                pair[0].can_transition_to(pair[1]),
                "{:?} -> {:?} must be allowed",
                pair[0],
                pair[1]
            );
        }
    }

    #[test]
    fn terminal_phases_accept_no_transition() {
        for terminal in [
            MaintenancePhase::Completed,
            MaintenancePhase::Failed,
            MaintenancePhase::Expired,
        ] {
            assert!(terminal.is_terminal());
            for next in all_phases() {
                assert!(
                    !terminal.can_transition_to(next),
                    "{terminal:?} must not transition to {next:?}"
                );
            }
        }
    }

    #[test]
    fn preparation_cannot_be_skipped() {
        assert!(!MaintenancePhase::Requested.can_transition_to(MaintenancePhase::Prepared));
        assert!(!MaintenancePhase::Requested.can_transition_to(MaintenancePhase::Draining));
        assert!(!MaintenancePhase::Preparing.can_transition_to(MaintenancePhase::Draining));
        assert!(!MaintenancePhase::Preparing.can_transition_to(MaintenancePhase::Executing));
    }

    #[test]
    fn blocked_can_retry_preparation_but_not_jump_to_prepared() {
        assert!(MaintenancePhase::Blocked.can_transition_to(MaintenancePhase::Preparing));
        assert!(!MaintenancePhase::Blocked.can_transition_to(MaintenancePhase::Prepared));
        assert!(!MaintenancePhase::Blocked.can_transition_to(MaintenancePhase::Draining));
    }

    #[test]
    fn prepared_can_be_revoked_when_safety_is_lost() {
        assert!(MaintenancePhase::Prepared.can_transition_to(MaintenancePhase::Blocked));
        assert!(MaintenancePhase::Prepared.can_transition_to(MaintenancePhase::Failed));
    }

    #[test]
    fn only_prepared_states_are_safe_to_drain() {
        for phase in all_phases() {
            let expected = matches!(
                phase,
                MaintenancePhase::Prepared
                    | MaintenancePhase::Draining
                    | MaintenancePhase::Executing
            );
            assert_eq!(phase.is_safe_to_drain(), expected, "{phase:?}");
        }
    }

    #[test]
    fn unsafe_phases_require_a_reason() {
        for phase in all_phases() {
            let expected = matches!(
                phase,
                MaintenancePhase::Blocked | MaintenancePhase::Failed | MaintenancePhase::Expired
            );
            assert_eq!(phase.requires_reason(), expected, "{phase:?}");
        }
    }

    #[test]
    fn satisfied_checks_report_no_unmet_conditions() {
        let checks = satisfied_checks();
        assert!(checks.satisfied());
        assert!(checks.unmet().is_empty());
    }

    #[test]
    fn every_safety_condition_blocks_preparation_on_its_own() {
        type BreakCheck = (&'static str, fn(&mut PreparationChecks));
        let fields: [BreakCheck; 7] = [
            ("replicasDiscovered", |c| c.replicas_discovered = false),
            ("noPrimaryOnNode", |c| c.no_primary_on_node = false),
            ("switchoversCommitted", |c| c.switchovers_committed = false),
            ("primaryServingElsewhere", |c| {
                c.primary_serving_elsewhere = false
            }),
            ("quorumWithoutNode", |c| c.quorum_without_node = false),
            ("nodeExcludedFromPlacement", |c| {
                c.node_excluded_from_placement = false
            }),
            ("applicationCloseCompleted", |c| {
                c.application_close_completed = false
            }),
        ];
        for (name, break_one) in fields {
            let mut checks = satisfied_checks();
            break_one(&mut checks);
            assert!(!checks.satisfied(), "{name} must block preparation");
            assert!(checks.unmet().contains(&name), "{name} must be reported");
            let (phase, reason) = evaluate_preparation(&checks, false);
            assert_ne!(phase, MaintenancePhase::Prepared, "{name}");
            assert!(reason.is_some(), "{name} must produce a structured reason");
        }
    }

    #[test]
    fn preparation_fails_closed_on_deadline() {
        let mut checks = satisfied_checks();
        checks.quorum_without_node = false;
        let (phase, reason) = evaluate_preparation(&checks, true);
        assert_eq!(phase, MaintenancePhase::Expired);
        assert_eq!(reason, Some(MaintenanceBlockedReason::DeadlineExceeded));
    }

    #[test]
    fn deadline_does_not_override_a_satisfied_preparation() {
        let (phase, reason) = evaluate_preparation(&satisfied_checks(), true);
        assert_eq!(phase, MaintenancePhase::Prepared);
        assert_eq!(reason, None);
    }

    #[test]
    fn quorum_loss_is_reported_before_other_reasons() {
        let mut checks = satisfied_checks();
        checks.quorum_without_node = false;
        checks.switchovers_committed = false;
        let (_, reason) = evaluate_preparation(&checks, false);
        assert_eq!(reason, Some(MaintenanceBlockedReason::BlockedByQuorum));
    }

    #[test]
    fn reimage_and_replace_discard_local_state() {
        assert!(MaintenanceOperation::Reimage.discards_local_state());
        assert!(MaintenanceOperation::Replace.discards_local_state());
        assert!(!MaintenanceOperation::Reboot.discards_local_state());
        assert!(!MaintenanceOperation::OsUpgrade.discards_local_state());
        assert!(!MaintenanceOperation::Shutdown.discards_local_state());
    }

    #[test]
    fn phase_and_operation_serialize_as_pascal_case() {
        let phase = serde_json::to_string(&MaintenancePhase::Prepared).unwrap();
        assert_eq!(phase, "\"Prepared\"");
        let operation = serde_json::to_string(&MaintenanceOperation::OsUpgrade).unwrap();
        assert_eq!(operation, "\"OsUpgrade\"");
        let reason = serde_json::to_string(&MaintenanceBlockedReason::BlockedByQuorum).unwrap();
        assert_eq!(reason, "\"BlockedByQuorum\"");
    }

    #[test]
    fn spec_round_trips_through_camel_case_json() {
        let spec = NodeMaintenanceRequestSpec {
            node_name: "aks-nodepool1-0".to_string(),
            operation: MaintenanceOperation::Reboot,
            provider: Some("AzureScheduledEvents".to_string()),
            provider_event_id: Some("abc-123".to_string()),
            not_before: Some("2026-01-01T00:00:00Z".to_string()),
            deadline: Some("2026-01-01T01:00:00Z".to_string()),
        };
        let json = serde_json::to_value(&spec).unwrap();
        assert_eq!(json["nodeName"], "aks-nodepool1-0");
        assert_eq!(json["providerEventId"], "abc-123");
        assert_eq!(json["notBefore"], "2026-01-01T00:00:00Z");
        let decoded: NodeMaintenanceRequestSpec = serde_json::from_value(json).unwrap();
        assert_eq!(decoded, spec);
    }

    #[test]
    fn crd_is_cluster_scoped_and_exposes_safety_fields() {
        let crd = serde_json::to_value(NodeMaintenanceRequest::crd()).unwrap();
        assert_eq!(crd["spec"]["scope"], "Cluster");
        assert_eq!(crd["spec"]["names"]["kind"], "NodeMaintenanceRequest");
        assert_eq!(
            crd["metadata"]["name"],
            "nodemaintenancerequests.kuberic.io"
        );

        let generated = serde_json::to_string(&crd).unwrap();
        for required in [
            "nodeName",
            "providerEventId",
            "notBefore",
            "deadline",
            "blockedReason",
            "affectedSets",
            "quorumWithoutNode",
            "hostsPrimary",
            "primaryMoved",
            "preparedAt",
            "observedGeneration",
            "nodeUid",
        ] {
            assert!(
                generated.contains(required),
                "missing generated schema {required}"
            );
        }
    }

    #[test]
    fn deployment_manifest_installs_the_maintenance_crd_and_node_access() {
        let deployment = include_str!("../deploy/deployment.yaml");
        for required in [
            "nodemaintenancerequests.kuberic.io",
            "nodemaintenancerequests",
            "nodemaintenancerequests/status",
            "NodeMaintenanceRequest",
            "v1alpha1",
            "nodes",
        ] {
            assert!(
                deployment.contains(required),
                "deployment.yaml missing {required}"
            );
        }
    }
}
