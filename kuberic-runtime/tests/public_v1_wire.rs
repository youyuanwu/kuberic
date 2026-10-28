use kuberic_runtime::control::{
    normalize_execute_request, proto, public_v1_history_command_to_proto,
    public_v1_operation_command_to_proto, validate_execute_request,
};
use kuberic_runtime::protocol::command::{
    ProtocolCommand, PublicV1HistoryCommand, PublicV1OperationCommand,
    PublicV1SessionRenewalCommand,
};
use kuberic_runtime::protocol::public_operations::{
    ProcessSessionRenewalRequired, PublicOperationClass, PublicOperationIntent,
    PublicOperationProgram, PublicV1Identity, PublicV1SessionRenewalStatus,
};
use kuberic_runtime::protocol::types::{
    AgentGeneration, ConfigurationDescriptor, ConfigurationMember, Epoch, OperationId, PodUid,
    ProcessSessionEndpoint, ProcessSessionId, ProcessSessionRenewalReason,
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

#[test]
fn availability_is_required_canonical_evidence_for_every_configuration_input() {
    use kuberic_runtime::protocol::public_operations::*;
    let remote = target_storage();
    let current = ConfigurationDescriptor::new(
        Epoch::new(0, 1),
        identity().replica_id,
        vec![
            ConfigurationMember {
                identity: identity(),
                role: ReplicaRole::Primary,
            },
            ConfigurationMember {
                identity: remote.replica.clone(),
                role: ReplicaRole::ActiveSecondary,
            },
        ],
        1,
    );
    let configuration = PublicConfiguration {
        current: current.clone(),
        previous: None,
        replicas: vec![PublicReplicaDescriptor {
            identity: remote.replica.clone(),
            replication_address: "http://127.0.0.1:50052".into(),
            process_session_id: ProcessSessionId::new("session-2"),
            storage_incarnation: Some(remote),
            role: ReplicaRole::ActiveSecondary,
            current_progress: 0,
            catch_up_capability: 0,
            is_up: true,
            is_ready: true,
            must_catchup: false,
        }],
    };
    let mut refreshed = configuration.clone();
    refreshed.current = ConfigurationDescriptor::new(
        Epoch::new(0, 2),
        identity().replica_id,
        configuration.current.members.clone(),
        1,
    );
    let programs = [
        PublicOperationProgram::Configuration(configuration.clone()),
        PublicOperationProgram::CatchUp {
            configuration: configuration.clone(),
            mode: PublicCatchUpMode::All,
        },
        PublicOperationProgram::Swap {
            starting: configuration.clone(),
            refreshed,
            epoch: Epoch::new(0, 2),
            handoff: ReplicaRole::ActiveSecondary,
            catch_up_specific_quorum: false,
            mode: PublicCatchUpMode::All,
            controller_composed: false,
        },
    ];
    for program in programs {
        let encoded = serde_json::to_vec(&program).unwrap();
        assert_eq!(
            serde_json::from_slice::<PublicOperationProgram>(&encoded).unwrap(),
            program
        );
        let intent = PublicOperationIntent {
            identity: PublicV1Identity::new(1),
            operation_id: OperationId::new("wire-availability"),
            revision: 1,
            process_session_id: ProcessSessionId::new("session-1"),
            class: if matches!(program, PublicOperationProgram::Swap { .. }) {
                PublicOperationClass::PlannedSwap
            } else {
                PublicOperationClass::Authority
            },
            input_digest: program.digest(),
            replay_disposition: program.replay_disposition(),
            lifecycle: None,
            program: Some(program.clone()),
        };
        let command = PublicV1OperationCommand {
            target: identity(),
            intent,
        };
        let request = proto::ExecuteCommandRequest {
            protocol_version: PUBLIC_V1_PROTOCOL_VERSION,
            resource_uid: "resource-1".into(),
            target: Some(identity().into()),
            expected_process_session_id: "session-1".into(),
            command: Some(proto::execute_command_request::Command::PublicV1Operation(
                public_v1_operation_command_to_proto(&command).unwrap(),
            )),
        };
        assert_eq!(
            normalize_execute_request(request.clone()).unwrap().command,
            ProtocolCommand::PublicV1Operation(Box::new(command))
        );
        let mut old = request;
        old.protocol_version = 12;
        assert!(normalize_execute_request(old).is_err());
        for is_up in [false, true] {
            for refreshed in [false, true] {
                let mut changed = program.clone();
                let configuration = match &mut changed {
                    PublicOperationProgram::Configuration(configuration)
                    | PublicOperationProgram::CatchUp { configuration, .. } => configuration,
                    PublicOperationProgram::Swap {
                        starting,
                        refreshed: final_configuration,
                        ..
                    } => {
                        if refreshed {
                            final_configuration
                        } else {
                            starting
                        }
                    }
                    _ => unreachable!(),
                };
                if is_up {
                    configuration.replicas[0].is_up = false;
                } else {
                    configuration.replicas[0].is_ready = false;
                }
                assert_ne!(changed.digest(), program.digest());
                let intent = PublicOperationIntent {
                    identity: PublicV1Identity::new(1),
                    operation_id: OperationId::new("canonical-availability"),
                    revision: 1,
                    process_session_id: ProcessSessionId::new("session-1"),
                    class: if matches!(changed, PublicOperationProgram::Swap { .. }) {
                        PublicOperationClass::PlannedSwap
                    } else {
                        PublicOperationClass::Authority
                    },
                    input_digest: program.digest(),
                    replay_disposition: changed.replay_disposition(),
                    lifecycle: None,
                    program: Some(changed),
                };
                assert!(
                    intent.validate().is_err(),
                    "changed evidence reused an old canonical digest"
                );
            }
        }
    }
    let input = PublicLifecycleInput {
        recipe: PublicLifecycleRecipe::InitialPrimary,
        replica: identity(),
        epoch: current.epoch,
        possible_data_loss: PossibleDataLossIntent::NotPossible,
        current,
        previous: None,
        replicas: configuration.replicas.clone(),
        data_loss_recovery: None,
    };
    for up in [false, true] {
        let mut changed = input.clone();
        if up {
            changed.replicas[0].is_up = false;
        } else {
            changed.replicas[0].is_ready = false;
        }
        assert_ne!(changed.digest(), input.digest());
        assert!(
            PublicOperationIntent {
                identity: PublicV1Identity::new(1),
                operation_id: OperationId::new("lifecycle-availability"),
                revision: 1,
                process_session_id: ProcessSessionId::new("session-1"),
                class: PublicOperationClass::Authority,
                input_digest: input.digest(),
                replay_disposition: PublicOperationReplayDisposition::Convergent,
                lifecycle: Some(changed),
                program: None,
            }
            .validate()
            .is_err()
        );
    }
    assert_eq!(
        serde_json::from_slice::<PublicLifecycleInput>(&serde_json::to_vec(&input).unwrap())
            .unwrap(),
        input
    );
    let json = serde_json::to_value(&configuration.replicas[0]).unwrap();
    for field in ["isUp", "isReady"] {
        let mut old = json.clone();
        old.as_object_mut().unwrap().remove(field);
        assert!(serde_json::from_value::<PublicReplicaDescriptor>(old).is_err());
    }
    let schema = schemars::schema_for!(PublicReplicaDescriptor);
    let required = schema.as_value()["required"].as_array().unwrap();
    assert!(required.iter().any(|field| field == "isUp"));
    assert!(required.iter().any(|field| field == "isReady"));
    assert_eq!(PUBLIC_V1_PROTOCOL_VERSION, 23);
    assert!(
        !PublicV1Identity {
            protocol_version: 20,
            generation: 1
        }
        .is_valid()
    );
    assert!(
        !PublicV1Identity {
            protocol_version: 12,
            generation: 1
        }
        .is_valid()
    );
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
fn accepted_native_stage_ledger_preserves_its_capacity_boundary() {
    use kuberic_runtime::protocol::public_operations::{
        PublicV1AcceptedOperationCompletion, PublicV1CallKind, PublicV1OperationCompletion,
        PublicV1OperationResult,
    };
    let mut authority = accepted();
    let program = PublicOperationProgram::Progress { capability: false };
    for revision in 1..=256 {
        authority
            .completed_operations
            .push(PublicV1AcceptedOperationCompletion::from_completion(
                &PublicV1OperationCompletion {
                    operation_id: OperationId::new(format!("native-stage-{revision}")),
                    revision,
                    call_kind: PublicV1CallKind::Progress,
                    process_session_id: ProcessSessionId::new("session-1"),
                    input_digest: program.digest(),
                    replay_disposition: program.replay_disposition(),
                    history: authority.history.clone(),
                    storage_incarnation: storage(),
                    previous_configuration_id: None,
                    current_configuration_id: None,
                    catch_up_mode: None,
                    catch_up_specific_quorum: None,
                    result: PublicV1OperationResult::Succeeded,
                },
            ));
    }
    authority.validate_evidence().unwrap();
    let previous = authority.clone();
    let mut overflow = authority.completed_operations.last().unwrap().clone();
    overflow.operation_id = OperationId::new("native-stage-overflow");
    overflow.revision += 1;
    authority.completed_operations.push(overflow);
    assert!(authority.is_evidence_successor_of(&previous));
    assert!(authority.validate_evidence().is_err());
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
fn removed_inbound_authority_commands_are_not_wire_inputs() {
    for kind in [
        "installInboundBuildAuthority",
        "revokeInboundBuildAuthority",
    ] {
        let input = serde_json::json!({"kind": kind, "authority": {}});
        assert!(serde_json::from_value::<PublicV1HistoryCommand>(input).is_err());
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
