use kuberic_runtime::control::{
    normalize_execute_request, proto, public_v1_history_command_to_proto,
    public_v1_operation_command_to_proto, validate_execute_request,
};
use kuberic_runtime::protocol::command::{
    ProtocolCommand, PublicV1HistoryCommand, PublicV1OperationCommand,
    PublicV1SessionRenewalCommand,
};
use kuberic_runtime::protocol::public_operations::{
    ProcessSessionRenewalRequired, PublicBuildAttemptReference, PublicInboundBuildAuthority,
    PublicOperationClass, PublicOperationIntent, PublicOperationProgram, PublicV1Identity,
    PublicV1SessionRenewalStatus,
};
use kuberic_runtime::protocol::types::{
    AgentGeneration, ConfigurationDescriptor, ConfigurationId, ConfigurationMember, Epoch,
    OperationId, PodUid, ProcessSessionEndpoint, ProcessSessionId, ProcessSessionRenewalReason,
    PublicProtocolGeneration, PvcUid, ReplicaId, ReplicaIdentity, ReplicaInstanceId, ReplicaRole,
    ReplicaStorageIdentity, ReplicaStorageIncarnationContext, ReplicatorCreationId, ResourceUid,
    StorageGeneration, derive_bootstrap_history, derive_history_initialization_id,
};
use sha2::{Digest, Sha256};

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

