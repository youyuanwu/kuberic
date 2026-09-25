use super::*;
use kuberic_protocol::command::{AcceptSecondaryRemovalCommit, ProtocolCommand};
use kuberic_protocol::observation::{AgentObservation, ReplicaObservationKey};
use kuberic_protocol::plan::Plan;
use kuberic_protocol::types::{AcceptedTopology, SecondaryRemovalReceipt};
use kuberic_wire::proto;
use tokio::sync::watch;
use tonic::{Code, Request};

#[test]
fn historical_local_acceptance_survives_sqlite_service_restart_and_newer_authority() {
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(|| {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(async {
                    tokio::time::timeout(std::time::Duration::from_secs(90), recover_history())
                        .await
                        .unwrap();
                });
        })
        .unwrap()
        .join()
        .unwrap();
}

fn request(
    intent: &SecondaryScaleDownIntent,
    local: &ReplicaIdentity,
    session: &str,
    command: proto::execute_command_request::Command,
) -> Request<proto::ExecuteCommandRequest> {
    let mut request = Request::new(proto::ExecuteCommandRequest {
        protocol_version: kuberic_protocol::PROTOCOL_VERSION,
        resource_uid: intent.resource_uid.to_string(),
        target: Some(local.clone().into()),
        expected_process_session_id: session.into(),
        command: Some(command),
    });
    request
        .metadata_mut()
        .insert("authorization", "Bearer token".parse().unwrap());
    request
}

fn wire_command(command: ProtocolCommand) -> proto::execute_command_request::Command {
    match command {
        ProtocolCommand::AcceptSecondaryRemovalCommit(c) => {
            proto::execute_command_request::Command::AcceptSecondaryRemovalCommit(Box::new(
                (*c).into(),
            ))
        }
        ProtocolCommand::EnsureConfiguration(c) => {
            assert_eq!(c.transition_kind, TransitionKind::Failover);
            proto::execute_command_request::Command::EnsureConfiguration(Box::new(
                proto::EnsureConfigurationCommand {
                    operation_id: c.operation_id.to_string(),
                    local_replica_id: c.local_replica_id.value(),
                    expected_instance_id: c.expected_instance_id.to_string(),
                    expected_agent_generation: c.expected_agent_generation.to_string(),
                    previous_configuration: c.previous_configuration.map(Into::into),
                    current_configuration: Some(c.current_configuration.into()),
                    previous_epoch: c.previous_epoch.map(Into::into),
                    current_epoch: Some(c.current_epoch.into()),
                    effective_policy: Some(c.effective_policy.into()),
                    transition_kind: proto::TransitionKind::Failover as i32,
                    primary_write_status: proto::AccessStatus::ReconfigurationPending as i32,
                    current_only: c.current_only,
                    failover_safe_lsn: c.failover_safe_lsn,
                    ..Default::default()
                },
            ))
        }
        other => panic!("unexpected recovery {other:?}"),
    }
}

