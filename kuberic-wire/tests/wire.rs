use kuberic_protocol::types::{
    AgentGeneration, ConfigurationDescriptor, ConfigurationMember, Epoch, InitializationId,
    ReplicaId, ReplicaIdentity, ReplicaInstanceId, ReplicaRole, derive_agent_generation,
};
use kuberic_wire::convert::WireError;
use kuberic_wire::{
    ensure_supported_version, proto, validate_agent_status_report, validate_execute_request,
    validate_replication_ack, validate_replication_item,
};

fn configuration() -> ConfigurationDescriptor {
    ConfigurationDescriptor::new(
        Epoch::new(0, 1),
        ReplicaId::new(1),
        vec![
            ConfigurationMember {
                identity: ReplicaIdentity {
                    replica_id: ReplicaId::new(1),
                    instance_id: ReplicaInstanceId::new("pod-1"),
                    agent_generation: AgentGeneration::new("generation-1"),
                },
                role: ReplicaRole::Primary,
            },
            ConfigurationMember {
                identity: ReplicaIdentity {
                    replica_id: ReplicaId::new(2),
                    instance_id: ReplicaInstanceId::new("pod-2"),
                    agent_generation: AgentGeneration::new("generation-2"),
                },
                role: ReplicaRole::ActiveSecondary,
            },
            ConfigurationMember {
                identity: ReplicaIdentity {
                    replica_id: ReplicaId::new(3),
                    instance_id: ReplicaInstanceId::new("pod-3"),
                    agent_generation: AgentGeneration::new("generation-3"),
                },
                role: ReplicaRole::ActiveSecondary,
            },
        ],
        2,
    )
}

#[test]
fn protocol_version_requires_exact_match() {
    assert!(ensure_supported_version(kuberic_protocol::PROTOCOL_VERSION).is_ok());
    assert!(matches!(
        ensure_supported_version(kuberic_protocol::PROTOCOL_VERSION + 1),
        Err(WireError::UnsupportedProtocolVersion { .. })
    ));
}

#[test]
fn identity_requires_exact_incarnation_and_generation() {
    let missing_instance = proto::ReplicaIdentity {
        replica_id: 1,
        instance_id: String::new(),
        agent_generation: "generation".to_string(),
    };
    assert!(matches!(
        ReplicaIdentity::try_from(missing_instance),
        Err(WireError::MissingField("replica_identity.instance_id"))
    ));

    let missing_generation = proto::ReplicaIdentity {
        replica_id: 1,
        instance_id: "pod".to_string(),
        agent_generation: String::new(),
    };
    assert!(matches!(
        ReplicaIdentity::try_from(missing_generation),
        Err(WireError::MissingField("replica_identity.agent_generation"))
    ));
}

#[test]
fn configuration_round_trip_preserves_canonical_authority() {
    let original = configuration();
    let wire: proto::Configuration = original.clone().into();
    let decoded = ConfigurationDescriptor::try_from(wire).unwrap();
    assert_eq!(decoded, original);
}

#[test]
fn configuration_id_mismatch_is_rejected() {
    let mut wire: proto::Configuration = configuration().into();
    wire.configuration_id = "cfg-conflict".to_string();

    assert!(matches!(
        ConfigurationDescriptor::try_from(wire),
        Err(WireError::InvalidAuthority(_))
    ));
}

#[test]
fn uninitialized_status_requires_pod_and_pvc_without_durable_identity() {
    let report = proto::AgentStatusReport {
        protocol_version: kuberic_protocol::PROTOCOL_VERSION,
        resource_uid: "resource".to_string(),
        identity: None,
        process_session_id: "session".to_string(),
        report_sequence: 1,
        role: proto::ReplicaRole::Unknown as i32,
        write_status: proto::AccessStatus::Unknown as i32,
        epoch: None,
        previous_configuration: None,
        current_configuration: None,
        current_progress: 0,
        committed_lsn: 0,
        catch_up_capability: None,
        storage_state: proto::AgentStorageState::Uninitialized as i32,
        pod_uid: "pod".to_string(),
        pvc_uid: "pvc".to_string(),
        storage_error: String::new(),
    };
    assert!(validate_agent_status_report(&report).is_ok());

    let mut invalid = report;
    invalid.identity = Some(proto::ReplicaIdentity {
        replica_id: 1,
        instance_id: "pod".to_string(),
        agent_generation: "generation".to_string(),
    });
    assert!(matches!(
        validate_agent_status_report(&invalid),
        Err(WireError::InvalidAuthority(_))
    ));
}

#[test]
fn initialize_request_carries_complete_fresh_storage_fence() {
    let assigned_generation = derive_agent_generation(&InitializationId::new("init"));
    let request = proto::ExecuteCommandRequest {
        protocol_version: kuberic_protocol::PROTOCOL_VERSION,
        resource_uid: "resource".to_string(),
        target: None,
        command: Some(
            proto::execute_command_request::Command::InitializeAgentStore(
                proto::InitializeAgentStoreCommand {
                    initialization_id: "init".to_string(),
                    resource_uid: "resource".to_string(),
                    local_replica_id: 1,
                    expected_instance_id: "pod".to_string(),
                    expected_pod_uid: "pod".to_string(),
                    expected_pvc_uid: "pvc".to_string(),
                    assigned_agent_generation: assigned_generation.to_string(),
                    effective_policy: Some(proto::EffectivePolicy {
                        replica_set_size: 3,
                        write_quorum: 2,
                        read_quorum: 2,
                        failover_delay_seconds: 10,
                    }),
                },
            ),
        ),
    };
    assert!(validate_execute_request(&request).is_ok());

    let mut invalid = request;
    let Some(proto::execute_command_request::Command::InitializeAgentStore(command)) =
        invalid.command.as_mut()
    else {
        unreachable!();
    };
    command.assigned_agent_generation.clear();
    assert!(matches!(
        validate_execute_request(&invalid),
        Err(WireError::MissingField(
            "initialize.assigned_agent_generation"
        ))
    ));
}

#[test]
fn replication_ack_requires_exact_authority_and_consistent_progress() {
    let sender = proto::ReplicaIdentity {
        replica_id: 1,
        instance_id: "pod-1".to_string(),
        agent_generation: "generation-1".to_string(),
    };
    let receiver = proto::ReplicaIdentity {
        replica_id: 2,
        instance_id: "pod-2".to_string(),
        agent_generation: "generation-2".to_string(),
    };
    let ack = proto::ReplicationAck {
        protocol_version: kuberic_protocol::PROTOCOL_VERSION,
        sender: Some(sender.clone()),
        receiver: Some(receiver),
        epoch: Some(proto::Epoch {
            data_loss_number: 0,
            configuration_number: 1,
        }),
        previous_configuration_id: String::new(),
        current_configuration_id: "cfg".to_string(),
        received_lsn: 10,
        applied_lsn: 10,
        committed_lsn: 9,
    };
    assert!(validate_replication_ack(&ack).is_ok());

    let item = proto::ReplicationItem {
        protocol_version: kuberic_protocol::PROTOCOL_VERSION,
        sender: Some(sender),
        epoch: ack.epoch,
        previous_configuration_id: String::new(),
        current_configuration_id: "cfg".to_string(),
        lsn: 10,
        committed_lsn: 9,
        data: vec![1],
    };
    assert!(validate_replication_item(&item).is_ok());

    let mut invalid = ack;
    invalid.applied_lsn = 11;
    assert!(matches!(
        validate_replication_ack(&invalid),
        Err(WireError::InvalidAuthority(_))
    ));
}
