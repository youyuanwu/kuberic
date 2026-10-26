use super::*;
use crate::replicator::v1::service_adapter::engine_session::RemovalCut;
use crate::replicator::{ReplicaInformation, ReplicaSetConfiguration, ReplicaSetQuorumMode};
use std::sync::Condvar;

async fn complete_build(
    source: &ReplicatorInterfaces,
    target: &ReplicatorInterfaces,
    publication: &ReplicaPublication,
    transports: [&Transport; 2],
) {
    let primary = source.primary_replicator().unwrap();
    let mut info = information(publication.scope.storage.replica.replica_id.value());
    info.identity = publication.scope.storage.replica.clone();
    info.process_session_id = publication.scope.process_session_id.clone();
    info.replication_address = publication.replication_address.clone();
    info.storage_incarnation = Some(publication.scope.storage.clone());
    let mut build = tokio::spawn(async move { primary.build_replica(info).await });
    let mut copy = target
        .state_replicator()
        .unwrap()
        .copy_operations()
        .unwrap();
    loop {
        let operation = tokio::select! {
            result = &mut build => panic!("Build ended before copy: {result:?}"),
            result = tokio::time::timeout(Duration::from_secs(10), copy.get_operation()) =>
                result.unwrap_or_else(|_| panic!("copy timeout: source-peers={} target-peers={} target-receivers={} source-native-idles={:?} source-failure={:?} target-failure={:?}",
                    transports[0].inspect_peers(), transports[1].inspect_peers(), transports[1].removal_receivers(),
                    transports[0].test_production().removal_primary().idle_replica_ids(),
                    transports[0].check_health(), transports[1].check_health())).unwrap().unwrap(),
        };
        if !operation.end_of_stream {
            assert_eq!(operation.data, Bytes::from_static(b"snapshot"));
        }
        operation.acknowledge().unwrap();
        if operation.end_of_stream {
            break;
        }
    }
    tokio::time::timeout(Duration::from_secs(5), build)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

fn configuration(pair: &Pair, include_target: bool) -> ReplicaSetConfiguration {
    let mut members = vec![ConfigurationMember {
        identity: pair.source.local().identity.clone(),
        role: ReplicaRole::Primary,
    }];
    let mut replicas = Vec::new();
    if include_target {
        members.push(ConfigurationMember {
            identity: pair.target.local().identity.clone(),
            role: ReplicaRole::ActiveSecondary,
        });
        replicas.push(ReplicaInformation {
            identity: pair.target.local().identity.clone(),
            process_session_id: pair.target.local().process_session_id.clone(),
            replication_address: pair.target.local().address.clone(),
            storage_incarnation: Some(pair.target.local().storage.clone()),
            role: ReplicaRole::ActiveSecondary,
            current_progress: 0,
            catch_up_capability: 0,
            must_catchup: false,
        });
    }
    ReplicaSetConfiguration {
        configuration: ConfigurationDescriptor::new(
            epoch(1),
            pair.source.local().identity.replica_id,
            members,
            if include_target { 2 } else { 1 },
        ),
        replicas,
    }
}

async fn write_and_catchup(source: &ReplicatorInterfaces, target: &ReplicatorInterfaces) {
    let mut stream = target
        .state_replicator()
        .unwrap()
        .replication_operations()
        .unwrap();
    let write = source
        .state_replicator()
        .unwrap()
        .begin_replicate(Bytes::from_static(b"previous-quorum-dependent"))
        .unwrap();
    let completion = tokio::spawn(write.completion);
    let operation = tokio::time::timeout(Duration::from_secs(5), stream.get_operation())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(operation.lsn, write.lsn);
    assert_eq!(
        operation.data,
        Bytes::from_static(b"previous-quorum-dependent")
    );
    assert!(
        !completion.is_finished(),
        "the previous voter must apply before completion"
    );
    operation.acknowledge().unwrap();
    tokio::time::timeout(Duration::from_secs(5), completion)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    for mode in [ReplicaSetQuorumMode::WriteQuorum, ReplicaSetQuorumMode::All] {
        tokio::time::timeout(
            Duration::from_secs(5),
            source
                .primary_replicator()
                .unwrap()
                .wait_for_catch_up_quorum(mode),
        )
        .await
        .unwrap()
        .unwrap();
    }
}

async fn close(engines: &[&ReplicatorInterfaces]) {
    for engine in engines {
        engine.replicator().abort();
    }
    for engine in engines {
        tokio::time::timeout(Duration::from_secs(5), engine.replicator().close())
            .await
            .unwrap()
            .unwrap();
    }
}

fn publish_configuration(
    pair: &Pair,
    current: &ReplicaSetConfiguration,
    previous: Option<&ReplicaSetConfiguration>,
    progress: Lsn,
) {
    let mut source = pair.binding.source.clone();
    let mut target = pair.binding.target.clone();
    for (publication, remote, role) in [
        (
            &mut source,
            &pair.binding.target,
            ReplicaRole::ActiveSecondary,
        ),
        (&mut target, &pair.binding.source, ReplicaRole::Primary),
    ] {
        publication.generation += current.configuration.epoch.configuration_number as u64;
        publication.stage = ReplicaPublicationStage::AchievedScope;
        publication.scope.epoch = current.configuration.epoch;
        publication.scope.configuration = ReplicaConfigurationScope {
            current: current.configuration.clone(),
            previous: previous.map(|previous| previous.configuration.clone()),
            replicas: vec![ReplicaPeerDescription {
                identity: remote.scope.storage.replica.clone(),
                process_session_id: remote.scope.process_session_id.clone(),
                storage_incarnation: remote.scope.storage.clone(),
                replication_address: remote.replication_address.clone(),
                role,
                current_progress: progress,
                catch_up_capability: progress,
                is_up: true,
                is_ready: true,
                must_catchup: false,
            }],
        };
    }
    target.role = ReplicaRole::ActiveSecondary;
    pair.publish(source, target);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn m3_same_primary_epoch_continues_actual_kv_sender_receiver_and_previous_quorum() {
    use crate::replicator::v1::service_adapter::tests::kv::{KvProvider, store};
    use kvstore2::v1_test_bridge::Assignment;

    let directory = std::path::PathBuf::from(".paw/work/sf-hosting-m3")
        .join(format!("phase2-native-kv-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&directory).unwrap();
    let source_kv = store(&directory.join("source"));
    let target_kv = store(&directory.join("target"));
    let pair = Pair::new().await;
    let (source, target) = pair
        .engines(
            Arc::new(KvProvider(source_kv.clone())),
            Arc::new(KvProvider(target_kv.clone())),
        )
        .await;
    source_kv.promote().unwrap();
    target_kv.begin_copy().unwrap();
    let mut info = information(2);
    info.replication_address = pair.target.local().address.clone();
    info.storage_incarnation = Some(pair.target.local().storage.clone());
    let primary = source.primary_replicator().unwrap();
    let build = tokio::spawn(async move { primary.build_replica(info).await });
    let mut copy = target
        .state_replicator()
        .unwrap()
        .copy_operations()
        .unwrap();
    loop {
        let operation = tokio::time::timeout(Duration::from_secs(5), copy.get_operation())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        if !operation.end_of_stream {
            target_kv.install_copy(operation.data.clone()).unwrap();
        }
        operation.acknowledge().unwrap();
        if operation.end_of_stream {
            break;
        }
    }
    target_kv.finish_copy().unwrap();
    tokio::time::timeout(Duration::from_secs(5), build)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    target
        .replicator()
        .change_role(epoch(1), ReplicaRole::ActiveSecondary)
        .await
        .unwrap();
    let current = configuration(&pair, true);
    source
        .primary_replicator()
        .unwrap()
        .update_current_replica_set_configuration(current.clone())
        .await
        .unwrap();
    publish_configuration(&pair, &current, None, 0);
    let old = pair.source.sessions.binding_for(2).unwrap();
    let native = pair.source.test_production().removal_primary();
    let sender = native.removal_session(2);
    let build_owner = native.removal_build(2);
    let old_incarnation = sender.identity().remote_incarnation;
    let receiver = pair.target.test_production().drainage_secondary();
    let mut operations = target
        .state_replicator()
        .unwrap()
        .replication_operations()
        .unwrap();
    let start_write = |value: &str| {
        let state = source.state_replicator().unwrap();
        source_kv
            .begin_put("k".into(), value.into(), move |data| {
                let operation = state.begin_replicate(data).map_err(std::io::Error::other)?;
                Ok(Assignment {
                    lsn: operation.lsn,
                    completion: Box::pin(async move {
                        operation.completion.await.map_err(std::io::Error::other)
                    }),
                })
            })
            .unwrap()
    };
    for (index, value) in ["acknowledged-before-epoch", "continued-previous-voter"]
        .into_iter()
        .enumerate()
    {
        let mut queued = None;
        if index == 1 {
            let late_ack = receiver.transport_ack();
            source
                .replicator()
                .change_role(epoch(2), ReplicaRole::Primary)
                .await
                .unwrap();
            target
                .replicator()
                .change_role(epoch(2), ReplicaRole::ActiveSecondary)
                .await
                .unwrap();
            assert!(!pair.source.test_production().changing_role());
            assert!(!pair.target.test_production().changing_role());
            let mut previous = current.clone();
            previous.replicas[0].current_progress = source_kv.progress().unwrap();
            previous.replicas[0].catch_up_capability =
                target.replicator().catch_up_capability().await.unwrap();
            let mut next = configuration(&pair, false);
            next.configuration = ConfigurationDescriptor::new(
                epoch(2),
                next.configuration.primary_id,
                next.configuration.members,
                1,
            );
            publish_configuration(&pair, &next, Some(&previous), source_kv.progress().unwrap());
            let mut fresh = Binding::new(
                pair.source_access.publication.lock().unwrap().clone(),
                pair.target_access.publication.lock().unwrap().clone(),
            )
            .unwrap();
            assert!(pair.source.sessions.proposal(fresh.clone()).is_err());
            assert!(pair.target.sessions.reserve(&fresh).is_err());
            fresh.continuation = Some(old.nonce.clone());
            assert!(pair.source.sessions.proposal(fresh.clone()).is_err());
            assert!(pair.target.sessions.reserve(&fresh).is_err());
            let before = pair.target.sessions.inspect();
            let mut crossed = fresh.clone();
            crossed.continuation = Some(uuid::Uuid::new_v4().to_string());
            assert!(
                pair.source
                    .control(&crossed, Control::CheckContinuation)
                    .await
                    .is_err()
            );
            assert!(
                pair.source
                    .control(&crossed, Control::Continue)
                    .await
                    .is_err()
            );
            assert_eq!(pair.target.sessions.inspect(), before);
            let retiring_route =
                super::super::super::super::production_transport::route(&old, pair.target.local())
                    .unwrap();
            let retiring =
                Pump::start_route(&pair.target, old.clone(), retiring_route.clone()).await;
            let copy = retiring_route
                .reversed()
                .envelope(wire::envelope::Body::StartCopy(wire::StartCopy {
                    epoch: Some(wire::Epoch {
                        data_loss: 1,
                        configuration: 1,
                    }),
                    replica: 2,
                    replication_start: 1,
                }));
            let id = copy.message_id.clone();
            retiring.remote.try_send(copy, None).unwrap();
            until(|| {
                pair.target.removal_receivers()["ingress"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|item| item[0] == id && item[1] == "sessionRejected")
            })
            .await;
            retiring.stop().await;
            assert!(receiver.same_instance(&pair.target.test_production().drainage_secondary()));
            let mut pending = start_write(value);
            assert!(pending.wait().now_or_never().is_none());
            assert_eq!(target_kv.progress().unwrap(), 1);
            queued = Some(pending);
            source
                .primary_replicator()
                .unwrap()
                .update_catch_up_replica_set_configuration(next, previous)
                .await
                .unwrap();
            let live = pair.source.sessions.binding_for(2).unwrap();
            assert_ne!(live.nonce, old.nonce);
            assert_eq!(live.continuation.as_ref(), Some(&old.nonce));
            assert!(pair.source.sessions.completed(&live));
            assert!(pair.target.sessions.completed(&live));
            assert!(pair.source.sessions.active(&old).is_err());
            assert!(pair.target.sessions.active(&old).is_err());
            assert!(pair.source.control(&old, Control::Complete).await.is_err());
            assert!(pair.target.control(&old, Control::Confirm).await.is_err());
            assert_eq!(native.removal_membership(2), (false, true, false));
            assert!(sender.same_instance(&native.removal_session(2)));
            assert!(build_owner.same_instance(&native.removal_build(2)));
            assert!(sender.identity().remote_incarnation > old_incarnation);
            assert_eq!(
                sender.identity().remote_incarnation,
                pair.source.native_incarnation(&live).unwrap()
            );
            assert!(receiver.same_instance(&pair.target.test_production().drainage_secondary()));
            let before = pair.target.pending_sends(&live);
            let secondary_route =
                super::super::super::super::production_transport::route(&live, pair.target.local())
                    .unwrap();
            assert!(pair.target.ack(&secondary_route, late_ack));
            assert_eq!(
                pair.target.pending_sends(&live),
                before,
                "old native ACK snapshot must not acquire the new nonce"
            );
            let source_route =
                super::super::super::super::production_transport::route(&old, pair.source.local())
                    .unwrap();
            let stale = Pump::start_route(&pair.source, old.clone(), source_route.clone()).await;
            let forged_ack =
                source_route
                    .reversed()
                    .envelope(wire::envelope::Body::ReplicationAck(wire::ReplicationAck {
                        replication_received: 2,
                        replication_applied: 2,
                        copy_received: -1,
                        copy_applied: -1,
                        error: 0,
                    }));
            let id = forged_ack.message_id.clone();
            stale.remote.try_send(forged_ack, None).unwrap();
            until(|| {
                pair.source.removal_receivers()["ingress"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|item| item[0] == id && item[1] == "sessionRejected")
            })
            .await;
            assert!(
                queued.as_mut().unwrap().wait().now_or_never().is_none(),
                "old ACK cannot settle the retained write"
            );
            stale.stop().await;
            let target_route = secondary_route.clone();
            let stale_target =
                Pump::start_route(&pair.target, old.clone(), target_route.clone()).await;
            let copy = target_route
                .reversed()
                .envelope(wire::envelope::Body::StartCopy(wire::StartCopy {
                    epoch: Some(wire::Epoch {
                        data_loss: 1,
                        configuration: 1,
                    }),
                    replica: 2,
                    replication_start: 1,
                }));
            let id = copy.message_id.clone();
            stale_target.remote.try_send(copy, None).unwrap();
            until(|| {
                pair.target.removal_receivers()["ingress"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|item| item[0] == id && item[1] == "sessionRejected")
            })
            .await;
            stale_target.stop().await;
            assert!(receiver.same_instance(&pair.target.test_production().drainage_secondary()));
            let live_pump =
                Pump::start_route(&pair.target, live.clone(), target_route.clone()).await;
            for (body, outcome) in [
                (
                    wire::envelope::Body::StartCopy(wire::StartCopy {
                        epoch: Some(wire::Epoch {
                            data_loss: 1,
                            configuration: 2,
                        }),
                        replica: 2,
                        replication_start: 1,
                    }),
                    "continuationCopyRejected",
                ),
                (
                    wire::envelope::Body::ReplicationOperation(wire::ReplicationOperation {
                        metadata: Some(metadata(2)),
                        original_epoch: Some(wire::Epoch {
                            data_loss: 1,
                            configuration: 1,
                        }),
                        sending_epoch: Some(wire::Epoch {
                            data_loss: 1,
                            configuration: 1,
                        }),
                        first_sequence: 2,
                        last_sequence: 2,
                        last_in_batch: 2,
                        buffer_counts: vec![1],
                        buffers: Some(wire::Buffers {
                            segments: vec![Bytes::from_static(b"obsolete")],
                        }),
                        completed_hint: 1,
                    }),
                    "epochRejected",
                ),
            ] {
                let envelope = target_route.reversed().envelope(body);
                let id = envelope.message_id.clone();
                live_pump.remote.try_send(envelope, None).unwrap();
                until(|| {
                    pair.target.removal_receivers()["ingress"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|item| item[0] == id && item[1] == outcome)
                })
                .await;
            }
            live_pump.stop().await;
            let before = pair.source.pending_sends(&live);
            assert!(pair.source.replication(SendIntent {
                identity: super::super::super::super::super::session::SessionIdentity {
                    primary_endpoint: source_route.sender_address.clone(),
                    remote_endpoint: source_route.remote_address.clone(),
                    remote_incarnation: old_incarnation,
                },
                kind: SendKind::StartCopy {
                    replication_start: 1,
                    epoch: epoch(2)
                },
                expires_after: None,
            }));
            assert_eq!(pair.source.pending_sends(&live), before);
            let before = pair.source.test_production().inspect();
            let mut fresh_build = information(2);
            fresh_build.replication_address = pair.target.local().address.clone();
            fresh_build.storage_incarnation = Some(pair.target.local().storage.clone());
            assert!(
                source
                    .primary_replicator()
                    .unwrap()
                    .build_replica(fresh_build)
                    .await
                    .is_err()
            );
            assert_eq!(pair.source.test_production().inspect(), before);
            assert_eq!(
                source_kv.get("k", || Ok(())).unwrap().as_deref(),
                Some("acknowledged-before-epoch")
            );
            assert_eq!(
                target_kv.get("k", || Ok(())).unwrap().as_deref(),
                Some("acknowledged-before-epoch")
            );
        }
        let mut write = queued.unwrap_or_else(|| start_write(value));
        let operation = tokio::time::timeout(Duration::from_secs(5), operations.get_operation())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(
            write.wait().now_or_never().is_none(),
            "PC/CC still requires the actual secondary"
        );
        target_kv
            .apply_secondary(operation.lsn, operation.data.clone())
            .unwrap();
        operation.acknowledge().unwrap();
        tokio::time::timeout(Duration::from_secs(5), write.wait())
            .await
            .unwrap()
            .unwrap();
        for mode in [ReplicaSetQuorumMode::WriteQuorum, ReplicaSetQuorumMode::All] {
            tokio::time::timeout(
                Duration::from_secs(5),
                source
                    .primary_replicator()
                    .unwrap()
                    .wait_for_catch_up_quorum(mode),
            )
            .await
            .unwrap()
            .unwrap();
        }
        assert_eq!(
            source_kv.get("k", || Ok(())).unwrap().as_deref(),
            Some(value)
        );
        assert_eq!(
            target_kv.get("k", || Ok(())).unwrap().as_deref(),
            Some(value)
        );
    }
    let mut final_current = configuration(&pair, false);
    final_current.configuration = ConfigurationDescriptor::new(
        epoch(2),
        final_current.configuration.primary_id,
        final_current.configuration.members,
        1,
    );
    source
        .primary_replicator()
        .unwrap()
        .update_current_replica_set_configuration(final_current)
        .await
        .unwrap();
    assert_eq!(native.removal_membership(2), (false, false, false));
    let continued = pair.source.sessions.binding_for(2).unwrap();
    source
        .primary_replicator()
        .unwrap()
        .remove_replica(pair.target.local().identity.replica_id)
        .await
        .unwrap();
    assert!(pair.source.sessions.binding_for(2).is_none());
    assert!(pair.source.sessions.active(&continued).is_err());
    assert!(pair.target.sessions.active(&continued).is_err());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut publication = pair.binding.target.clone();
    publication.scope.storage.replica.instance_id = ReplicaInstanceId::new("same-id-new-storage");
    publication.scope.storage.storage = ReplicaStorageIdentity::Persistent {
        pod_uid: PodUid::new("replacement-pod"),
        pvc_uid: PvcUid::new("replacement-pvc"),
    };
    publication.scope.storage.storage_generation = StorageGeneration::new("replacement-storage");
    publication.scope.process_session_id = ProcessSessionId::new("replacement-process");
    publication.scope.creation_id = ReplicatorCreationId::new("replacement-creation");
    publication.replication_address = format!("http://{}", listener.local_addr().unwrap());
    publication.scope.epoch = epoch(2);
    publication.scope.configuration = ReplicaConfigurationScope {
        current: configuration(&pair, false).configuration,
        previous: None,
        replicas: Vec::new(),
    };
    publication.scope.configuration.current = ConfigurationDescriptor::new(
        epoch(2),
        pair.source.local().identity.replica_id,
        publication.scope.configuration.current.members.clone(),
        1,
    );
    let mut primary_publication = pair.source_access.publication.lock().unwrap().clone();
    primary_publication.scope.configuration = publication.scope.configuration.clone();
    primary_publication.generation += 1;
    *pair.source_access.publication.lock().unwrap() = primary_publication.clone();
    pair.source.test_publish(vec![publication.clone()]);
    let access = Pair::access(publication.clone());
    let context = Pair::context(&publication, access.clone());
    let registry = MessageRegistry::default();
    let fresh = Transport::test_owner(
        context.clone(),
        Endpoint::publication(&publication),
        pair.source.history().clone(),
        vec![primary_publication.clone()],
        registry.clone(),
    );
    fresh.test_listen(listener);
    let fresh_kv = store(&directory.join("replacement"));
    let fresh_engine = test_compose(
        context,
        Arc::new(KvProvider(fresh_kv.clone())),
        registry,
        fresh.clone(),
    )
    .await
    .unwrap();
    fresh_engine.replicator().open().await.unwrap();
    fresh_engine
        .replicator()
        .change_role(epoch(2), ReplicaRole::IdleSecondary)
        .await
        .unwrap();
    fresh_kv.begin_copy().unwrap();
    let mut info = information(2);
    info.identity = publication.scope.storage.replica.clone();
    info.process_session_id = publication.scope.process_session_id.clone();
    info.replication_address = publication.replication_address.clone();
    info.storage_incarnation = Some(publication.scope.storage.clone());
    let mut admitted = info.clone();
    let primary = source.primary_replicator().unwrap();
    let build = tokio::spawn(async move { primary.build_replica(info).await });
    let mut copy = fresh_engine
        .state_replicator()
        .unwrap()
        .copy_operations()
        .unwrap();
    loop {
        let operation = tokio::time::timeout(Duration::from_secs(5), copy.get_operation())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        if !operation.end_of_stream {
            fresh_kv.install_copy(operation.data.clone()).unwrap();
        }
        operation.acknowledge().unwrap();
        if operation.end_of_stream {
            break;
        }
    }
    fresh_kv.finish_copy().unwrap();
    tokio::time::timeout(Duration::from_secs(5), build)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(
        fresh_kv.get("k", || Ok(())).unwrap().as_deref(),
        Some("continued-previous-voter")
    );
    let replacement = pair.source.sessions.binding_for(2).unwrap();
    assert!(replacement.continuation.is_none());
    assert!(!sender.same_instance(&native.removal_session(2)));
    assert!(pair.source.sessions.active(&old).is_err());
    assert!(pair.source.sessions.active(&continued).is_err());
    fresh_engine
        .replicator()
        .change_role(epoch(2), ReplicaRole::ActiveSecondary)
        .await
        .unwrap();
    admitted.role = ReplicaRole::ActiveSecondary;
    admitted.current_progress = fresh_engine.replicator().current_progress().await.unwrap();
    admitted.catch_up_capability = fresh_engine
        .replicator()
        .catch_up_capability()
        .await
        .unwrap();
    let joined = ReplicaSetConfiguration {
        configuration: ConfigurationDescriptor::new(
            epoch(2),
            pair.source.local().identity.replica_id,
            vec![
                ConfigurationMember {
                    identity: pair.source.local().identity.clone(),
                    role: ReplicaRole::Primary,
                },
                ConfigurationMember {
                    identity: admitted.identity.clone(),
                    role: ReplicaRole::ActiveSecondary,
                },
            ],
            2,
        ),
        replicas: vec![admitted.clone()],
    };
    source
        .primary_replicator()
        .unwrap()
        .update_current_replica_set_configuration(joined.clone())
        .await
        .unwrap();
    primary_publication.scope.configuration = ReplicaConfigurationScope {
        current: joined.configuration.clone(),
        previous: None,
        replicas: vec![ReplicaPeerDescription {
            identity: admitted.identity,
            replication_address: admitted.replication_address,
            process_session_id: admitted.process_session_id,
            storage_incarnation: admitted.storage_incarnation.unwrap(),
            role: ReplicaRole::ActiveSecondary,
            current_progress: admitted.current_progress,
            catch_up_capability: admitted.catch_up_capability,
            is_up: true,
            is_ready: true,
            must_catchup: false,
        }],
    };
    publication.scope.configuration = ReplicaConfigurationScope {
        current: joined.configuration,
        previous: None,
        replicas: vec![ReplicaPeerDescription {
            identity: primary_publication.scope.storage.replica.clone(),
            replication_address: primary_publication.replication_address.clone(),
            process_session_id: primary_publication.scope.process_session_id.clone(),
            storage_incarnation: primary_publication.scope.storage.clone(),
            role: ReplicaRole::Primary,
            current_progress: admitted.current_progress,
            catch_up_capability: admitted.catch_up_capability,
            is_up: true,
            is_ready: true,
            must_catchup: false,
        }],
    };
    publication.role = ReplicaRole::ActiveSecondary;
    *pair.source_access.publication.lock().unwrap() = primary_publication.clone();
    *access.publication.lock().unwrap() = publication.clone();
    pair.source.test_publish(vec![publication]);
    fresh.test_publish(vec![primary_publication]);
    let mut operations = fresh_engine
        .state_replicator()
        .unwrap()
        .replication_operations()
        .unwrap();
    let mut write = start_write("same-id-replacement-write");
    let operation = tokio::time::timeout(Duration::from_secs(5), operations.get_operation())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    fresh_kv
        .apply_secondary(operation.lsn, operation.data.clone())
        .unwrap();
    operation.acknowledge().unwrap();
    tokio::time::timeout(Duration::from_secs(5), write.wait())
        .await
        .unwrap()
        .unwrap();
    tokio::time::timeout(
        Duration::from_secs(5),
        source
            .primary_replicator()
            .unwrap()
            .wait_for_catch_up_quorum(ReplicaSetQuorumMode::All),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(
        source_kv.get("k", || Ok(())).unwrap().as_deref(),
        Some("same-id-replacement-write")
    );
    assert_eq!(
        fresh_kv.get("k", || Ok(())).unwrap().as_deref(),
        Some("same-id-replacement-write")
    );
    assert_eq!(
        target_kv.get("k", || Ok(())).unwrap().as_deref(),
        Some("continued-previous-voter")
    );
    close(&[&source, &target, &fresh_engine]).await;
    fresh_kv.close().await.unwrap();
    fresh.drain().await.unwrap();
    source_kv.close().await.unwrap();
    target_kv.close().await.unwrap();
    let reopened = store(&directory.join("target"));
    assert_eq!(
        reopened.get("k", || Ok(())).unwrap().as_deref(),
        Some("continued-previous-voter")
    );
    reopened.close().await.unwrap();
    pair.finish().await;
}

async fn established_pair(
    pair: &Pair,
    provider: Arc<Provider>,
) -> (
    ReplicatorInterfaces,
    ReplicatorInterfaces,
    ReplicaSetConfiguration,
) {
    let (source, target) = pair.engines(provider, Arc::new(Provider::new())).await;
    complete_build(
        &source,
        &target,
        &pair.binding.target,
        [&pair.source, &pair.target],
    )
    .await;
    target
        .replicator()
        .change_role(epoch(1), ReplicaRole::ActiveSecondary)
        .await
        .unwrap();
    let current = configuration(pair, true);
    source
        .primary_replicator()
        .unwrap()
        .update_current_replica_set_configuration(current.clone())
        .await
        .unwrap();
    publish_configuration(pair, &current, None, 0);
    write_and_catchup(&source, &target).await;
    (source, target, current)
}

fn next_configuration(current: &ReplicaSetConfiguration) -> ReplicaSetConfiguration {
    let mut next = current.clone();
    next.configuration = ConfigurationDescriptor::new(
        epoch(2),
        current.configuration.primary_id,
        current.configuration.members.clone(),
        current.configuration.write_quorum,
    );
    for information in &mut next.replicas {
        information.current_progress = 1;
        information.catch_up_capability = 1;
    }
    next
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn m3_continuation_waits_for_real_epoch_provider_and_current_only_callback() {
    let pair = Pair::new().await;
    let provider = Arc::new(Provider::new());
    let (source, target, current) = established_pair(&pair, provider.clone()).await;
    let old = pair.source.sessions.binding_for(2).unwrap();
    let sender = pair
        .source
        .test_production()
        .removal_primary()
        .removal_session(2);
    provider.epoch_gate.forget_permits(usize::MAX);
    let control = source.replicator();
    let advance =
        tokio::spawn(async move { control.change_role(epoch(2), ReplicaRole::Primary).await });
    until(|| {
        provider
            .epochs
            .lock()
            .unwrap()
            .iter()
            .any(|(epoch_selected, _)| *epoch_selected == epoch(2))
    })
    .await;
    assert!(!advance.is_finished());
    assert!(pair.source.sessions.active(&old).is_err());
    assert!(!pair.source.test_production().changing_role());
    let next = next_configuration(&current);
    publish_configuration(&pair, &next, None, 1);
    let source_state = pair.source.sessions.inspect();
    let target_state = pair.target.sessions.inspect();
    assert!(
        source
            .primary_replicator()
            .unwrap()
            .update_current_replica_set_configuration(next.clone())
            .await
            .is_err()
    );
    assert_eq!(pair.source.sessions.inspect(), source_state);
    assert_eq!(pair.target.sessions.inspect(), target_state);
    provider.epoch_gate.add_permits(1);
    advance.await.unwrap().unwrap();
    assert_eq!(
        pair.source.sessions.inspect(),
        source_state,
        "provider completion alone cannot select continuation"
    );
    assert!(
        source
            .primary_replicator()
            .unwrap()
            .update_current_replica_set_configuration(next.clone())
            .await
            .is_err(),
        "matching publications cannot substitute for the target's native epoch owner"
    );
    assert_eq!(pair.source.sessions.inspect(), source_state);
    assert_eq!(pair.target.sessions.inspect(), target_state);
    target
        .replicator()
        .change_role(epoch(2), ReplicaRole::ActiveSecondary)
        .await
        .unwrap();
    source
        .primary_replicator()
        .unwrap()
        .update_current_replica_set_configuration(next)
        .await
        .unwrap();
    let live = pair.source.sessions.binding_for(2).unwrap();
    assert!(pair.source.sessions.completed(&live));
    assert!(pair.target.sessions.completed(&live));
    assert_ne!(old.nonce, live.nonce);
    assert!(
        sender.same_instance(
            &pair
                .source
                .test_production()
                .removal_primary()
                .removal_session(2)
        )
    );
    write_and_catchup(&source, &target).await;
    close(&[&source, &target]).await;
    pair.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn m3_continuation_retains_ack_join_after_configuration_caller_drop_without_old_credit() {
    let pair = Pair::new().await;
    let (source, target, current) = established_pair(&pair, Arc::new(Provider::new())).await;
    let old = pair.source.sessions.binding_for(2).unwrap();
    let sender = pair.source.acknowledgements();
    let permit = sender.reserve_owned().await.unwrap();
    let credits = Arc::new(AtomicUsize::new(0));
    let credit = credits.clone();
    pair.source_access.validated.forget_permits(usize::MAX);
    pair.source_access.pause.store(true, Ordering::SeqCst);
    pair.source
        .sessions
        .effect(&old, || {
            pair.source.queue_ack(
                old.clone(),
                permit,
                Box::new(move || {
                    credit.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                }),
            )
        })
        .unwrap()
        .unwrap();
    validated(&pair.source_access).await;
    source
        .replicator()
        .change_role(epoch(2), ReplicaRole::Primary)
        .await
        .unwrap();
    target
        .replicator()
        .change_role(epoch(2), ReplicaRole::ActiveSecondary)
        .await
        .unwrap();
    let next = next_configuration(&current);
    publish_configuration(&pair, &next, None, 1);
    let primary = source.primary_replicator().unwrap();
    let selected = next.clone();
    let callback = tokio::spawn(async move {
        primary
            .update_current_replica_set_configuration(selected)
            .await
    });
    until(|| pair.source.native_incarnation(&old).is_err()).await;
    assert!(
        !callback.is_finished(),
        "actual ACK settlement still owns the continuation join"
    );
    assert_eq!(pair.source.sessions.binding_for(2), Some(old.clone()));
    assert_eq!(pair.target.sessions.inbound(), Some(old.clone()));
    callback.abort();
    assert!(callback.await.unwrap_err().is_cancelled());
    pair.source_access.release.add_permits(1);
    until(|| {
        pair.source.sessions.binding_for(2).is_some_and(|binding| {
            binding.nonce != old.nonce && pair.source.sessions.completed(&binding)
        })
    })
    .await;
    assert_eq!(credits.load(Ordering::SeqCst), 0);
    assert!(pair.source.sessions.active(&old).is_err());
    source
        .primary_replicator()
        .unwrap()
        .update_current_replica_set_configuration(next)
        .await
        .unwrap();
    write_and_catchup(&source, &target).await;
    close(&[&source, &target]).await;
    pair.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn m3_continuation_refuses_unapplied_native_receiver_without_reset_or_fresh_build() {
    let pair = Pair::new().await;
    let (source, target, current) = established_pair(&pair, Arc::new(Provider::new())).await;
    let old = pair.source.sessions.binding_for(2).unwrap();
    let receiver = pair.target.test_production().drainage_secondary();
    let mut stream = target
        .state_replicator()
        .unwrap()
        .replication_operations()
        .unwrap();
    let write = source
        .state_replicator()
        .unwrap()
        .begin_replicate(Bytes::from_static(b"unapplied-native-operation"))
        .unwrap();
    let operation = tokio::time::timeout(Duration::from_secs(5), stream.get_operation())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(operation.lsn, write.lsn);
    let target_progress = target.replicator().current_progress().await.unwrap();
    let target_capability = target.replicator().catch_up_capability().await.unwrap();
    source
        .replicator()
        .change_role(epoch(2), ReplicaRole::Primary)
        .await
        .unwrap();
    let control = target.replicator();
    let advance = tokio::spawn(async move {
        control
            .change_role(epoch(2), ReplicaRole::ActiveSecondary)
            .await
    });
    until(|| pair.target.sessions.active(&old).is_err()).await;
    assert!(
        !advance.is_finished(),
        "native epoch drainage must retain the unapplied receiver"
    );
    let mut next = next_configuration(&current);
    next.replicas[0].current_progress = target_progress;
    next.replicas[0].catch_up_capability = target_capability;
    publish_configuration(&pair, &next, None, next.replicas[0].current_progress);
    let before = pair.source.sessions.inspect();
    let target_before = pair.target.sessions.inspect();
    assert!(
        source
            .primary_replicator()
            .unwrap()
            .update_current_replica_set_configuration(next)
            .await
            .is_err()
    );
    assert_eq!(pair.source.sessions.inspect(), before);
    assert_eq!(pair.target.sessions.inspect(), target_before);
    assert!(receiver.same_instance(&pair.target.test_production().drainage_secondary()));
    assert!(
        operation.acknowledge().is_err(),
        "a handed-out old ACK stays fenced"
    );
    let mut info = information(2);
    info.replication_address = pair.target.local().address.clone();
    info.storage_incarnation = Some(pair.target.local().storage.clone());
    assert!(
        source
            .primary_replicator()
            .unwrap()
            .build_replica(info)
            .await
            .is_err()
    );
    assert_eq!(pair.source.sessions.binding_for(2), Some(old));
    assert_eq!(pair.target.sessions.inspect(), target_before);
    close(&[&source, &target]).await;
    assert!(advance.await.unwrap().is_err());
    assert!(write.completion.await.is_err());
    pair.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn m2_remove_rejects_completed_idle_previous_only_voter_without_retiring_owners() {
    let pair = Pair::new().await;
    let (source, target) = pair
        .engines(Arc::new(Provider::new()), Arc::new(Provider::new()))
        .await;
    complete_build(
        &source,
        &target,
        &pair.binding.target,
        [&pair.source, &pair.target],
    )
    .await;
    let binding = pair.source.sessions.binding_for(2).unwrap();
    assert!(pair.source.sessions.completed(&binding));
    assert!(pair.target.sessions.completed(&binding));
    source
        .primary_replicator()
        .unwrap()
        .update_catch_up_replica_set_configuration(
            configuration(&pair, false),
            configuration(&pair, true),
        )
        .await
        .unwrap();
    target
        .replicator()
        .change_role(epoch(1), ReplicaRole::ActiveSecondary)
        .await
        .unwrap();
    let native = pair.source.test_production().removal_primary();
    assert_eq!(native.removal_membership(2), (false, true, true));
    write_and_catchup(&source, &target).await;
    let native_owner = native.removal_session(2);
    let native_build = native.removal_build(2);
    assert!(native_build.result().unwrap().result.is_ok());
    let receiver = pair.target.removal_receivers()["receivers"].clone();
    let source_sessions = pair.source.sessions.inspect();
    let target_sessions = pair.target.sessions.inspect();
    let source_peers = pair.source.inspect_peers();
    let target_peers = pair.target.inspect_peers();
    let source_acks = pair.source.inspect_acks();
    let target_acks = pair.target.inspect_acks();
    let production = pair.source.test_production().inspect();
    let progress = target.replicator().current_progress().await.unwrap();
    let error = source
        .primary_replicator()
        .unwrap()
        .remove_replica(pair.target.local().identity.replica_id)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("IdleNotFound"), "{error}");
    assert_eq!(native.removal_membership(2), (false, true, true));
    assert!(native_owner.same_instance(&native.removal_session(2)));
    assert!(native_build.same_instance(&native.removal_build(2)));
    assert_eq!(pair.target.removal_receivers()["receivers"], receiver);
    assert_eq!(pair.source.sessions.inspect(), source_sessions);
    assert_eq!(pair.target.sessions.inspect(), target_sessions);
    assert_eq!(pair.source.inspect_peers(), source_peers);
    assert_eq!(pair.target.inspect_peers(), target_peers);
    assert_eq!(pair.source.inspect_acks(), source_acks);
    assert_eq!(pair.target.inspect_acks(), target_acks);
    assert_eq!(pair.source.test_production().inspect(), production);
    assert_eq!(
        target.replicator().current_progress().await.unwrap(),
        progress
    );
    write_and_catchup(&source, &target).await;
    assert!(native_owner.same_instance(&native.removal_session(2)));
    assert!(native_build.same_instance(&native.removal_build(2)));
    close(&[&source, &target]).await;
    pair.finish().await;
}

struct BlockingCut {
    entered: Semaphore,
    released: Mutex<bool>,
    release: Condvar,
}

impl BlockingCut {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            entered: Semaphore::new(0),
            released: Mutex::new(false),
            release: Condvar::new(),
        })
    }

    fn block(&self) {
        self.entered.add_permits(1);
        let (released, _) = self
            .release
            .wait_timeout_while(
                self.released.lock().unwrap(),
                Duration::from_secs(10),
                |released| !*released,
            )
            .unwrap();
        assert!(*released, "actual synchronous lock cut was not released");
    }

    async fn entered(&self) {
        tokio::time::timeout(Duration::from_secs(5), self.entered.acquire())
            .await
            .unwrap()
            .unwrap()
            .forget();
    }

    fn open(&self) {
        *self.released.lock().unwrap() = true;
        self.release.notify_all();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn m2_remove_and_real_other_peer_ingress_share_gate_native_order_and_join() {
    let pair = Pair::new().await;
    let (source, target) = pair
        .engines(Arc::new(Provider::new()), Arc::new(Provider::new()))
        .await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut idle_publication = pair.binding.target.clone();
    idle_publication.scope.storage.replica.replica_id = crate::protocol::types::ReplicaId::new(3);
    idle_publication.scope.storage.replica.instance_id =
        crate::protocol::types::ReplicaInstanceId::new("third-idle");
    idle_publication.scope.process_session_id = ProcessSessionId::new("third-process");
    idle_publication.scope.creation_id = ReplicatorCreationId::new("third-creation");
    idle_publication.replication_address = format!("http://{}", listener.local_addr().unwrap());
    let access = Pair::access(idle_publication.clone());
    let context = Pair::context(&idle_publication, access);
    let registry = MessageRegistry::default();
    let idle = Transport::test_owner(
        context.clone(),
        crate::replicator::v1::service_adapter::engine_session::Endpoint::publication(
            &idle_publication,
        ),
        pair.source.history().clone(),
        vec![pair.binding.source.clone()],
        registry.clone(),
    );
    idle.test_listen(listener);
    pair.source
        .test_publish(vec![pair.binding.target.clone(), idle_publication.clone()]);
    let idle_engine = test_compose(context, Arc::new(Provider::new()), registry, idle.clone())
        .await
        .unwrap();
    idle_engine.replicator().open().await.unwrap();
    idle_engine
        .replicator()
        .change_role(epoch(1), ReplicaRole::IdleSecondary)
        .await
        .unwrap();
    complete_build(
        &source,
        &target,
        &pair.binding.target,
        [&pair.source, &pair.target],
    )
    .await;
    complete_build(
        &source,
        &idle_engine,
        &idle_publication,
        [&pair.source, &idle],
    )
    .await;
    source
        .primary_replicator()
        .unwrap()
        .update_current_replica_set_configuration(configuration(&pair, true))
        .await
        .unwrap();
    target
        .replicator()
        .change_role(epoch(1), ReplicaRole::ActiveSecondary)
        .await
        .unwrap();
    let native = pair.source.test_production().removal_primary();
    assert_eq!(native.removal_membership(3), (false, false, true));
    let mut operations = target
        .state_replicator()
        .unwrap()
        .replication_operations()
        .unwrap();
    let write = source
        .state_replicator()
        .unwrap()
        .begin_replicate(Bytes::from_static(b"concurrent-healthy-peer"))
        .unwrap();
    let completion = tokio::spawn(write.completion);
    let operation = tokio::time::timeout(Duration::from_secs(5), operations.get_operation())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(operation.lsn, write.lsn);
    assert_eq!(
        operation.data,
        Bytes::from_static(b"concurrent-healthy-peer")
    );
    let removal = BlockingCut::new();
    let dispatch = BlockingCut::new();
    let attempting = Arc::new(Semaphore::new(0));
    let threads = Arc::new(Mutex::new(Vec::new()));
    *pair.source.sessions.1.lock().unwrap() = Some(Arc::new({
        let removal = removal.clone();
        let dispatch = dispatch.clone();
        let threads = threads.clone();
        let attempting = attempting.clone();
        let armed = AtomicBool::new(true);
        move |cut| match cut {
            RemovalCut::GateRequested => {
                threads.lock().unwrap().push(std::thread::current().id());
                removal.block();
                attempting.add_permits(1);
            }
            RemovalCut::Dispatch {
                replica: 2,
                applied,
            } if applied >= write.lsn && armed.swap(false, Ordering::SeqCst) => {
                threads.lock().unwrap().push(std::thread::current().id());
                dispatch.block();
            }
            _ => {}
        }
    }));
    let primary = source.primary_replicator().unwrap();
    let remove = tokio::spawn(async move {
        primary
            .remove_replica(crate::protocol::types::ReplicaId::new(3))
            .await
    });
    removal.entered().await;
    operation.acknowledge().unwrap();
    dispatch.entered().await;
    removal.open();
    tokio::time::timeout(Duration::from_secs(5), attempting.acquire())
        .await
        .unwrap()
        .unwrap()
        .forget();
    // Real ingress owns the session gate. Remove has not taken native locks.
    let probe = tokio::task::spawn_blocking(move || native.current_progress());
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), probe)
            .await
            .unwrap()
            .unwrap()
            .unwrap(),
        0
    );
    assert!(!remove.is_finished());
    assert!(!completion.is_finished());
    dispatch.open();
    tokio::time::timeout(Duration::from_secs(5), completion)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), remove)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    {
        let threads = threads.lock().unwrap();
        assert_eq!(threads.len(), 2);
        assert_ne!(
            threads[0], threads[1],
            "cuts must run on distinct OS threads"
        );
    }
    *pair.source.sessions.1.lock().unwrap() = None;
    assert_eq!(idle.sessions.inspect()["entries"][0]["phase"], "Retired");
    assert_eq!(
        pair.source
            .test_production()
            .removal_primary()
            .removal_membership(3),
        (false, false, false)
    );
    write_and_catchup(&source, &target).await;
    close(&[&source, &target, &idle_engine]).await;
    tokio::time::timeout(Duration::from_secs(5), idle.drain())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(idle.cardinality(), 0);
    pair.finish().await;
}
