use kuberic_controller::crd::KubericSetStatus;
use kuberic_runtime::control::{normalize_public_v1_agent_status_report, proto};
use kuberic_runtime::protocol::public_operations::{
    PublicOperationPreviewIdentity, PublicV1AcceptedStatus,
};
use kuberic_runtime::protocol::types::{
    HistoryContext, HistoryId, HistoryInitializationId, PublicProtocolGeneration, ResourceUid,
};

#[test]
fn old_and_partial_controller_inputs_reject_without_mutation() {
    let old = proto::AgentStatusReport {
        protocol_version: 9,
        public_v1: Some(proto::PublicV1AgentStatus::default()),
        ..Default::default()
    };
    let old_before = old.clone();
    assert!(normalize_public_v1_agent_status_report(old.clone()).is_err());
    assert_eq!(old, old_before);

    let partial = proto::AgentStatusReport {
        protocol_version: 11,
        public_v1: Some(proto::PublicV1AgentStatus {
            protocol_version: 11,
            generation: 1,
            resource_uid: "resource".into(),
            ..Default::default()
        }),
        ..Default::default()
    };
    let partial_before = partial.clone();
    assert!(normalize_public_v1_agent_status_report(partial.clone()).is_err());
    assert_eq!(partial, partial_before);
}

#[test]
fn old_and_partial_controller_status_reject_without_mutation() {
    let mut old = KubericSetStatus::default();
    old.authority.initialized = true;
    let old_before = old.clone();
    assert!(old.validate_public_v1().is_err());
    assert_eq!(old, old_before);

    let partial = KubericSetStatus {
        public_v1_authority: Some(PublicV1AcceptedStatus {
            identity: PublicOperationPreviewIdentity::public_v1(1),
            history_initialization_id: HistoryInitializationId::new(""),
            history: HistoryContext {
                resource_uid: ResourceUid::new("resource-1"),
                protocol_generation: PublicProtocolGeneration::new(1),
                history_id: HistoryId::new(""),
                data_loss_number: 0,
            },
            data_loss_recovery: None,
            completed_operations: Vec::new(),
            extensions: Default::default(),
        }),
        ..Default::default()
    };
    let partial_before = partial.clone();
    assert!(partial.validate_public_v1().is_err());
    assert_eq!(partial, partial_before);
}
