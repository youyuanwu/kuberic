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
    stage_with_completion(node, read, write, true).await;
}

async fn stage_with_completion(
    node: &Node,
    read: AccessStatus,
    write: AccessStatus,
    complete_write: bool,
) {
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
            if complete_write {
                let lsn = assigned.lsn;
                tokio::time::timeout(Duration::from_secs(10), assigned.completion)
                    .await
                    .unwrap()
                    .unwrap();
                node.service.provider.commit(lsn, data).unwrap();
            }
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

// A new joint stage follows a new message epoch, not the completed Current
// stage of Pair::start. Advance the real children and target transport authority;
// leave the source joint callback to the operation under test.
async fn next_joint(
    pair: &mut Pair,
    previous: ConfigurationDescriptor,
    previous_peers: Vec<PublicReplicaDescriptor>,
) -> PublicConfiguration {
    let epoch = Epoch::new(
        pair.configuration.epoch.data_loss_number,
        pair.configuration.epoch.configuration_number + 1,
    );
    pair.configuration = ConfigurationDescriptor::new(
        epoch,
        pair.configuration.primary_id,
        pair.configuration.members.clone(),
        pair.configuration.write_quorum,
    );
    for node in [&pair.source, &pair.target] {
        node.command(PublicOperationProgram::Epoch { epoch })
            .await
            .unwrap();
    }
    let mut target_peers = vec![pair.source.descriptor(ReplicaRole::Primary).await];
    target_peers.extend(previous_peers.iter().cloned());
    pair.target
        .command(PublicOperationProgram::Configuration(PublicConfiguration {
            current: pair.configuration.clone(),
            previous: Some(previous.clone()),
            replicas: target_peers,
        }))
        .await
        .unwrap();
    let mut input = PublicConfiguration {
        previous: Some(previous),
        ..configuration(pair).await
    };
    input.replicas.extend(previous_peers);
    input
}

// Standalone Swap really catches up the starting authority before UpdateEpoch.
// Moving the peer first invalidates that live channel and deadlocks its quorum.
// Prepare the peer only after the source's successful native epoch callback;
// the second wait then uses matching real transport authority on both hosts.
async fn advance_swap_peer(
    pair: &Pair,
    configuration: PublicConfiguration,
) -> tokio::task::JoinHandle<()> {
    let observed = pair.source.service.observed.clone();
    let target = pair.target.handle();
    let epoch = configuration.current.epoch;
    let epoch_intent = pair
        .target
        .intent(PublicOperationProgram::Epoch { epoch })
        .await;
    let mut configuration_intent = pair
        .target
        .intent(PublicOperationProgram::Configuration(configuration))
        .await;
    configuration_intent.revision = epoch_intent.revision + 1;
    tokio::spawn(async move {
        eventually(|| {
            observed
                .lock()
                .unwrap()
                .iter()
                .any(|call| matches!(call, NativeCall::Epoch(applied) if *applied == epoch))
        })
        .await;
        target.execute_public_v1(epoch_intent).await.unwrap();
        target
            .execute_public_v1(configuration_intent)
            .await
            .unwrap();
    })
}

async fn planner_primary_configuration(pair: &Pair) -> PublicConfiguration {
    use kuberic_controller::evaluator::public_v1_topology::{
        PublicV1TopologyPlan, PublicV1TopologyReplica, plan_public_v1_topology,
    };
    for _ in 0..16 {
        let mut replicas = Vec::new();
        for node in [&pair.source, &pair.target] {
            let report = wire_report(node).await;
            report.validate().unwrap();
            replicas.push(PublicV1TopologyReplica {
                replication_address: report.replication_address.clone().unwrap(),
                report,
            });
        }
        let accepted = PublicV1AcceptedStatus {
            identity: pair.source.preview.clone(),
            history_initialization_id: derive_history_initialization_id(
                &pair.source.history.resource_uid,
                PublicProtocolGeneration::new(1),
            ),
            history: pair.source.history.clone(),
            data_loss_recovery: None,
            completed_operations: replicas
                .iter()
                .flat_map(|replica| &replica.report.completed_operations)
                .map(PublicV1AcceptedOperationCompletion::from_completion)
                .collect(),
            extensions: Default::default(),
        };
        let plan = plan_public_v1_topology(
            &accepted,
            Some(&pair.configuration),
            &replicas,
            2,
            None,
            Some(pair.source.identity.replica_id),
            "selected-availability",
        )
        .unwrap();
        let PublicV1TopologyPlan::Begin { command, .. } = plan else {
            panic!("planner did not publish required availability work: {plan:?}");
        };
        let program = command.intent.program.unwrap();
        if command.target == pair.source.identity {
            return match program {
                PublicOperationProgram::Configuration(input)
                | PublicOperationProgram::CatchUp {
                    configuration: input,
                    ..
                } => input,
                other => panic!("primary availability work was not a configuration: {other:?}"),
            };
        }
        assert_eq!(command.target, pair.target.identity);
        pair.target.command(program).await.unwrap();
    }
    panic!("planner never reached primary availability work");
}

#[tokio::test]
async fn selected_planner_unknown_progress_and_restoration_drive_real_c_and_p() {
    use AccessStatus::*;
    let root = root();
    for (missing_current, missing_capability) in [(true, false), (false, true), (true, true)] {
        let pair = Pair::start(
            &root
                .path()
                .join(format!("{missing_current}-{missing_capability}")),
            1,
        )
        .await;
        pair.target
            .service
            .provider
            .query_current
            .store(u64::from(missing_current), Ordering::Release);
        pair.target
            .service
            .provider
            .query_capability
            .store(u64::from(missing_capability), Ordering::Release);
        let unavailable = planner_primary_configuration(&pair).await;
        let peer = &unavailable.replicas[0];
        assert!(peer.is_up && !peer.is_ready);
        assert_eq!(peer.current_progress == INVALID_LSN, missing_current);
        assert_eq!(peer.catch_up_capability == INVALID_LSN, missing_capability);
        let cut = hold(&pair.source, Cut::InstructionApplied, 0);
        let intent = pair
            .source
            .intent(PublicOperationProgram::Configuration(unavailable.clone()))
            .await;
        let mut task = rpc(&pair.source, intent).await;
        entered(&cut, &mut task).await;
        stage(&pair.source, Granted, NoWriteQuorum).await;
        finish(&pair.source, &cut, task).await;
        stage(&pair.source, Granted, NoWriteQuorum).await;

        pair.target
            .service
            .provider
            .query_current
            .store(0, Ordering::Release);
        pair.target
            .service
            .provider
            .query_capability
            .store(0, Ordering::Release);
        let restored = planner_primary_configuration(&pair).await;
        assert!(restored.replicas[0].is_up && restored.replicas[0].is_ready);
        let cut = hold(&pair.source, Cut::InstructionApplied, 0);
        let intent = pair
            .source
            .intent(PublicOperationProgram::Configuration(restored.clone()))
            .await;
        let mut task = rpc(&pair.source, intent).await;
        entered(&cut, &mut task).await;
        // Restored descriptor readiness authorizes assignment, not transport completion.
        stage_with_completion(&pair.source, Granted, Granted, false).await;
        finish(&pair.source, &cut, task).await;

        let epoch = Epoch::new(0, 2);
        pair.source
            .command(PublicOperationProgram::Epoch { epoch })
            .await
            .unwrap();
        let mut previous_only = PublicConfiguration {
            current: ConfigurationDescriptor::new(
                epoch,
                pair.source.identity.replica_id,
                vec![ConfigurationMember {
                    identity: pair.source.identity.clone(),
                    role: ReplicaRole::Primary,
                }],
                1,
            ),
            previous: Some(pair.configuration.clone()),
            replicas: unavailable.replicas,
        };
        let cut = hold(&pair.source, Cut::InstructionApplied, 0);
        let intent = pair
            .source
            .intent(PublicOperationProgram::Configuration(previous_only.clone()))
            .await;
        let mut task = rpc(&pair.source, intent).await;
        entered(&cut, &mut task).await;
        // C is a ready local singleton; only the produced unavailable previous voter denies P.
        stage(&pair.source, Granted, NoWriteQuorum).await;
        finish(&pair.source, &cut, task).await;
        pair.target
            .command(PublicOperationProgram::Epoch { epoch })
            .await
            .unwrap();
        pair.target
            .command(PublicOperationProgram::Configuration(PublicConfiguration {
                current: previous_only.current.clone(),
                previous: previous_only.previous.clone(),
                replicas: vec![pair.source.descriptor(ReplicaRole::Primary).await],
            }))
            .await
            .unwrap();
        previous_only.replicas = restored.replicas;
        pair.source
            .command(PublicOperationProgram::Configuration(previous_only.clone()))
            .await
            .unwrap();
        stage_with_completion(&pair.source, Granted, Granted, false).await;
        previous_only.previous = None;
        previous_only.replicas.clear();
        pair.source
            .command(PublicOperationProgram::Configuration(previous_only))
            .await
            .unwrap();
        stage_with_completion(&pair.source, Granted, Granted, false).await;
        pair.stop().await;
    }
}

#[tokio::test]
async fn selected_changed_primary_context_cannot_borrow_same_primary_grants() {
    use AccessStatus::*;
    let root = root();
    for catch_up in [false, true] {
        let mut pair = Pair::start(&root.path().join(catch_up.to_string()), 1).await;
        let previous = ConfigurationDescriptor::new(
            Epoch::new(0, 0),
            pair.target.identity.replica_id,
            vec![
                ConfigurationMember {
                    identity: pair.source.identity.clone(),
                    role: ReplicaRole::ActiveSecondary,
                },
                ConfigurationMember {
                    identity: pair.target.identity.clone(),
                    role: ReplicaRole::Primary,
                },
            ],
            2,
        );
        let changed = next_joint(&mut pair, previous, Vec::new()).await;
        changed.validate().unwrap();
        let program = if catch_up {
            PublicOperationProgram::CatchUp {
                configuration: changed,
                mode: PublicCatchUpMode::All,
            }
        } else {
            PublicOperationProgram::Configuration(changed)
        };
        let cut = hold(&pair.source, Cut::InstructionApplied, 0);
        let old = pair.source.intent(program).await;
        let mut task = rpc(&pair.source, old.clone()).await;
        entered(&cut, &mut task).await;
        stage(&pair.source, ReconfigurationPending, ReconfigurationPending).await;
        pair.source.handle().set_public_operation_cut(None);
        pair.target
            .command(PublicOperationProgram::Configuration(PublicConfiguration {
                current: pair.configuration.clone(),
                previous: None,
                replicas: vec![pair.source.descriptor(ReplicaRole::Primary).await],
            }))
            .await
            .unwrap();
        if catch_up {
            // The successor replaces the uncertain wait, not its missing promotion evidence.
            pair.source
                .command(PublicOperationProgram::Configuration(
                    configuration(&pair).await,
                ))
                .await
                .unwrap();
            cut.release.notify_one();
            assert!(task.await.unwrap().is_err());
        } else {
            cut.release.notify_one();
            task.await.unwrap().unwrap();
            stage(&pair.source, ReconfigurationPending, ReconfigurationPending).await;
            pair.source
                .command(PublicOperationProgram::Configuration(
                    configuration(&pair).await,
                ))
                .await
                .unwrap();
        }
        stage(&pair.source, Granted, Granted).await;
        assert_eq!(
            pair.source.handle().execute_public_v1(old).await.is_err(),
            catch_up
        );
        stage(&pair.source, Granted, Granted).await;
        pair.stop().await;
    }
}

type HeldDemotion = (
    Arc<PublicOperationCut>,
    tokio::task::JoinHandle<kuberic_runtime::host::Result<()>>,
    PublicOperationIntent,
);

#[tokio::test]
async fn selected_changed_primary_context_after_revoke_cannot_borrow_demotion_reads() {
    use AccessStatus::*;
    let root = root();
    let pair = Pair::start(root.path(), 1).await;
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
    let peer = advance_swap_peer(
        &pair,
        PublicConfiguration {
            current: refreshed.current.clone(),
            previous: None,
            replicas: vec![pair.source.descriptor(ReplicaRole::Primary).await],
        },
    )
    .await;
    let mut final_configuration = refreshed.clone();
    final_configuration.replicas[0].must_catchup = false;
    let old_cut = hold(&pair.source, Cut::BeforeInstruction, 5);
    let swap = pair
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
    let mut old_task = execute(&pair.source, swap);
    entered(&old_cut, &mut old_task).await;
    peer.await.unwrap();
    stage(&pair.source, Granted, ReconfigurationPending).await;
    let mut changed = final_configuration.clone();
    changed.previous = Some(ConfigurationDescriptor::new(
        EPOCH,
        pair.target.identity.replica_id,
        vec![
            ConfigurationMember {
                identity: pair.source.identity.clone(),
                role: ReplicaRole::ActiveSecondary,
            },
            ConfigurationMember {
                identity: pair.target.identity.clone(),
                role: ReplicaRole::Primary,
            },
        ],
        2,
    ));
    changed.validate().unwrap();
    let cut = hold(&pair.source, Cut::InstructionApplied, 0);
    let intent = pair
        .source
        .intent(PublicOperationProgram::Configuration(changed))
        .await;
    let mut task = rpc(&pair.source, intent).await;
    entered(&cut, &mut task).await;
    stage(&pair.source, ReconfigurationPending, ReconfigurationPending).await;
    old_cut.release.notify_one();
    assert!(old_task.await.unwrap().is_err());
    stage(&pair.source, ReconfigurationPending, ReconfigurationPending).await;
    finish(&pair.source, &cut, task).await;
    stage(&pair.source, ReconfigurationPending, ReconfigurationPending).await;
    pair.source
        .command(PublicOperationProgram::Configuration(final_configuration))
        .await
        .unwrap();
    stage(&pair.source, Granted, Granted).await;
    pair.stop().await;
}

async fn demote_before_repromotion(
    pair: &Pair,
    composed: bool,
    demotion_cut: Option<Cut>,
) -> (ConfigurationDescriptor, Option<HeldDemotion>) {
    let mut starting = configuration(pair).await;
    starting.replicas[0].must_catchup = true;
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
    let peer = advance_swap_peer(
        pair,
        PublicConfiguration {
            current: refreshed.current.clone(),
            previous: None,
            replicas: vec![pair.source.descriptor(ReplicaRole::Primary).await],
        },
    )
    .await;
    let previous = refreshed.current.clone();
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
    let held = if let Some(position) = demotion_cut {
        let cut = hold(&pair.source, position, if composed { 4 } else { 6 });
        let mut task = execute(&pair.source, intent.clone());
        entered(&cut, &mut task).await;
        pair.source.handle().set_public_operation_cut(None);
        assert_eq!(pair.source.report().await.role, ReplicaRole::Primary);
        Some((cut, task, intent))
    } else {
        pair.source
            .handle()
            .execute_public_v1(intent)
            .await
            .unwrap();
        assert_eq!(
            pair.source.report().await.role,
            ReplicaRole::ActiveSecondary
        );
        None
    };
    peer.await.unwrap();
    stage(
        &pair.source,
        AccessStatus::NotPrimary,
        AccessStatus::NotPrimary,
    )
    .await;
    (previous, held)
}

async fn release_old_demotion(node: &Node, held: Option<HeldDemotion>) {
    if let Some((cut, task, intent)) = held {
        cut.release.notify_one();
        assert!(
            tokio::time::timeout(Duration::from_secs(10), task)
                .await
                .unwrap()
                .unwrap()
                .is_err()
        );
        assert!(node.handle().execute_public_v1(intent).await.is_err());
    }
}

async fn prepare_repromotion(
    pair: &Pair,
    previous: Option<ConfigurationDescriptor>,
) -> PublicConfiguration {
    prepare_repromotion_at(pair, previous, Epoch::new(0, 3)).await
}

async fn prepare_repromotion_at(
    pair: &Pair,
    previous: Option<ConfigurationDescriptor>,
    epoch: Epoch,
) -> PublicConfiguration {
    let current = ConfigurationDescriptor::new(
        epoch,
        pair.source.identity.replica_id,
        pair.configuration.members.clone(),
        2,
    );
    pair.target
        .command(PublicOperationProgram::Epoch { epoch })
        .await
        .unwrap();
    pair.target
        .command(PublicOperationProgram::Configuration(PublicConfiguration {
            current: current.clone(),
            previous: previous.clone(),
            replicas: vec![pair.source.descriptor(ReplicaRole::Primary).await],
        }))
        .await
        .unwrap();
    PublicConfiguration {
        current,
        previous,
        replicas: vec![pair.target.descriptor(ReplicaRole::ActiveSecondary).await],
    }
}

async fn repromotion_intent(
    node: &Node,
    configuration: &PublicConfiguration,
    recipe: bool,
) -> PublicOperationIntent {
    let epoch = configuration.current.epoch;
    if !recipe {
        return node
            .intent(PublicOperationProgram::Role {
                epoch,
                role: ReplicaRole::Primary,
            })
            .await;
    }
    let input = PublicLifecycleInput {
        recipe: PublicLifecycleRecipe::FailoverPromotion,
        replica: node.identity.clone(),
        epoch,
        possible_data_loss: PossibleDataLossIntent::NotPossible,
        current: configuration.current.clone(),
        previous: configuration.previous.clone(),
        replicas: configuration.replicas.clone(),
        data_loss_recovery: None,
    };
    input.validate().unwrap();
    let mut intent = node.intent(PublicOperationProgram::Epoch { epoch }).await;
    intent.program = None;
    intent.input_digest = input.digest();
    intent.lifecycle = Some(input);
    intent
}

#[tokio::test]
async fn selected_nonactivating_configuration_preserves_demotion_reconciliation() {
    use AccessStatus::*;
    let root = root();
    for recipe in [false, true] {
        for composed in [false, true] {
            for demotion_cut in [Cut::BeforeInstruction, Cut::InstructionSelected] {
                let pair = Pair::start(
                    &root
                        .path()
                        .join(format!("{recipe}-{composed}-{demotion_cut:?}")),
                    1,
                )
                .await;
                let (previous, old) =
                    demote_before_repromotion(&pair, composed, Some(demotion_cut)).await;
                let refresh = PublicConfiguration {
                    current: previous.clone(),
                    previous: None,
                    replicas: vec![pair.target.descriptor(ReplicaRole::ActiveSecondary).await],
                };
                let refresh = pair
                    .source
                    .intent(PublicOperationProgram::Configuration(refresh))
                    .await;
                rpc(&pair.source, refresh).await.await.unwrap().unwrap();
                release_old_demotion(&pair.source, old).await;
                stage(&pair.source, NotPrimary, NotPrimary).await;

                let current = prepare_repromotion(&pair, recipe.then_some(previous)).await;
                let intent = repromotion_intent(&pair.source, &current, recipe).await;
                let role_cut = hold(
                    &pair.source,
                    Cut::BeforeInstruction,
                    if recipe { 2 } else { 1 },
                );
                let mut task = rpc(&pair.source, intent).await;
                entered(&role_cut, &mut task).await;
                stage(&pair.source, ReconfigurationPending, ReconfigurationPending).await;
                finish(&pair.source, &role_cut, task).await;
                if !recipe {
                    pair.source
                        .command(PublicOperationProgram::Epoch {
                            epoch: current.current.epoch,
                        })
                        .await
                        .unwrap();
                    let catch_up = pair
                        .source
                        .intent(PublicOperationProgram::CatchUp {
                            configuration: current,
                            mode: PublicCatchUpMode::All,
                        })
                        .await;
                    let wait_cut = hold(&pair.source, Cut::BeforeInstruction, 1);
                    let mut task = rpc(&pair.source, catch_up).await;
                    entered(&wait_cut, &mut task).await;
                    stage(&pair.source, ReconfigurationPending, ReconfigurationPending).await;
                    finish(&pair.source, &wait_cut, task).await;
                }
                stage(&pair.source, Granted, Granted).await;
                pair.stop().await;
            }
        }
    }
}

#[tokio::test]
async fn selected_superseded_nonprimary_role_split_preserves_demotion_reconciliation() {
    superseded_nonprimary_reservation(false, false).await;
}

#[tokio::test]
async fn selected_superseded_nonprimary_role_recipe_preserves_demotion_reconciliation() {
    superseded_nonprimary_reservation(true, false).await;
}

#[tokio::test]
async fn selected_superseded_swap_reservation_split_preserves_demotion_reconciliation() {
    superseded_nonprimary_reservation(false, true).await;
}

#[tokio::test]
async fn selected_superseded_swap_reservation_recipe_preserves_demotion_reconciliation() {
    superseded_nonprimary_reservation(true, true).await;
}

async fn superseded_nonprimary_reservation(recipe: bool, swap: bool) {
    use AccessStatus::*;
    let root = root();
    let cuts = if swap {
        vec![Some(Cut::BeforeInstruction), Some(Cut::InstructionSelected)]
    } else {
        vec![
            None,
            Some(Cut::CallbackSuccess),
            Some(Cut::BeforeInstruction),
            Some(Cut::InstructionSelected),
        ]
    };
    for composed in [false, true] {
        for &position in &cuts {
            let pair = Pair::start(
                &root
                    .path()
                    .join(format!("{recipe}-{composed}-{position:?}")),
                1,
            )
            .await;
            let (previous, old) =
                demote_before_repromotion(&pair, composed, Some(Cut::InstructionSelected)).await;
            let program = if swap {
                let mut program = old.as_ref().unwrap().2.program.clone().unwrap();
                if let PublicOperationProgram::Swap {
                    starting,
                    refreshed,
                    epoch,
                    ..
                } = &mut program
                {
                    // This successor is new work, not exact replay of the
                    // predecessor's now-retired pre-epoch channel.
                    *starting = refreshed.clone();
                    *epoch = Epoch::new(0, 3);
                    refreshed.current = ConfigurationDescriptor::new(
                        *epoch,
                        starting.current.primary_id,
                        starting.current.members.clone(),
                        starting.current.write_quorum,
                    );
                }
                program
            } else {
                PublicOperationProgram::Role {
                    epoch: previous.epoch,
                    role: ReplicaRole::ActiveSecondary,
                }
            };
            let intermediate = pair.source.intent(program).await;
            let intermediate_cut =
                hold(&pair.source, position.unwrap_or(Cut::BeforeInstruction), 0);
            let mut intermediate_task = rpc(&pair.source, intermediate.clone()).await;
            entered(&intermediate_cut, &mut intermediate_task).await;
            let mut held = Some((intermediate_cut, intermediate_task));
            if position.is_none() {
                let (cut, task) = held.take().unwrap();
                finish(&pair.source, &cut, task).await;
            }
            let native_secondary = position.is_none() || position == Some(Cut::CallbackSuccess);
            assert_eq!(
                pair.source.report().await.role,
                if native_secondary {
                    ReplicaRole::ActiveSecondary
                } else {
                    ReplicaRole::Primary
                }
            );
            let pending = position == Some(Cut::BeforeInstruction)
                || (swap && !composed && position == Some(Cut::InstructionSelected));
            let status = if pending {
                ReconfigurationPending
            } else {
                NotPrimary
            };
            stage(&pair.source, status, status).await;

            let current = prepare_repromotion(&pair, recipe.then_some(previous)).await;
            let successor = repromotion_intent(&pair.source, &current, recipe).await;
            assert!(successor.revision > intermediate.revision);
            let role_cut = hold(
                &pair.source,
                Cut::BeforeInstruction,
                if recipe { 2 } else { 1 },
            );
            let mut task = rpc(&pair.source, successor.clone()).await;
            entered(&role_cut, &mut task).await;
            stage(&pair.source, ReconfigurationPending, ReconfigurationPending).await;
            release_old_demotion(&pair.source, old).await;
            if let Some((cut, old_task)) = held {
                cut.release.notify_one();
                assert!(
                    tokio::time::timeout(Duration::from_secs(10), old_task)
                        .await
                        .unwrap()
                        .unwrap()
                        .is_err()
                );
                assert!(
                    rpc(&pair.source, intermediate.clone())
                        .await
                        .await
                        .unwrap()
                        .is_err()
                );
            }
            stage(&pair.source, ReconfigurationPending, ReconfigurationPending).await;
            finish(&pair.source, &role_cut, task).await;
            if !recipe {
                stage(&pair.source, ReconfigurationPending, ReconfigurationPending).await;
                pair.source
                    .command(PublicOperationProgram::Epoch {
                        epoch: current.current.epoch,
                    })
                    .await
                    .unwrap();
                pair.source
                    .command(PublicOperationProgram::Configuration(current.clone()))
                    .await
                    .unwrap();
                stage(&pair.source, ReconfigurationPending, ReconfigurationPending).await;
                let catch_up = pair
                    .source
                    .intent(PublicOperationProgram::CatchUp {
                        configuration: current,
                        mode: PublicCatchUpMode::All,
                    })
                    .await;
                let wait_cut = hold(&pair.source, Cut::BeforeInstruction, 1);
                let mut task = rpc(&pair.source, catch_up).await;
                entered(&wait_cut, &mut task).await;
                stage(&pair.source, ReconfigurationPending, ReconfigurationPending).await;
                finish(&pair.source, &wait_cut, task).await;
            }
            stage(&pair.source, Granted, Granted).await;
            let replay = rpc(&pair.source, intermediate).await.await.unwrap();
            if position.is_none() {
                replay.unwrap();
            } else {
                assert!(replay.is_err());
            }
            if recipe {
                rpc(&pair.source, successor).await.await.unwrap().unwrap();
            }
            stage(&pair.source, Granted, Granted).await;
            pair.stop().await;
        }
    }
}

#[tokio::test]
async fn selected_secondary_epoch_recipe_cannot_activate_pending_primary_reconciliation() {
    use AccessStatus::*;
    let root = root();
    let pair = Pair::start(root.path(), 1).await;
    let (previous, old) =
        demote_before_repromotion(&pair, true, Some(Cut::InstructionSelected)).await;
    let current = prepare_repromotion(&pair, Some(previous)).await;
    let first = repromotion_intent(&pair.source, &current, true).await;
    let first_cut = hold(&pair.source, Cut::BeforeInstruction, 5);
    let mut first_task = rpc(&pair.source, first.clone()).await;
    entered(&first_cut, &mut first_task).await;
    stage(&pair.source, Granted, ReconfigurationPending).await;

    let mut intermediate = repromotion_intent(&pair.source, &current, true).await;
    let input = intermediate.lifecycle.as_mut().unwrap();
    input.recipe = PublicLifecycleRecipe::SecondaryEpochAdvance;
    input.previous = None;
    input.epoch = Epoch::new(0, 4);
    input.current.epoch = input.epoch;
    input.current.primary_id = pair.target.identity.replica_id;
    for member in &mut input.current.members {
        member.role = if member.identity == pair.target.identity {
            ReplicaRole::Primary
        } else {
            ReplicaRole::ActiveSecondary
        };
    }
    input.current.configuration_id = input.current.expected_id();
    input.replicas = vec![pair.target.descriptor(ReplicaRole::Primary).await];
    input.validate().unwrap();
    intermediate.input_digest = input.digest();
    pair.source.handle().set_public_operation_cut(None);
    rpc(&pair.source, intermediate)
        .await
        .await
        .unwrap()
        .unwrap();
    stage(&pair.source, ReconfigurationPending, ReconfigurationPending).await;

    let next = prepare_repromotion_at(&pair, current.previous, Epoch::new(0, 5)).await;
    let successor = repromotion_intent(&pair.source, &next, true).await;
    let role_cut = hold(&pair.source, Cut::BeforeInstruction, 2);
    let mut task = rpc(&pair.source, successor).await;
    entered(&role_cut, &mut task).await;
    stage(&pair.source, ReconfigurationPending, ReconfigurationPending).await;
    release_old_demotion(&pair.source, old).await;
    first_cut.release.notify_one();
    assert!(
        tokio::time::timeout(Duration::from_secs(10), first_task)
            .await
            .unwrap()
            .unwrap()
            .is_err()
    );
    assert!(rpc(&pair.source, first).await.await.unwrap().is_err());
    stage(&pair.source, ReconfigurationPending, ReconfigurationPending).await;
    finish(&pair.source, &role_cut, task).await;
    stage(&pair.source, Granted, Granted).await;

    let redundant_primary = pair
        .source
        .intent(PublicOperationProgram::Role {
            epoch: next.current.epoch,
            role: ReplicaRole::Primary,
        })
        .await;
    let error = rpc(&pair.source, redundant_primary)
        .await
        .await
        .unwrap()
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("role transition is not admitted from Ready(Primary) to Primary"),
        "{error}"
    );
    stage(&pair.source, NotPrimary, NotPrimary).await;
    pair.stop().await;
}