async fn recover_history() {
    for (replacement, cut) in [false, true]
        .into_iter()
        .flat_map(|replacement| (0..3).map(move |cut| (replacement, cut)))
    {
        let directory = tempdir().unwrap();
        let mut intent = fixture::intent(&[1, 2, 3, 4, 5, 6], 1);
        intent.desired_replicas = 5;
        intent.operation_id = intent.expected_operation_id();
        let mut members = Vec::new();
        for index in 0..5 {
            members.push(member(directory.path(), &intent, index, true).await);
        }
        let primary = &members[0];
        for peer in &members[1..] {
            register(primary, peer).await;
        }
        let pending = primary
            .runtime
            .data_plane()
            .begin_write(ClientWrite {
                operation_id: OperationId::new("before-six-to-five"),
                data: Bytes::from_static(b"historical-prefix"),
            })
            .await
            .unwrap();
        for peer in &members[1..] {
            let identity = peer.runtime.snapshot().await.identity;
            let item = pending
                .replication_items
                .iter()
                .find(|item| item.receiver.as_ref() == Some(&identity.clone().into()))
                .unwrap()
                .clone();
            let mut ack = peer
                .runtime
                .data_plane()
                .receive_replication(item)
                .await
                .unwrap()
                .applied()
                .await
                .unwrap();
            ack.receiver_session_id = peer.session.to_string();
            primary
                .runtime
                .data_plane()
                .accept_acknowledgement(ack)
                .await
                .unwrap();
        }
        assert_eq!(pending.committed().await.unwrap().lsn, 1);
        let preparation = Coordinator::new(primary.store.clone(), primary.runtime.clone())
            .ensure_secondary_removal_prepared(
                fixture::prepare_command(&intent),
                primary.session.clone(),
                1,
            )
            .await
            .unwrap();
        assert_eq!(preparation.boundary_lsn, 1);
        let mut evidence = SecondaryRemovalEvidence {
            preparation,
            previous_read_quorum: Vec::new(),
            reduced_write_quorum: Vec::new(),
        };
        for peer in &members[..3] {
            evidence
                .previous_read_quorum
                .push(witness(peer, &intent, 2).await);
        }
        for peer in members[1..].iter().chain(&members[..1]) {
            configure(peer, &evidence, false).await;
        }
        for peer in &members[..3] {
            evidence
                .reduced_write_quorum
                .push(witness(peer, &intent, 3).await);
        }
        for peer in members[1..].iter().chain(&members[..1]) {
            configure(peer, &evidence, true).await;
        }
        let mut committed = SecondaryScaleDownCleanup {
            evidence,
            current_only_write_quorum: Vec::new(),
            retirement: None,
        };
        for peer in &members[..3] {
            committed
                .current_only_write_quorum
                .push(witness(peer, &intent, 4).await);
        }
        for peer in &members[..4] {
            for source in &members[..3] {
                if peer.session != source.session {
                    register(peer, source).await;
                }
            }
            let local = peer.runtime.snapshot().await.identity;
            Coordinator::new(peer.store.clone(), peer.runtime.clone())
                .accept_secondary_removal_commit(AcceptSecondaryRemovalCommit {
                    operation_id: intent
                        .command_operation_id(SecondaryRemovalStage::AcceptCommit, &local),
                    target: local,
                    committed: committed.clone(),
                    local_recovery: false,
                })
                .await
                .unwrap();
        }
        let mut old_session = members[4].session.to_string();
        assert!(
            members[4]
                .store
                .load_state()
                .await
                .unwrap()
                .accepted_secondary_removal
                .is_none()
        );
        for peer in &members {
            peer.runtime.abort();
        }
        drop(members);

        // Cleanup has completed and a quorum has accepted newer authority while 5 is absent.
        let receipt = SecondaryRemovalReceipt {
            evidence: committed.evidence.clone(),
            current_only_write_quorum: committed.current_only_write_quorum.clone(),
        };
        let mut cluster = model::Model::new(&[1, 2, 3, 4, 5, 6], 1, 5);
        let mut newer = intent.current_configuration.members.clone();
        if replacement {
            newer[3].identity.instance_id = ReplicaInstanceId::new("replacement-4");
            newer[3].identity.agent_generation = AgentGeneration::new("replacement-generation");
        } else {
            newer[0].role = ReplicaRole::ActiveSecondary;
            newer[1].role = ReplicaRole::Primary;
        }
        let accepted = ConfigurationDescriptor::new(
            Epoch::new(2, 12),
            ReplicaId::new(if replacement { 1 } else { 2 }),
            newer,
            intent.current_policy.write_quorum,
        );
        let removed = cluster.key(6);
        cluster.snapshot.replicas.remove(&removed);
        for member in &accepted.members {
            let key = cluster.key(member.identity.replica_id.value());
            let mut observation = cluster.snapshot.replicas.remove(&key).unwrap();
            let AgentObservation::Report(report) = &mut observation.agent else {
                unreachable!()
            };
            report.identity = member.identity.clone();
            report.epoch = accepted.epoch;
            report.current_configuration = Some(accepted.clone());
            report.role = member.role;
            report.write_status = if member.role == ReplicaRole::Primary {
                AccessStatus::Granted
            } else {
                AccessStatus::NotPrimary
            };
            observation.kubernetes.as_mut().unwrap().pod_uid =
                Some(PodUid::new(member.identity.instance_id.as_str()));
            cluster.snapshot.replicas.insert(
                ReplicaObservationKey::new(
                    member.identity.replica_id,
                    member.identity.instance_id.clone(),
                ),
                observation,
            );
        }
        cluster.snapshot.status.topology = Some(AcceptedTopology {
            configuration: accepted.clone(),
        });
        cluster.snapshot.status.effective_policy = Some(intent.current_policy.clone());
        cluster.snapshot.status.last_secondary_removal = Some(receipt.clone());
        cluster.snapshot.routing.write_target = Some(
            accepted
                .members
                .iter()
                .find(|m| m.role == ReplicaRole::Primary)
                .unwrap()
                .identity
                .clone(),
        );
        let local = intent.current_configuration.members[4].identity.clone();
        let mut recovery = None;
        for restart in 0..3 {
            let late = member(directory.path(), &intent, 4, false).await;
            assert_ne!(late.session.as_str(), old_session);
            assert_eq!(
                late.application.state.lock().unwrap().operations[&1],
                b"historical-prefix"
            );
            let address = std::net::TcpListener::bind("127.0.0.1:0")
                .unwrap()
                .local_addr()
                .unwrap();
            let replication = std::net::TcpListener::bind("127.0.0.1:0")
                .unwrap()
                .local_addr()
                .unwrap();
            let (ready, mut ready_rx) = watch::channel(false);
            let (shutdown, shutdown_rx) = watch::channel(false);
            let server = tokio::spawn(late.service.clone().serve(
                address,
                replication,
                ready,
                shutdown_rx,
            ));
            tokio::time::timeout(std::time::Duration::from_secs(5), ready_rx.wait_for(|r| *r))
                .await
                .unwrap()
                .unwrap();
            let mut client = proto::agent_control_client::AgentControlClient::connect(format!(
                "http://{address}"
            ))
            .await
            .unwrap();
            let mut status = Request::new(proto::GetAgentStatusRequest {
                protocol_version: kuberic_protocol::PROTOCOL_VERSION,
                resource_uid: intent.resource_uid.to_string(),
                replica_id: local.replica_id.value(),
                expected_instance_id: local.instance_id.to_string(),
            });
            status
                .metadata_mut()
                .insert("authorization", "Bearer token".parse().unwrap());
            let report = client.get_status(status).await.unwrap().into_inner();
            let AgentObservation::Report(report) =
                kuberic_wire::normalize_agent_status_report(report).unwrap()
            else {
                panic!("initialized")
            };
            *cluster.report(5) = *report;
            if restart == 0 {
                let Plan::Execute {
                    command: ProtocolCommand::AcceptSecondaryRemovalCommit(command),
                } = cluster.plan()
                else {
                    panic!("expected exact local recovery: {:?}", cluster.plan());
                };
                assert!(command.local_recovery);
                assert_eq!(command.committed, committed);
                recovery = Some(*command);
            }
            let command = recovery.clone().unwrap();
            let before = late.store.load_state().await.unwrap();
            let envelope = wire_command(ProtocolCommand::AcceptSecondaryRemovalCommit(Box::new(
                command.clone(),
            )));
            assert_eq!(
                client
                    .execute(request(&intent, &local, &old_session, envelope.clone()))
                    .await
                    .unwrap_err()
                    .code(),
                Code::FailedPrecondition
            );
            assert_eq!(late.store.load_state().await.unwrap(), before);
            if restart == 0 {
                for mutation in 0..7 {
                    let mut wrong = command.clone();
                    match mutation {
                        0 => wrong.operation_id = OperationId::new("different-operation"),
                        1 => wrong.committed.evidence.preparation.boundary_lsn = 2,
                        2 => wrong.committed.current_only_write_quorum.clear(),
                        3 => {
                            wrong.target.agent_generation = AgentGeneration::new("wrong-generation")
                        }
                        4 => wrong.target = intent.target.clone(),
                        5 => wrong.target = intent.primary.clone(),
                        _ => {
                            wrong.committed.evidence.preparation.intent.resource_uid =
                                ResourceUid::new("other-resource")
                        }
                    }
                    assert!(
                        late.store
                            .begin_effect(&RuntimeEffect {
                                operation_id: wrong.operation_id.clone(),
                                sequence: before.next_effect_sequence,
                                action: RuntimeEffectAction::AcceptHistoricalSecondaryRemovalCommit(
                                    Box::new(wrong.clone())
                                ),
                            })
                            .await
                            .is_err()
                    );
                    assert!(
                        client
                            .execute(request(
                                &intent,
                                &local,
                                late.session.as_str(),
                                wire_command(ProtocolCommand::AcceptSecondaryRemovalCommit(
                                    Box::new(wrong)
                                ))
                            ))
                            .await
                            .is_err()
                    );
                    assert_eq!(late.store.load_state().await.unwrap(), before);
                }
                // The normal correction really is fenced before local acceptance.
                let mut assumed = cluster.clone();
                assumed.report(5).accepted_secondary_removal = Some(committed.clone());
                let Plan::Execute {
                    command: correction,
                } = assumed.plan()
                else {
                    panic!("correction")
                };
                assert_eq!(
                    client
                        .execute(request(
                            &intent,
                            &local,
                            late.session.as_str(),
                            wire_command(correction)
                        ))
                        .await
                        .unwrap_err()
                        .code(),
                    Code::FailedPrecondition
                );
                assert_eq!(late.store.load_state().await.unwrap(), before);
            }
            if restart < 2 {
                if restart == 0 && cut > 0 {
                    let effect = RuntimeEffect {
                        operation_id: command.operation_id.clone(),
                        sequence: before.next_effect_sequence,
                        action: RuntimeEffectAction::AcceptHistoricalSecondaryRemovalCommit(
                            Box::new(command.clone()),
                        ),
                    };
                    late.store.begin_effect(&effect).await.unwrap();
                    if cut == 2 {
                        late.runtime.apply_effect(effect.clone()).await.unwrap();
                        late.store.mark_effect_applied(&effect).await.unwrap();
                    }
                    assert!(
                        late.store
                            .load_state()
                            .await
                            .unwrap()
                            .accepted_secondary_removal
                            .is_none()
                    );
                    old_session = late.session.to_string();
                    shutdown.send_replace(true);
                    server.await.unwrap().unwrap();
                    continue;
                }
                let before_runtime = late.runtime.snapshot().await;
                let report = client
                    .execute(request(&intent, &local, late.session.as_str(), envelope))
                    .await
                    .unwrap()
                    .into_inner()
                    .observation
                    .unwrap();
                let AgentObservation::Report(report) =
                    kuberic_wire::normalize_agent_status_report(report).unwrap()
                else {
                    panic!("initialized")
                };
                *cluster.report(5) = *report;
                let after_runtime = late.runtime.snapshot().await;
                assert_eq!(after_runtime.authority, before_runtime.authority);
                assert_eq!(after_runtime.write_status, before_runtime.write_status);
                assert!(
                    !after_runtime.catch_up_complete,
                    "no historical quorum/session credit"
                );
                assert_eq!(
                    after_runtime.current_configuration_quorum_progress,
                    before_runtime.current_configuration_quorum_progress
                );
                assert_eq!(
                    late.store.load_secondary_removal_commit().await.unwrap(),
                    None
                );
                assert_eq!(
                    late.store
                        .load_state()
                        .await
                        .unwrap()
                        .accepted_secondary_removal,
                    Some(committed.clone())
                );
                let mut wrong = command.clone();
                wrong.committed.current_only_write_quorum[0].report_sequence += 1;
                assert!(
                    client
                        .execute(request(
                            &intent,
                            &local,
                            late.session.as_str(),
                            wire_command(ProtocolCommand::AcceptSecondaryRemovalCommit(Box::new(
                                wrong
                            )))
                        ))
                        .await
                        .is_err()
                );
                for current_only in [false, true] {
                    if restart == 0 {
                        break;
                    }
                    let Plan::Execute {
                        command: correction,
                    } = cluster.plan()
                    else {
                        panic!("correction: {:?}", cluster.plan())
                    };
                    assert!(
                        matches!(&correction, ProtocolCommand::EnsureConfiguration(c) if c.current_only == current_only && c.current_configuration == accepted)
                    );
                    let wire = wire_command(correction);
                    assert_eq!(
                        client
                            .execute(request(&intent, &local, &old_session, wire.clone()))
                            .await
                            .unwrap_err()
                            .code(),
                        Code::FailedPrecondition
                    );
                    let report = client
                        .execute(request(&intent, &local, late.session.as_str(), wire))
                        .await
                        .unwrap()
                        .into_inner()
                        .observation
                        .unwrap();
                    let AgentObservation::Report(report) =
                        kuberic_wire::normalize_agent_status_report(report).unwrap()
                    else {
                        panic!("initialized")
                    };
                    *cluster.report(5) = *report;
                }
            } else {
                assert!(
                    late.store
                        .begin_effect(&RuntimeEffect {
                            operation_id: command.operation_id.clone(),
                            sequence: before.next_effect_sequence,
                            action: RuntimeEffectAction::AcceptHistoricalSecondaryRemovalCommit(
                                Box::new(command.clone())
                            ),
                        })
                        .await
                        .is_err(),
                    "store cannot regress newer authority"
                );
                assert!(
                    client
                        .execute(request(&intent, &local, late.session.as_str(), envelope))
                        .await
                        .is_err(),
                    "historical replay cannot regress newer authority"
                );
                assert_eq!(
                    cluster.report(5).current_configuration,
                    Some(accepted.clone())
                );
                cluster.finish();
                assert!(matches!(cluster.plan(), Plan::Stable { .. }));
            }
            assert_eq!(
                cluster.snapshot.status.last_secondary_removal,
                Some(receipt.clone())
            );
            old_session = late.session.to_string();
            shutdown.send_replace(true);
            server.await.unwrap().unwrap();
        }
    }
}
