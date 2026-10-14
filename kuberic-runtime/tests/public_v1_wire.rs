use kuberic_runtime::control::{
    normalize_execute_request, proto, public_v1_history_command_to_proto,
    public_v1_operation_command_to_proto, validate_execute_request,
};
use kuberic_runtime::protocol::command::{
    ProtocolCommand, PublicV1HistoryCommand, PublicV1OperationCommand,
};
use kuberic_runtime::protocol::public_operations::{
    PublicOperationClass, PublicOperationIntent, PublicOperationProgram, PublicV1Identity,
};
use kuberic_runtime::protocol::types::{
    AgentGeneration, OperationId, PodUid, PublicProtocolGeneration, PvcUid, ReplicaId,
    ReplicaIdentity, ReplicaInstanceId, ReplicaStorageIdentity, ReplicaStorageIncarnationContext,
    ResourceUid, StorageGeneration, derive_bootstrap_history, derive_history_initialization_id,
};

fn identity() -> ReplicaIdentity {
    ReplicaIdentity {
        replica_id: ReplicaId::new(1),
        instance_id: ReplicaInstanceId::new("pod-1"),
        agent_generation: AgentGeneration::new("generation-1"),
    }
}

fn storage() -> ReplicaStorageIncarnationContext {
    ReplicaStorageIncarnationContext {
        replica: identity(),
        storage: ReplicaStorageIdentity::Persistent {
            pod_uid: PodUid::new("pod-1"),
            pvc_uid: PvcUid::new("pvc-1"),
        },
        storage_generation: StorageGeneration::new("storage-1"),
    }
}

fn accepted() -> kuberic_runtime::protocol::public_operations::PublicV1AcceptedStatus {
    let resource_uid = ResourceUid::new("resource-1");
    let generation = PublicProtocolGeneration::new(1);
    let initialization = derive_history_initialization_id(&resource_uid, generation);
    kuberic_runtime::protocol::public_operations::PublicV1AcceptedStatus {
        identity: PublicV1Identity::new(1),
        history_initialization_id: initialization.clone(),
        history: derive_bootstrap_history(&resource_uid, generation, &initialization),
        data_loss_recovery: None,
        completed_operations: Vec::new(),
        extensions: Default::default(),
    }
}

#[test]
fn public_v1_history_command_round_trips_with_exact_envelope_fences() {
    let accepted = accepted();
    let command = PublicV1HistoryCommand::Initialize {
        identity: accepted.identity.clone(),
        accepted,
        storage: storage(),
    };
    let serialized = serde_json::to_value(&command).unwrap();
    assert!(serialized.get("identity").is_some());
    assert!(serialized.get("preview").is_none());
    let request = proto::ExecuteCommandRequest {
        protocol_version: kuberic_runtime::protocol::PROTOCOL_VERSION,
        resource_uid: "resource-1".into(),
        target: Some(identity().into()),
        expected_process_session_id: "session-1".into(),
        command: Some(proto::execute_command_request::Command::PublicV1History(
            public_v1_history_command_to_proto(&command).unwrap(),
        )),
    };
    let normalized = normalize_execute_request(request.clone()).unwrap();
    assert_eq!(
        normalized.command,
        ProtocolCommand::PublicV1History(Box::new(command))
    );

    let mut crossed = request;
    crossed.resource_uid = "other-resource".into();
    assert!(validate_execute_request(&crossed).is_err());
    assert_eq!(crossed.resource_uid, "other-resource");
}

#[test]
fn public_v1_operation_command_round_trips_and_rejects_partial_or_crossed_input() {
    let program = PublicOperationProgram::Abort;
    let intent = PublicOperationIntent {
        identity: PublicV1Identity::new(1),
        operation_id: OperationId::new("operation-1"),
        revision: 1,
        process_session_id: kuberic_runtime::protocol::types::ProcessSessionId::new("session-1"),
        class: PublicOperationClass::Abort,
        input_digest: program.digest(),
        replay_disposition: program.replay_disposition(),
        lifecycle: None,
        program: Some(program),
    };
    let mut legacy_intent = intent.clone();
    legacy_intent.identity.protocol_version = 10;
    assert!(
        legacy_intent.validate().is_err(),
        "protocol 10 must not enter the Public V1 operation path"
    );
    let serialized = serde_json::to_value(&intent).unwrap();
    assert!(serialized.get("identity").is_some());
    assert!(serialized.get("preview").is_none());
    let command = PublicV1OperationCommand {
        target: identity(),
        intent,
    };
    let wire = public_v1_operation_command_to_proto(&command).unwrap();
    let request = proto::ExecuteCommandRequest {
        protocol_version: kuberic_runtime::protocol::PROTOCOL_VERSION,
        resource_uid: "resource-1".into(),
        target: Some(identity().into()),
        expected_process_session_id: "session-1".into(),
        command: Some(proto::execute_command_request::Command::PublicV1Operation(
            wire.clone(),
        )),
    };
    assert_eq!(
        normalize_execute_request(request.clone()).unwrap().command,
        ProtocolCommand::PublicV1Operation(Box::new(command))
    );
    assert_eq!(
        normalize_execute_request(request.clone()).unwrap(),
        normalize_execute_request(request.clone()).unwrap()
    );

    let mut partial = request.clone();
    partial.command = Some(proto::execute_command_request::Command::PublicV1Operation(
        proto::PublicV1OperationCommand::default(),
    ));
    assert!(validate_execute_request(&partial).is_err());

    let mut crossed = request;
    crossed.expected_process_session_id = "other-session".into();
    assert!(validate_execute_request(&crossed).is_err());
}