#[tokio::test]
async fn selected_superseded_split_repromotion_rebinds_demotion_lineage() {
    superseded_repromotion(false).await;
}

#[tokio::test]
async fn selected_superseded_recipe_repromotion_rebinds_demotion_lineage() {
    superseded_repromotion(true).await;
}

async fn superseded_repromotion(recipe: bool) {
    use AccessStatus::*;
    let root = root();
    let first_cuts = if recipe {
        [(Cut::BeforeInstruction, 0), (Cut::BeforeInstruction, 5)]
    } else {
        [(Cut::BeforeInstruction, 0), (Cut::InstructionSelected, 0)]
    };
    for composed in [false, true] {
        for demotion_cut in [Cut::BeforeInstruction, Cut::InstructionSelected] {
            for &(position, index) in &first_cuts {
                let pair = Pair::start(
                    &root
                        .path()
                        .join(format!("{composed}-{demotion_cut:?}-{position:?}-{index}")),
                    1,
                )
                .await;
                let (previous, old) =
                    demote_before_repromotion(&pair, composed, Some(demotion_cut)).await;
                let current = prepare_repromotion(&pair, recipe.then_some(previous)).await;
                let first = repromotion_intent(&pair.source, &current, recipe).await;
                let first_cut = hold(&pair.source, position, index);
                let mut first_task = rpc(&pair.source, first.clone()).await;
                entered(&first_cut, &mut first_task).await;
                let read = if index == 5 {
                    Granted
                } else {
                    ReconfigurationPending
                };
                stage(&pair.source, read, ReconfigurationPending).await;
                release_old_demotion(&pair.source, old).await;
                stage(&pair.source, read, ReconfigurationPending).await;

                let next =
                    prepare_repromotion_at(&pair, current.previous.clone(), Epoch::new(0, 4)).await;
                let successor = repromotion_intent(&pair.source, &next, recipe).await;
                assert!(successor.revision > first.revision);
                // The successor has reconciled its own native role, but must not
                // inherit the predecessor's application/configuration/wait grants.
                let successor_cut = hold(
                    &pair.source,
                    Cut::BeforeInstruction,
                    if recipe { 2 } else { 1 },
                );
                let mut successor_task = rpc(&pair.source, successor.clone()).await;
                entered(&successor_cut, &mut successor_task).await;
                stage(&pair.source, ReconfigurationPending, ReconfigurationPending).await;
                first_cut.release.notify_one();
                assert!(
                    tokio::time::timeout(Duration::from_secs(10), first_task)
                        .await
                        .unwrap()
                        .unwrap()
                        .is_err()
                );
                assert!(
                    rpc(&pair.source, first.clone())
                        .await
                        .await
                        .unwrap()
                        .is_err()
                );
                stage(&pair.source, ReconfigurationPending, ReconfigurationPending).await;
                finish(&pair.source, &successor_cut, successor_task).await;

                if !recipe {
                    stage(&pair.source, ReconfigurationPending, ReconfigurationPending).await;
                    pair.source
                        .command(PublicOperationProgram::Epoch {
                            epoch: next.current.epoch,
                        })
                        .await
                        .unwrap();
                    pair.source
                        .command(PublicOperationProgram::Configuration(next.clone()))
                        .await
                        .unwrap();
                    stage(&pair.source, ReconfigurationPending, ReconfigurationPending).await;
                    let catch_up = pair
                        .source
                        .intent(PublicOperationProgram::CatchUp {
                            configuration: next,
                            mode: PublicCatchUpMode::All,
                        })
                        .await;
                    let catch_up_cut = hold(&pair.source, Cut::BeforeInstruction, 1);
                    let mut catch_up_task = rpc(&pair.source, catch_up).await;
                    entered(&catch_up_cut, &mut catch_up_task).await;
                    stage(&pair.source, ReconfigurationPending, ReconfigurationPending).await;
                    finish(&pair.source, &catch_up_cut, catch_up_task).await;
                }
                stage(&pair.source, Granted, Granted).await;
                assert!(rpc(&pair.source, first).await.await.unwrap().is_err());
                if recipe {
                    rpc(&pair.source, successor).await.await.unwrap().unwrap();
                }
                stage(&pair.source, Granted, Granted).await;
                pair.stop().await;
            }
        }
    }
}