fn target_storage() -> ReplicaStorageIncarnationContext {
    let replica = ReplicaIdentity {
        replica_id: ReplicaId::new(2),
        instance_id: ReplicaInstanceId::new("pod-2"),
        agent_generation: AgentGeneration::new("generation-2"),
    };
    ReplicaStorageIncarnationContext {
        replica,
        storage: ReplicaStorageIdentity::Persistent {
            pod_uid: PodUid::new("pod-2"),
            pvc_uid: PvcUid::new("pvc-2"),
        },
        storage_generation: StorageGeneration::new("storage-2"),
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

fn renewal_required() -> ProcessSessionRenewalRequired {
    let accepted = accepted();
    let predecessor = ProcessSessionEndpoint {
        identity: identity(),
        storage_incarnation: storage(),
        process_session_id: ProcessSessionId::new("session-old"),
        replicator_creation_id: ReplicatorCreationId::new("creation-old"),
        role: ReplicaRole::Primary,
    };
    let replacement = ProcessSessionEndpoint {
        process_session_id: ProcessSessionId::new("session-new"),
        replicator_creation_id: ReplicatorCreationId::new("creation-new"),
        role: ReplicaRole::None,
        ..predecessor.clone()
    };
    let survivor_storage = target_storage();
    let survivor = ProcessSessionEndpoint {
        identity: survivor_storage.replica.clone(),
        storage_incarnation: survivor_storage,
        process_session_id: ProcessSessionId::new("session-2"),
        replicator_creation_id: ReplicatorCreationId::new("creation-2"),
        role: ReplicaRole::ActiveSecondary,
    };
    let renewal_epoch = Epoch::new(0, 2);
    ProcessSessionRenewalRequired {
        operation_id: OperationId::new("renewal-1"),
        reason: ProcessSessionRenewalReason::PrimaryReplacement,
        resource_uid: accepted.history.resource_uid.clone(),
        history: accepted.history,
        previous_epoch: Epoch::new(0, 1),
        renewal_epoch,
        predecessor,
        replacement: Some(replacement),
        surviving_sessions: vec![survivor.clone()],
        fresh_configuration: ConfigurationDescriptor::new(
            renewal_epoch,
            ReplicaId::new(1),
            vec![
                ConfigurationMember {
                    identity: identity(),
                    role: ReplicaRole::Primary,
                },
                ConfigurationMember {
                    identity: survivor.identity,
                    role: ReplicaRole::ActiveSecondary,
                },
            ],
            1,
        ),
        predecessor_contained: true,
        data_loss: None,
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

#[test]
fn public_v1_inbound_build_authority_commands_round_trip() {
    let accepted = accepted();
    let source_storage = storage();
    let target_storage = target_storage();
    let authority = PublicInboundBuildAuthority {
        operation_id: OperationId::new("build-operation"),
        operation_revision: 7,
        attempt: PublicBuildAttemptReference {
            resource_uid: ResourceUid::new("resource-1"),
            epoch: Epoch::new(0, 2),
            build_id: OperationId::new("build-attempt"),
            source: source_storage.replica.clone(),
            source_storage,
            source_session: ProcessSessionId::new("source-session"),
            target: target_storage.replica.clone(),
            target_storage: target_storage.clone(),
            target_session: ProcessSessionId::new("target-session"),
        },
        history: accepted.history,
        current_configuration_id: ConfigurationId::new("configuration-2"),
        target_address: "http://pod-2:50052".into(),
    };
    for command in [
        PublicV1HistoryCommand::InstallInboundBuildAuthority {
            authority: Box::new(authority.clone()),
        },
        PublicV1HistoryCommand::RevokeInboundBuildAuthority {
            authority: Box::new(authority.clone()),
        },
    ] {
        let request = proto::ExecuteCommandRequest {
            protocol_version: kuberic_runtime::protocol::PROTOCOL_VERSION,
            resource_uid: "resource-1".into(),
            target: Some(target_storage.replica.clone().into()),
            expected_process_session_id: "target-session".into(),
            command: Some(proto::execute_command_request::Command::PublicV1History(
                public_v1_history_command_to_proto(&command).unwrap(),
            )),
        };
        assert_eq!(
            normalize_execute_request(request).unwrap().command,
            ProtocolCommand::PublicV1History(Box::new(command))
        );
    }
}

#[test]
fn public_v1_session_renewal_commands_round_trip_with_one_kind_per_layer() {
    let required = PublicV1SessionRenewalStatus::required(renewal_required()).unwrap();
    let closed = required.close_access().unwrap();
    let frozen = closed.freeze_surviving_set().unwrap();
    let barriers = frozen.begin_epoch_barriers().unwrap();
    let survivor = barriers.required.surviving_sessions[0].clone();
    let updated = barriers
        .record_epoch_updated(&survivor, barriers.required.renewal_epoch)
        .unwrap();
    let role_pending = updated.finish_epoch_barriers().unwrap();
    let configuration_pending = role_pending
        .replay_replacement_role(
            &role_pending
                .required
                .replacement
                .as_ref()
                .unwrap()
                .process_session_id,
            role_pending.required.renewal_epoch,
            ReplicaRole::Primary,
        )
        .unwrap();
    let commands = [
        PublicV1SessionRenewalCommand::CloseAccessAndIngress { accepted: required },
        PublicV1SessionRenewalCommand::UpdateEpochBarrier {
            epoch: barriers.required.renewal_epoch,
            survivor,
            accepted: barriers,
        },
        PublicV1SessionRenewalCommand::ReplayReplacementRole {
            process_session_id: role_pending
                .required
                .replacement
                .as_ref()
                .unwrap()
                .process_session_id
                .clone(),
            epoch: role_pending.required.renewal_epoch,
            role: ReplicaRole::Primary,
            accepted: role_pending,
        },
        PublicV1SessionRenewalCommand::InstallFreshConfiguration {
            configuration: configuration_pending.required.fresh_configuration.clone(),
            accepted: configuration_pending,
        },
    ];
    let expected_json_sha256 = [
        "1d80825ec662b09baa81700558eb2b57eaf2423a52ca6779271de7e88cf92845",
        "7f68901fa3c287915192259838eba920f196c855e64b9a4f43cc26857ee29d5f",
        "0a132183aca9d5441d071962c6dc1e889855ace1eaa1ba0a6db084c2b4fbec65",
        "3fd0f587726675d40a1b70e2f5484d5288fd2fb036bd59cef404c8fb118a6ed0",
    ];
    for (index, command) in commands.into_iter().enumerate() {
        let expected_endpoint = command.expected_endpoint().clone();
        let history = PublicV1HistoryCommand::SessionRenewal {
            command: Box::new(command),
        };
        let json = serde_json::to_value(&history).unwrap();
        let canonical_json = serde_json::to_vec(&history).unwrap();
        let digest = Sha256::digest(&canonical_json)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        assert_eq!(digest, expected_json_sha256[index]);
        assert_eq!(
            json.get("kind").and_then(serde_json::Value::as_str),
            Some("sessionRenewal")
        );
        assert_eq!(
            json.as_object()
                .unwrap()
                .keys()
                .filter(|key| key.as_str() == "kind")
                .count(),
            1
        );
        assert!(
            json.get("command")
                .and_then(|value| value.get("kind"))
                .is_some()
        );
        let request = proto::ExecuteCommandRequest {
            protocol_version: kuberic_runtime::protocol::PROTOCOL_VERSION,
            resource_uid: "resource-1".into(),
            target: Some(expected_endpoint.identity.into()),
            expected_process_session_id: expected_endpoint.process_session_id.to_string(),
            command: Some(proto::execute_command_request::Command::PublicV1History(
                public_v1_history_command_to_proto(&history).unwrap(),
            )),
        };
        assert_eq!(
            normalize_execute_request(request).unwrap().command,
            ProtocolCommand::PublicV1History(Box::new(history))
        );
    }
}
