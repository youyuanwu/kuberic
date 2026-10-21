use super::*;
use kuberic_runtime::testing::{PublicCutPosition as Cut, PublicOperationCut};

fn hold(node: &Node, position: Cut, index: usize) -> Arc<PublicOperationCut> {
    let cut = Arc::new(PublicOperationCut {
        position,
        index,
        entered: Notify::new(),
        release: Notify::new(),
    });
    node.handle().set_public_operation_cut(Some(cut.clone()));
    cut
}

async fn entered(
    cut: &PublicOperationCut,
    task: &mut tokio::task::JoinHandle<kuberic_runtime::host::Result<()>>,
) {
    tokio::select! {
        () = cut.entered.notified() => {},
        result = task => panic!("selected operation returned before barrier: {result:?}"),
        () = tokio::time::sleep(Duration::from_secs(10)) => panic!("selected operation did not reach {position:?}/{index}", position = cut.position, index = cut.index),
    }
}

async fn finish(
    node: &Node,
    cut: &PublicOperationCut,
    task: tokio::task::JoinHandle<kuberic_runtime::host::Result<()>>,
) {
    node.handle().set_public_operation_cut(None);
    cut.release.notify_one();
    tokio::time::timeout(Duration::from_secs(15), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

fn execute(
    node: &Node,
    intent: PublicOperationIntent,
) -> tokio::task::JoinHandle<kuberic_runtime::host::Result<()>> {
    let handle = node.handle();
    tokio::spawn(async move { handle.execute_public_v1(intent).await })
}

async fn rpc(
    node: &Node,
    intent: PublicOperationIntent,
) -> tokio::task::JoinHandle<kuberic_runtime::host::Result<()>> {
    use kuberic_runtime::control::{convert, proto};
    let mut client =
        proto::agent_control_client::AgentControlClient::connect(node.control_address.clone())
            .await
            .unwrap();
    let command = kuberic_runtime::protocol::command::PublicV1OperationCommand {
        target: node.identity.clone(),
        intent,
    };
    let mut request = Request::new(proto::ExecuteCommandRequest {
        protocol_version: kuberic_runtime::protocol::PROTOCOL_VERSION,
        resource_uid: node.history.resource_uid.to_string(),
        target: Some(node.identity.clone().into()),
        expected_process_session_id: node.handle().process_session_id().to_string(),
        command: Some(proto::execute_command_request::Command::PublicV1Operation(
            convert::public_v1_operation_command_to_proto(&command).unwrap(),
        )),
    });
    request
        .metadata_mut()
        .insert("authorization", format!("Bearer {TOKEN}").parse().unwrap());
    tokio::spawn(async move {
        client
            .execute(request)
            .await
            .map(|_| ())
            .map_err(|error| kuberic_runtime::host::HostError::CommandRejected(error.to_string()))
    })
}

async fn stage(node: &Node, read: AccessStatus, write: AccessStatus) {
    let partition = node.service.partition.read().await.clone().unwrap();
    assert_eq!(partition.get_read_status().await.unwrap(), read);
    assert_eq!(partition.get_write_status().await.unwrap(), write);
    let report = node.report().await;
    assert_eq!((report.read_status, report.write_status), (read, write));
    let diagnostics = node.handle().diagnostics().await.unwrap();
    assert_eq!(diagnostics.read_status, format!("{read:?}"));
    assert_eq!(diagnostics.write_status, format!("{write:?}"));
    if read == AccessStatus::Granted {
        assert_eq!(
            node.service.values().await.unwrap(),
            node.service.provider.values()
        );
    } else {
        match node.service.values().await {
            Err(RuntimeError::ReadClosed(status)) => assert_eq!(status, read),
            Err(RuntimeError::Closed) => assert_eq!(read, AccessStatus::NotPrimary),
            result => panic!("service read crossed stage denial: {result:?}"),
        }
    }
    let state = node.service.state.read().await.clone().unwrap();
    let data = Bytes::from_static(b"stage-raw-write");
    match state.begin_replicate(data.clone()) {
        Ok(assigned) => {
            assert_eq!(
                write,
                AccessStatus::Granted,
                "raw assignment crossed a stage denial"
            );
            let lsn = assigned.lsn;
            tokio::time::timeout(Duration::from_secs(10), assigned.completion)
                .await
                .unwrap()
                .unwrap();
            node.service.provider.commit(lsn, data).unwrap();
        }
        Err(RuntimeError::WriteClosed(status)) => assert_eq!(status, write),
        Err(error) => {
            assert_ne!(write, AccessStatus::Granted);
            if !matches!(error, RuntimeError::Closed) {
                assert_eq!(
                    error.to_string(),
                    format!("control-plane error: partition write access: {write:?}")
                );
            }
        }
    }
}

#[tokio::test]
async fn selected_delayed_old_reservation_cannot_replace_current_promotion_owner() {
    use AccessStatus::*;
    let root = root();
    let mut pair = Pair::start(root.path(), 1).await;
    pair.source.stop().await;
    let target = &pair.target;
    let epoch = Epoch::new(0, 2);
    target
        .command(PublicOperationProgram::Role {
            epoch,
            role: ReplicaRole::Primary,
        })
        .await
        .unwrap();
    target
        .command(PublicOperationProgram::Epoch { epoch })
        .await
        .unwrap();
    let current = PublicConfiguration {
        current: ConfigurationDescriptor::new(
            epoch,
            target.identity.replica_id,
            vec![ConfigurationMember {
                identity: target.identity.clone(),
                role: ReplicaRole::Primary,
            }],
            1,
        ),
        previous: None,
        replicas: vec![],
    };
    target
        .command(PublicOperationProgram::Configuration(current.clone()))
        .await
        .unwrap();
    let old_cut = hold(target, Cut::BeforeAccessReservation, 0);
    let old = target
        .intent(PublicOperationProgram::Configuration(current.clone()))
        .await;
    let mut old_task = execute(target, old.clone());
    entered(&old_cut, &mut old_task).await;

    let current_cut = hold(target, Cut::InstructionApplied, 1);
    let current_intent = target
        .intent(PublicOperationProgram::CatchUp {
            configuration: current,
            mode: PublicCatchUpMode::All,
        })
        .await;
    assert!(current_intent.revision > old.revision);
    let current_id = current_intent.operation_id.clone();
    let mut current_task = rpc(target, current_intent).await;
    entered(&current_cut, &mut current_task).await;
    stage(target, Granted, ReconfigurationPending).await;

    old_cut.release.notify_one();
    assert!(
        tokio::time::timeout(Duration::from_secs(10), old_task)
            .await
            .unwrap()
            .unwrap()
            .is_err()
    );
    stage(target, Granted, ReconfigurationPending).await;
    finish(target, &current_cut, current_task).await;
    assert!(
        target
            .report()
            .await
            .completed_operations
            .iter()
            .any(|completion| {
                completion.operation_id == current_id
                    && completion.result == PublicV1OperationResult::Succeeded
            })
    );
    stage(target, Granted, Granted).await;
    assert!(target.handle().execute_public_v1(old).await.is_err());
    stage(target, Granted, Granted).await;
    pair.target.stop().await;
}

#[tokio::test]
async fn selected_old_generation_completion_cannot_release_new_no_quorum_fence() {
    use AccessStatus::*;
    let root = root();
    let pair = Pair::start(root.path(), 1).await;
    let input = configuration(&pair).await;
    let cut = hold(&pair.source, Cut::InstructionApplied, 1);
    let old = pair
        .source
        .intent(PublicOperationProgram::CatchUp {
            configuration: input.clone(),
            mode: PublicCatchUpMode::All,
        })
        .await;
    let mut task = execute(&pair.source, old.clone());
    entered(&cut, &mut task).await;
    stage(&pair.source, Granted, Granted).await;
    pair.source.handle().set_public_operation_cut(None);
    let mut unavailable = input;
    unavailable.replicas[0].is_up = false;
    pair.source
        .command(PublicOperationProgram::Configuration(unavailable))
        .await
        .unwrap();
    cut.release.notify_one();
    assert!(task.await.unwrap().is_err());
    assert!(pair.source.handle().execute_public_v1(old).await.is_err());
    stage(&pair.source, Granted, NoWriteQuorum).await;
    pair.stop().await;
}

#[tokio::test]
async fn selected_availability_exact_replay_and_old_descriptor_state_rejection() {
    let root = root();
    let mut pair = Pair::start(root.path(), 1).await;
    let program = PublicOperationProgram::Configuration(configuration(&pair).await);
    let intent = pair.source.intent(program).await;
    pair.source
        .handle()
        .execute_public_v1(intent.clone())
        .await
        .unwrap();
    pair.source
        .handle()
        .execute_public_v1(intent.clone())
        .await
        .unwrap();
    let mut changed = intent.clone();
    if let Some(PublicOperationProgram::Configuration(configuration)) = &mut changed.program {
        configuration.replicas[0].is_ready = false;
    }
    assert!(
        pair.source
            .handle()
            .execute_public_v1(changed.clone())
            .await
            .is_err()
    );
    changed.input_digest = changed.program.as_ref().unwrap().digest();
    assert!(
        pair.source
            .handle()
            .execute_public_v1(changed)
            .await
            .is_err()
    );
    pair.source.stop().await;
    let path = SqliteStore::metadata_database_path(&pair.source.root);
    let connection = rusqlite::Connection::open(&path).unwrap();
    let original: String = connection
        .query_row(
            "SELECT state_json FROM agent_state WHERE singleton=1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(SqliteStore::open_public_v1(&path, &pair.source.preview).is_ok());
    fn remove_field(value: &mut serde_json::Value, field: &str) -> usize {
        match value {
            serde_json::Value::Object(object) => {
                let removed = usize::from(
                    object.contains_key("mustCatchup") && object.remove(field).is_some(),
                );
                removed
                    + object
                        .values_mut()
                        .map(|value| remove_field(value, field))
                        .sum::<usize>()
            }
            serde_json::Value::Array(values) => values
                .iter_mut()
                .map(|value| remove_field(value, field))
                .sum(),
            _ => 0,
        }
    }
    for field in ["isUp", "isReady"] {
        let mut old: serde_json::Value = serde_json::from_str(&original).unwrap();
        assert!(remove_field(&mut old, field) > 0);
        connection
            .execute(
                "UPDATE agent_state SET state_json=?1 WHERE singleton=1",
                [serde_json::to_string(&old).unwrap()],
            )
            .unwrap();
        assert!(SqliteStore::open_public_v1(&path, &pair.source.preview).is_err());
    }
    let mut old: serde_json::Value = serde_json::from_str(&original).unwrap();
    old["publicV1OperationState"]["identity"]["protocolVersion"] = serde_json::json!(12);
    connection
        .execute(
            "UPDATE agent_state SET state_json=?1 WHERE singleton=1",
            [serde_json::to_string(&old).unwrap()],
        )
        .unwrap();
    assert!(SqliteStore::open_public_v1(&path, &pair.source.preview).is_err());
    connection
        .execute(
            "UPDATE agent_state SET state_json=?1 WHERE singleton=1",
            [original],
        )
        .unwrap();
    pair.target.stop().await;
}

#[tokio::test]
async fn selected_fault_and_terminal_override_stage_publication_and_exact_replay() {
    use AccessStatus::*;
    let root = root();
    for fault in [false, true] {
        let mut pair = Pair::start(&root.path().join(fault.to_string()), 1).await;
        let cut = hold(&pair.source, Cut::InstructionApplied, 1);
        let input = configuration(&pair).await;
        let old = pair
            .source
            .intent(PublicOperationProgram::CatchUp {
                configuration: input,
                mode: PublicCatchUpMode::All,
            })
            .await;
        let mut task = execute(&pair.source, old.clone());
        entered(&cut, &mut task).await;
        stage(&pair.source, Granted, Granted).await;
        pair.source.handle().set_public_operation_cut(None);
        if fault {
            pair.source
                .service
                .partition
                .read()
                .await
                .clone()
                .unwrap()
                .report_fault(FaultType::Transient)
                .await
                .unwrap();
        } else {
            pair.source
                .command(PublicOperationProgram::Abort)
                .await
                .unwrap();
        }
        cut.release.notify_one();
        assert!(task.await.unwrap().is_err());
        assert!(
            pair.source
                .handle()
                .execute_public_v1(old.clone())
                .await
                .is_err()
        );
        let mut stale = old;
        stale.identity.protocol_version = 12;
        assert!(pair.source.handle().execute_public_v1(stale).await.is_err());
        let report = pair.source.report().await;
        assert_eq!(
            (report.read_status, report.write_status),
            (NotPrimary, NotPrimary)
        );
        assert!(pair.source.service.values().await.is_err());
        let state = pair.source.service.state.read().await.clone().unwrap();
        assert!(
            state
                .begin_replicate(Bytes::from_static(b"stale-stage-write"))
                .is_err()
        );
        pair.source.stop().await;
        pair.target.stop().await;
    }
}

async fn configuration(pair: &Pair) -> PublicConfiguration {
    PublicConfiguration {
        current: pair.configuration.clone(),
        previous: None,
        replicas: vec![pair.target.descriptor(ReplicaRole::ActiveSecondary).await],
    }
}

#[tokio::test]
async fn selected_same_primary_joint_and_final_use_real_c_and_p() {
    use AccessStatus::*;
    let root = root();
    let pair = Pair::start(root.path(), 1).await;
    let mut joint = configuration(&pair).await;
    joint.previous = Some(joint.current.clone());
    pair.target
        .command(PublicOperationProgram::Configuration(PublicConfiguration {
            current: joint.current.clone(),
            previous: joint.previous.clone(),
            replicas: vec![pair.source.descriptor(ReplicaRole::Primary).await],
        }))
        .await
        .unwrap();
    let cut = hold(&pair.source, Cut::BeforeInstruction, 1);
    let intent = pair
        .source
        .intent(PublicOperationProgram::CatchUp {
            configuration: joint.clone(),
            mode: PublicCatchUpMode::All,
        })
        .await;
    let mut task = execute(&pair.source, intent);
    entered(&cut, &mut task).await;
    stage(&pair.source, Granted, Granted).await;
    finish(&pair.source, &cut, task).await;
    stage(&pair.source, Granted, Granted).await;
    let mut no_c = joint.clone();
    no_c.replicas[0].is_ready = false;
    pair.source
        .command(PublicOperationProgram::Configuration(no_c))
        .await
        .unwrap();
    stage(&pair.source, Granted, NoWriteQuorum).await;

    let resource = pair.source.history.resource_uid.clone();
    let mut previous_only = Node::start(&root.path().join("previous"), &resource, 3).await;
    previous_only.open(ReplicaRole::IdleSecondary, EPOCH).await;
    pair.source.build(&previous_only, &pair.configuration).await;
    previous_only
        .command(PublicOperationProgram::Role {
            epoch: EPOCH,
            role: ReplicaRole::ActiveSecondary,
        })
        .await
        .unwrap();
    let report = previous_only.report().await;
    let peer = PublicReplicaDescriptor {
        build_id: OperationId::default(),
        identity: previous_only.identity.clone(),
        replication_address: report.replication_address.unwrap(),
        process_session_id: report.process_session_id,
        storage_incarnation: Some(previous_only.storage.clone()),
        role: ReplicaRole::ActiveSecondary,
        current_progress: INVALID_LSN,
        catch_up_capability: INVALID_LSN,
        must_catchup: false,
        is_up: false,
        is_ready: false,
    };
    let mut no_p = joint;
    no_p.previous = Some(ConfigurationDescriptor::new(
        EPOCH,
        pair.source.identity.replica_id,
        vec![
            ConfigurationMember {
                identity: pair.source.identity.clone(),
                role: ReplicaRole::Primary,
            },
            ConfigurationMember {
                identity: previous_only.identity.clone(),
                role: ReplicaRole::ActiveSecondary,
            },
        ],
        1,
    ));
    no_p.replicas.push(peer);
    // Finish the prior joint configuration before selecting a different previous electorate.
    pair.source
        .command(PublicOperationProgram::Configuration(
            configuration(&pair).await,
        ))
        .await
        .unwrap();
    pair.source
        .command(PublicOperationProgram::Configuration(no_p.clone()))
        .await
        .unwrap();
    stage(&pair.source, Granted, NoWriteQuorum).await;
    let cut = hold(&pair.source, Cut::BeforeInstruction, 1);
    let pending = pair
        .source
        .intent(PublicOperationProgram::CatchUp {
            configuration: no_p,
            mode: PublicCatchUpMode::All,
        })
        .await;
    let mut pending_task = execute(&pair.source, pending.clone());
    entered(&cut, &mut pending_task).await;
    stage(&pair.source, Granted, NoWriteQuorum).await;
    pair.source.handle().set_public_operation_cut(None);
    let final_input = configuration(&pair).await;
    pair.target
        .command(PublicOperationProgram::Configuration(PublicConfiguration {
            current: pair.configuration.clone(),
            previous: None,
            replicas: vec![pair.source.descriptor(ReplicaRole::Primary).await],
        }))
        .await
        .unwrap();
    pair.source
        .command(PublicOperationProgram::Configuration(final_input))
        .await
        .unwrap();
    cut.release.notify_one();
    assert!(pending_task.await.unwrap().is_err());
    assert!(
        pair.source
            .handle()
            .execute_public_v1(pending)
            .await
            .is_err()
    );
    stage(&pair.source, Granted, Granted).await;
    previous_only.stop().await;
    pair.stop().await;
}

#[tokio::test]
async fn selected_promotion_waits_for_replicator_role_before_any_grant() {
    use AccessStatus::*;
    let root = root();
    let mut pair = Pair::start(root.path(), 1).await;
    pair.source.stop().await;
    let cut = hold(&pair.target, Cut::BeforeInstruction, 0);
    let intent = pair
        .target
        .intent(PublicOperationProgram::Role {
            epoch: Epoch::new(0, 2),
            role: ReplicaRole::Primary,
        })
        .await;
    let mut task = rpc(&pair.target, intent).await;
    entered(&cut, &mut task).await;
    stage(&pair.target, ReconfigurationPending, ReconfigurationPending).await;
    finish(&pair.target, &cut, task).await;
    stage(&pair.target, ReconfigurationPending, ReconfigurationPending).await;
    pair.target.stop().await;
}

#[tokio::test]
async fn selected_split_promotion_rpc_catchup_activation_survives_observer_drop() {
    use AccessStatus::*;
    let root = root();
    let mut pair = Pair::start(root.path(), 1).await;
    pair.source.stop().await;
    let target = &pair.target;
    let epoch = Epoch::new(0, 2);
    let cut = hold(target, Cut::BeforeInstruction, 1);
    let intent = target
        .intent(PublicOperationProgram::Role {
            epoch,
            role: ReplicaRole::Primary,
        })
        .await;
    let mut task = rpc(target, intent).await;
    entered(&cut, &mut task).await;
    stage(target, ReconfigurationPending, ReconfigurationPending).await;
    finish(target, &cut, task).await;
    target
        .command(PublicOperationProgram::Epoch { epoch })
        .await
        .unwrap();
    let current = PublicConfiguration {
        current: ConfigurationDescriptor::new(
            epoch,
            target.identity.replica_id,
            vec![ConfigurationMember {
                identity: target.identity.clone(),
                role: ReplicaRole::Primary,
            }],
            1,
        ),
        previous: None,
        replicas: vec![],
    };
    target
        .command(PublicOperationProgram::Configuration(current.clone()))
        .await
        .unwrap();
    stage(target, ReconfigurationPending, ReconfigurationPending).await;
    let cut = hold(target, Cut::InstructionApplied, 1);
    let intent = target
        .intent(PublicOperationProgram::CatchUp {
            configuration: current.clone(),
            mode: PublicCatchUpMode::All,
        })
        .await;
    let mut task = rpc(target, intent).await;
    entered(&cut, &mut task).await;
    stage(target, Granted, ReconfigurationPending).await;
    let id = target
        .report()
        .await
        .active_operations
        .iter()
        .find(|intent| matches!(intent.program, Some(PublicOperationProgram::CatchUp { .. })))
        .unwrap()
        .operation_id
        .clone();
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    target.handle().set_public_operation_cut(None);
    cut.release.notify_one();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let report = wire_report(target).await;
            if report
                .completed_operations
                .iter()
                .any(|completion| completion.operation_id == id)
                && report.read_status == Granted
                && report.write_status == Granted
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    stage(target, Granted, Granted).await;
    pair.target.stop().await;
}

#[tokio::test]
async fn selected_recipe_promotion_reads_after_catchup_before_access() {
    use AccessStatus::*;
    let root = root();
    let mut pair = Pair::start(root.path(), 1).await;
    let target = &pair.target;
    let epoch = Epoch::new(0, 2);
    pair.source
        .command(PublicOperationProgram::Role {
            epoch,
            role: ReplicaRole::ActiveSecondary,
        })
        .await
        .unwrap();
    pair.source
        .command(PublicOperationProgram::Epoch { epoch })
        .await
        .unwrap();
    let current = ConfigurationDescriptor::new(
        epoch,
        target.identity.replica_id,
        vec![
            ConfigurationMember {
                identity: target.identity.clone(),
                role: ReplicaRole::Primary,
            },
            ConfigurationMember {
                identity: pair.source.identity.clone(),
                role: ReplicaRole::ActiveSecondary,
            },
        ],
        2,
    );
    pair.source
        .command(PublicOperationProgram::Configuration(PublicConfiguration {
            current: current.clone(),
            previous: Some(pair.configuration.clone()),
            replicas: vec![target.descriptor(ReplicaRole::Primary).await],
        }))
        .await
        .unwrap();
    let input = PublicLifecycleInput {
        recipe: PublicLifecycleRecipe::FailoverPromotion,
        replica: target.identity.clone(),
        epoch,
        possible_data_loss: PossibleDataLossIntent::NotPossible,
        current,
        previous: Some(pair.configuration.clone()),
        replicas: vec![pair.source.descriptor(ReplicaRole::ActiveSecondary).await],
        data_loss_recovery: None,
    };
    let mut intent = target.intent(PublicOperationProgram::Epoch { epoch }).await;
    intent.program = None;
    intent.input_digest = input.digest();
    intent.lifecycle = Some(input);
    let cut = hold(target, Cut::BeforeInstruction, 5);
    let mut task = execute(target, intent);
    entered(&cut, &mut task).await;
    stage(target, Granted, ReconfigurationPending).await;
    finish(target, &cut, task).await;
    stage(target, Granted, Granted).await;
    pair.source.stop().await;
    pair.target.stop().await;
}

#[tokio::test]
async fn selected_split_joint_promotion_keeps_reads_until_matching_final_activation() {
    use AccessStatus::*;
    let root = root();
    let pair = Pair::start(root.path(), 1).await;
    let epoch = Epoch::new(0, 2);
    pair.source
        .command(PublicOperationProgram::Role {
            epoch,
            role: ReplicaRole::ActiveSecondary,
        })
        .await
        .unwrap();
    pair.source
        .command(PublicOperationProgram::Epoch { epoch })
        .await
        .unwrap();
    pair.target
        .command(PublicOperationProgram::Role {
            epoch,
            role: ReplicaRole::Primary,
        })
        .await
        .unwrap();
    pair.target
        .command(PublicOperationProgram::Epoch { epoch })
        .await
        .unwrap();
    let current = ConfigurationDescriptor::new(
        epoch,
        pair.target.identity.replica_id,
        vec![
            ConfigurationMember {
                identity: pair.target.identity.clone(),
                role: ReplicaRole::Primary,
            },
            ConfigurationMember {
                identity: pair.source.identity.clone(),
                role: ReplicaRole::ActiveSecondary,
            },
        ],
        2,
    );
    pair.source
        .command(PublicOperationProgram::Configuration(PublicConfiguration {
            current: current.clone(),
            previous: Some(pair.configuration.clone()),
            replicas: vec![pair.target.descriptor(ReplicaRole::Primary).await],
        }))
        .await
        .unwrap();
    let joint = PublicConfiguration {
        current: current.clone(),
        previous: Some(pair.configuration.clone()),
        replicas: vec![pair.source.descriptor(ReplicaRole::ActiveSecondary).await],
    };
    let cut = hold(&pair.target, Cut::BeforeInstruction, 1);
    let intent = pair
        .target
        .intent(PublicOperationProgram::CatchUp {
            configuration: joint.clone(),
            mode: PublicCatchUpMode::All,
        })
        .await;
    let mut task = rpc(&pair.target, intent).await;
    entered(&cut, &mut task).await;
    stage(&pair.target, ReconfigurationPending, ReconfigurationPending).await;
    finish(&pair.target, &cut, task).await;
    stage(&pair.target, Granted, ReconfigurationPending).await;
    pair.source
        .command(PublicOperationProgram::Configuration(PublicConfiguration {
            current,
            previous: None,
            replicas: vec![pair.target.descriptor(ReplicaRole::Primary).await],
        }))
        .await
        .unwrap();
    let mut final_configuration = joint;
    final_configuration.previous = None;
    pair.target
        .command(PublicOperationProgram::Configuration(final_configuration))
        .await
        .unwrap();
    stage(&pair.target, Granted, Granted).await;
    pair.stop().await;
}

#[tokio::test]
async fn selected_swap_cancelled_pre_and_post_revoke_never_replays_authority() {
    use AccessStatus::*;
    let root = root();
    for post_revoke in [false, true] {
        let pair = Pair::start(&root.path().join(post_revoke.to_string()), 1).await;
        let epoch = Epoch::new(0, 2);
        let mut starting = configuration(&pair).await;
        starting.replicas[0].must_catchup = true;
        let mut refreshed = starting.clone();
        refreshed.current = ConfigurationDescriptor::new(
            epoch,
            pair.source.identity.replica_id,
            pair.configuration.members.clone(),
            2,
        );
        if post_revoke {
            pair.target
                .command(PublicOperationProgram::Epoch { epoch })
                .await
                .unwrap();
            pair.target
                .command(PublicOperationProgram::Configuration(PublicConfiguration {
                    current: refreshed.current.clone(),
                    previous: None,
                    replicas: vec![pair.source.descriptor(ReplicaRole::Primary).await],
                }))
                .await
                .unwrap();
        }
        let intent = pair
            .source
            .intent(PublicOperationProgram::Swap {
                starting,
                refreshed,
                epoch,
                handoff: ReplicaRole::ActiveSecondary,
                catch_up_specific_quorum: true,
                mode: PublicCatchUpMode::WriteQuorum,
                controller_composed: false,
            })
            .await;
        let cut = hold(
            &pair.source,
            Cut::BeforeInstruction,
            if post_revoke { 5 } else { 1 },
        );
        let mut task = execute(&pair.source, intent.clone());
        entered(&cut, &mut task).await;
        stage(
            &pair.source,
            Granted,
            if post_revoke {
                ReconfigurationPending
            } else {
                Granted
            },
        )
        .await;
        pair.source.handle().set_public_operation_cut(None);
        pair.source
            .command(PublicOperationProgram::Abort)
            .await
            .unwrap();
        cut.release.notify_one();
        assert!(task.await.unwrap().is_err());
        assert!(
            pair.source
                .handle()
                .execute_public_v1(intent)
                .await
                .is_err()
        );
        let report = pair.source.report().await;
        assert_eq!(
            (report.read_status, report.write_status),
            (NotPrimary, NotPrimary)
        );
        assert!(pair.source.service.values().await.is_err());
        assert!(
            pair.source
                .service
                .state
                .read()
                .await
                .clone()
                .unwrap()
                .begin_replicate(Bytes::new())
                .is_err()
        );
        pair.stop().await;
    }
}

#[tokio::test]
async fn selected_swap_standalone_and_composed_current_and_joint_stages() {
    use AccessStatus::*;
    let root = root();
    for composed in [false, true] {
        for joint in [false, true] {
            // Each cut gets a fresh actual host; no cut can borrow future receipts.
            for (position, index, expected_read, expected_write) in [
                (
                    Cut::BeforeInstruction,
                    if composed { 0 } else { 1 },
                    Granted,
                    if composed {
                        ReconfigurationPending
                    } else {
                        Granted
                    },
                ),
                (
                    Cut::BeforeInstruction,
                    if composed { 3 } else { 5 },
                    Granted,
                    ReconfigurationPending,
                ),
                (
                    Cut::CallbackSuccess,
                    if composed { 3 } else { 5 },
                    NotPrimary,
                    NotPrimary,
                ),
                (
                    Cut::BeforeInstruction,
                    if composed { 4 } else { 6 },
                    NotPrimary,
                    NotPrimary,
                ),
            ] {
                let directory = root
                    .path()
                    .join(format!("{composed}-{joint}-{position:?}-{index}"));
                let pair = Pair::start(&directory, 1).await;
                let mut starting = configuration(&pair).await;
                starting.replicas[0].must_catchup = true;
                if joint {
                    starting.previous = Some(starting.current.clone());
                }
                if joint {
                    pair.target
                        .command(PublicOperationProgram::Configuration(PublicConfiguration {
                            current: starting.current.clone(),
                            previous: starting.previous.clone(),
                            replicas: vec![pair.source.descriptor(ReplicaRole::Primary).await],
                        }))
                        .await
                        .unwrap();
                }
                if composed {
                    pair.source
                        .command(PublicOperationProgram::CatchUp {
                            configuration: starting.clone(),
                            mode: PublicCatchUpMode::All,
                        })
                        .await
                        .unwrap();
                }
                let epoch = Epoch::new(0, 2);
                let mut refreshed = starting.clone();
                refreshed.current = ConfigurationDescriptor::new(
                    epoch,
                    pair.source.identity.replica_id,
                    pair.configuration.members.clone(),
                    2,
                );
                let target_configuration = PublicConfiguration {
                    current: refreshed.current.clone(),
                    previous: refreshed.previous.clone(),
                    replicas: vec![pair.source.descriptor(ReplicaRole::Primary).await],
                };
                let prepare_before = expected_write != Granted;
                if prepare_before {
                    pair.target
                        .command(PublicOperationProgram::Epoch { epoch })
                        .await
                        .unwrap();
                    pair.target
                        .command(PublicOperationProgram::Configuration(
                            target_configuration.clone(),
                        ))
                        .await
                        .unwrap();
                }
                let intent = pair
                    .source
                    .intent(PublicOperationProgram::Swap {
                        starting,
                        refreshed,
                        epoch,
                        handoff: ReplicaRole::ActiveSecondary,
                        catch_up_specific_quorum: true,
                        mode: PublicCatchUpMode::WriteQuorum,
                        controller_composed: composed,
                    })
                    .await;
                let replay = intent.clone();
                let cut = hold(&pair.source, position, index);
                let mut task = execute(&pair.source, intent);
                entered(&cut, &mut task).await;
                stage(&pair.source, expected_read, expected_write).await;
                if !prepare_before {
                    pair.target
                        .command(PublicOperationProgram::Epoch { epoch })
                        .await
                        .unwrap();
                    pair.target
                        .command(PublicOperationProgram::Configuration(target_configuration))
                        .await
                        .unwrap();
                }
                finish(&pair.source, &cut, task).await;
                stage(&pair.source, NotPrimary, NotPrimary).await;
                pair.source
                    .handle()
                    .execute_public_v1(replay)
                    .await
                    .unwrap();
                stage(&pair.source, NotPrimary, NotPrimary).await;
                pair.stop().await;
            }
        }
    }
}