#[tokio::test]
async fn selected_same_live_host_split_repromotion_retires_only_old_demotion() {
    use AccessStatus::*;
    let root = root();
    for composed in [false, true] {
        for demotion_cut in [
            None,
            Some(Cut::BeforeInstruction),
            Some(Cut::InstructionSelected),
        ] {
            for wait_cut in [Cut::BeforeInstruction, Cut::InstructionApplied] {
                let pair = Pair::start(
                    &root
                        .path()
                        .join(format!("{composed}-{demotion_cut:?}-{wait_cut:?}")),
                    1,
                )
                .await;
                let (_, old) = demote_before_repromotion(&pair, composed, demotion_cut).await;
                let current = prepare_repromotion(&pair, None).await;
                let epoch = current.current.epoch;
                let role_cut = hold(&pair.source, Cut::BeforeInstruction, 1);
                let role = pair
                    .source
                    .intent(PublicOperationProgram::Role {
                        epoch,
                        role: ReplicaRole::Primary,
                    })
                    .await;
                let mut task = rpc(&pair.source, role).await;
                entered(&role_cut, &mut task).await;
                stage(&pair.source, ReconfigurationPending, ReconfigurationPending).await;
                release_old_demotion(&pair.source, old).await;
                stage(&pair.source, ReconfigurationPending, ReconfigurationPending).await;
                finish(&pair.source, &role_cut, task).await;
                pair.source
                    .command(PublicOperationProgram::Epoch { epoch })
                    .await
                    .unwrap();
                pair.source
                    .command(PublicOperationProgram::Configuration(current.clone()))
                    .await
                    .unwrap();
                stage(&pair.source, ReconfigurationPending, ReconfigurationPending).await;
                let cut = hold(&pair.source, wait_cut, 1);
                let catch_up = pair
                    .source
                    .intent(PublicOperationProgram::CatchUp {
                        configuration: current,
                        mode: PublicCatchUpMode::All,
                    })
                    .await;
                let replay = catch_up.clone();
                let mut task = rpc(&pair.source, catch_up).await;
                entered(&cut, &mut task).await;
                stage(
                    &pair.source,
                    if wait_cut == Cut::BeforeInstruction {
                        ReconfigurationPending
                    } else {
                        Granted
                    },
                    ReconfigurationPending,
                )
                .await;
                finish(&pair.source, &cut, task).await;
                stage(&pair.source, Granted, Granted).await;
                pair.source
                    .handle()
                    .execute_public_v1(replay)
                    .await
                    .unwrap();
                stage(&pair.source, Granted, Granted).await;
                pair.stop().await;
            }
        }
    }
}

#[tokio::test]
async fn selected_same_live_host_recipe_repromotion_retires_only_old_demotion() {
    use AccessStatus::*;
    let root = root();
    for composed in [false, true] {
        for demotion_cut in [
            None,
            Some(Cut::BeforeInstruction),
            Some(Cut::InstructionSelected),
        ] {
            for index in [0, 4, 5] {
                let pair = Pair::start(
                    &root
                        .path()
                        .join(format!("{composed}-{demotion_cut:?}-{index}")),
                    1,
                )
                .await;
                let (previous, old) =
                    demote_before_repromotion(&pair, composed, demotion_cut).await;
                let configuration = prepare_repromotion(&pair, Some(previous)).await;
                let input = PublicLifecycleInput {
                    recipe: PublicLifecycleRecipe::FailoverPromotion,
                    replica: pair.source.identity.clone(),
                    epoch: configuration.current.epoch,
                    possible_data_loss: PossibleDataLossIntent::NotPossible,
                    current: configuration.current,
                    previous: configuration.previous,
                    replicas: configuration.replicas,
                    data_loss_recovery: None,
                };
                input.validate().unwrap();
                let mut intent = pair
                    .source
                    .intent(PublicOperationProgram::Epoch { epoch: input.epoch })
                    .await;
                intent.program = None;
                intent.input_digest = input.digest();
                intent.lifecycle = Some(input);
                let replay = intent.clone();
                let cut = hold(&pair.source, Cut::BeforeInstruction, index);
                let mut task = rpc(&pair.source, intent).await;
                entered(&cut, &mut task).await;
                let read = if index == 5 {
                    Granted
                } else {
                    ReconfigurationPending
                };
                stage(&pair.source, read, ReconfigurationPending).await;
                release_old_demotion(&pair.source, old).await;
                stage(&pair.source, read, ReconfigurationPending).await;
                finish(&pair.source, &cut, task).await;
                stage(&pair.source, Granted, Granted).await;
                pair.source
                    .handle()
                    .execute_public_v1(replay)
                    .await
                    .unwrap();
                stage(&pair.source, Granted, Granted).await;
                pair.stop().await;
            }
        }
    }
}

#[tokio::test]
async fn selected_resumed_promotion_cannot_clear_fault_terminal_or_late_generation_denial() {
    use AccessStatus::*;
    let root = root();
    for demotion_cut in [Cut::BeforeInstruction, Cut::InstructionSelected] {
        for fault in [false, true] {
            let mut pair =
                Pair::start(&root.path().join(format!("{demotion_cut:?}-{fault}")), 1).await;
            let (_, old) = demote_before_repromotion(&pair, true, Some(demotion_cut)).await;
            let current = prepare_repromotion(&pair, None).await;
            let epoch = current.current.epoch;
            pair.source
                .command(PublicOperationProgram::Role {
                    epoch,
                    role: ReplicaRole::Primary,
                })
                .await
                .unwrap();
            pair.source
                .command(PublicOperationProgram::Epoch { epoch })
                .await
                .unwrap();
            let cut = hold(&pair.source, Cut::InstructionApplied, 1);
            let intent = pair
                .source
                .intent(PublicOperationProgram::CatchUp {
                    configuration: current,
                    mode: PublicCatchUpMode::All,
                })
                .await;
            let mut task = rpc(&pair.source, intent.clone()).await;
            entered(&cut, &mut task).await;
            stage(&pair.source, Granted, ReconfigurationPending).await;
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
            release_old_demotion(&pair.source, old).await;
            stage(&pair.source, NotPrimary, NotPrimary).await;
            cut.release.notify_one();
            assert!(task.await.unwrap().is_err());
            assert!(
                pair.source
                    .handle()
                    .execute_public_v1(intent)
                    .await
                    .is_err()
            );
            stage(&pair.source, NotPrimary, NotPrimary).await;
            pair.source.stop().await;
            pair.target.stop().await;
        }
    }
}

#[tokio::test]
async fn selected_same_primary_joint_and_final_use_real_c_and_p() {
    use AccessStatus::*;
    let root = root();
    let mut pair = Pair::start(root.path(), 1).await;
    let previous = pair.configuration.clone();
    let joint = next_joint(&mut pair, previous, Vec::new()).await;
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
    previous_only
        .open(ReplicaRole::IdleSecondary, pair.configuration.epoch)
        .await;
    pair.source.build(&previous_only, &pair.configuration).await;
    previous_only
        .command(PublicOperationProgram::Role {
            epoch: pair.configuration.epoch,
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
    let previous = ConfigurationDescriptor::new(
        pair.configuration.epoch,
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
    );
    // Finish the prior joint configuration before selecting a different previous electorate.
    pair.source
        .command(PublicOperationProgram::Configuration(
            configuration(&pair).await,
        ))
        .await
        .unwrap();
    let no_p = next_joint(&mut pair, previous, vec![peer]).await;
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
                let mut pair = Pair::start(&directory, 1).await;
                let mut starting = if joint {
                    let previous = pair.configuration.clone();
                    let input = next_joint(&mut pair, previous, Vec::new()).await;
                    pair.source
                        .command(PublicOperationProgram::Configuration(input.clone()))
                        .await
                        .unwrap();
                    input
                } else {
                    configuration(&pair).await
                };
                starting.replicas[0].must_catchup = true;
                if composed {
                    pair.source
                        .command(PublicOperationProgram::CatchUp {
                            configuration: starting.clone(),
                            mode: PublicCatchUpMode::All,
                        })
                        .await
                        .unwrap();
                }
                let epoch = Epoch::new(0, pair.configuration.epoch.configuration_number + 1);
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
                let peer = advance_swap_peer(&pair, target_configuration).await;
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
                finish(&pair.source, &cut, task).await;
                peer.await.unwrap();
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
