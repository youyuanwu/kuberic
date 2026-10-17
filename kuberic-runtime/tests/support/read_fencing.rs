use super::*;

pub(super) async fn scalar_progress(replica: &HostedMemoryReplica) -> i64 {
    replica
        .application
        .control
        .read()
        .await
        .clone()
        .unwrap()
        .current_progress()
        .await
        .unwrap()
}

#[tokio::test]
async fn sf_d02_primary_scalar_waits_for_quorum_and_local_commit_across_cancel_and_epoch() {
    tokio::time::timeout(Duration::from_secs(90), async {
        for outcome in ["success", "cancel", "epoch"] {
            let pair = AuthorizedHostedPair::start(&format!("sf-d02-{outcome}"), 512).await;
            let mut operations = pair.secondary.application.control_operations();
            pair.primary.state.block_commit();
            let write = tokio::spawn({
                let application = pair.primary.application.clone();
                async move {
                    application
                        .replicate(Bytes::from_static(b"sf-d02-value"))
                        .await
                }
            });
            let operation = delivered(&mut operations).await;
            let accepted = operation
                .apply_to(pair.secondary.state.as_ref())
                .await
                .unwrap();
            assert_eq!(
                accepted,
                DurableApplicationProgress {
                    applied_lsn: 1,
                    committed_lsn: 0
                }
            );
            // Real peer delivery follows primary apply; service ownership holds
            // the actual quorum ACK, rather than inferring it from elapsed time.
            assert_eq!(scalar_progress(&pair.primary).await, 0);
            assert_eq!(scalar_progress(&pair.secondary).await, 0);
            let report = pair.primary.handle().public_v1_report().await.unwrap();
            assert_eq!((report.progress.first, report.progress.last), (0, 1));
            assert_ne!(report.read_status, AccessStatus::Granted);
            assert_ne!(report.write_status, AccessStatus::Granted);
            pair.primary
                .handle()
                .execute_public_v1(intent(
                    &pair.identity,
                    pair.primary.handle().process_session_id(),
                    5,
                    "sf-d02-held-progress",
                    PublicOperationProgram::Progress { capability: false },
                ))
                .await
                .unwrap();
            let record =
                stored_public_operation(&pair.primary, &OperationId::new("sf-d02-held-progress"));
            assert_eq!(record["lifecycle"]["outcomes"][0]["Progress"], 0);

            if outcome == "success" {
                operation.acknowledge(accepted).unwrap();
                pair.primary.state.commit_entered.notified().await;
                // A real peer ACK alone still cannot publish local commitment.
                assert_eq!(scalar_progress(&pair.primary).await, 0);
                assert_eq!(scalar_progress(&pair.secondary).await, 1);
                pair.primary.state.release_commit.notify_one();
                assert_eq!(write.await.unwrap().unwrap(), 1);
                assert_eq!(scalar_progress(&pair.primary).await, 1);
                let watermark = delivered(&mut operations).await;
                assert_eq!(
                    watermark.metadata,
                    OperationMetadata::Replication {
                        lsn: 1,
                        committed_lsn: 1
                    }
                );
                assert_eq!(scalar_progress(&pair.secondary).await, 1);
                let report = pair.secondary.handle().public_v1_report().await.unwrap();
                assert_eq!((report.progress.first, report.progress.last), (0, 1));
                assert_ne!(report.read_status, AccessStatus::Granted);
                let committed = watermark
                    .apply_to(pair.secondary.state.as_ref())
                    .await
                    .unwrap();
                watermark.acknowledge(committed).unwrap();
            } else {
                write.abort();
                assert!(write.await.unwrap_err().is_cancelled());
                assert_eq!(scalar_progress(&pair.primary).await, 0);
                if outcome == "epoch" {
                    pair.primary
                        .handle()
                        .execute_public_v1(intent(
                            &pair.identity,
                            pair.primary.handle().process_session_id(),
                            6,
                            "sf-d02-epoch",
                            PublicOperationProgram::Epoch {
                                epoch: Epoch::new(0, 2),
                            },
                        ))
                        .await
                        .unwrap();
                    assert_eq!(
                        pair.primary
                            .state
                            .epoch_tails
                            .lock()
                            .unwrap()
                            .last()
                            .copied(),
                        Some(0)
                    );
                    assert_eq!(scalar_progress(&pair.primary).await, 0);
                }
                // Late service ACK is not a resurrected primary write.
                let _ = operation.acknowledge(accepted);
                assert_eq!(scalar_progress(&pair.primary).await, 0);
                let report = pair.primary.handle().public_v1_report().await.unwrap();
                assert_eq!((report.progress.first, report.progress.last), (0, 1));
                assert_ne!(report.read_status, AccessStatus::Granted);
                assert_ne!(report.write_status, AccessStatus::Granted);
            }
            pair.cleanup().await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn sf_d02_secondary_scalar_is_contiguous_service_acknowledged_not_committed() {
    tokio::time::timeout(Duration::from_secs(30), async {
        let pair = AuthorizedHostedPair::start("sf-d02-secondary", 513).await;
        let mut operations = pair.secondary.application.control_operations();
        let mut response = pair.send_frame(pair.replication_frame(1, 0)).await;
        let operation = delivered(&mut operations).await;
        let accepted = operation
            .apply_to(pair.secondary.state.as_ref())
            .await
            .unwrap();
        assert_eq!(scalar_progress(&pair.secondary).await, 0);
        operation.acknowledge(accepted).unwrap();
        let ack = response.message().await.unwrap().unwrap();
        assert_eq!((ack.applied_lsn, ack.committed_lsn), (1, 0));
        assert_eq!(scalar_progress(&pair.secondary).await, 1);
        let report = pair.secondary.handle().public_v1_report().await.unwrap();
        assert_eq!((report.progress.first, report.progress.last), (0, 1));
        assert_ne!(report.read_status, AccessStatus::Granted);
        assert_ne!(report.write_status, AccessStatus::Granted);
        // An out-of-order frame cannot turn the scalar into a raw storage tail.
        let mut gap = pair.send_frame(pair.replication_frame(3, 0)).await;
        assert!(gap.message().await.is_err());
        assert_eq!(scalar_progress(&pair.secondary).await, 1);
        pair.cleanup().await;
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn sf11_committed_prefix_6_through_10_precedes_ambiguous_11_and_rejects_bad_history() {
    tokio::time::timeout(Duration::from_secs(90), async {
        for case in ["exact", "promotion", "divergent", "missing", "unavailable"] {
            let name = format!("sf11-prefix-{case}");
            let resource = ResourceUid::new(&name);
            let preview = PublicV1Identity::new(511);
            let mut primary = HostedMemoryReplica::start(
                &format!("{name}-primary"), &resource, &preview, 1,
            ).await;
            let mut secondary = HostedMemoryReplica::start(
                &format!("{name}-secondary"), &resource, &preview, 2,
            ).await;
            let generation = PublicProtocolGeneration::new(511);
            let initialization = derive_history_initialization_id(&resource, generation);
            let history = derive_bootstrap_history(&resource, generation, &initialization);
            primary.initialize_history(&initialization, &history).await;
            secondary.initialize_history(&initialization, &history).await;
            // Seed real retained operations before Open snapshots provider progress.
            for lsn in 1..=11 {
                primary.state.apply(Operation {
                    lsn, committed_lsn: lsn - 1,
                    data: Bytes::from(format!("sf11-value-{lsn}")),
                }).await.unwrap();
            }
            primary.state.commit(10).await.unwrap();
            let peer_tail = if case == "divergent" { 6 } else { 5 };
            for lsn in 1..=peer_tail {
                secondary.state.apply(Operation {
                    lsn, committed_lsn: lsn - 1,
                    data: Bytes::from(if lsn == 6 {
                        "genuinely-conflicting-six".to_owned()
                    } else {
                        format!("sf11-value-{lsn}")
                    }),
                }).await.unwrap();
            }
            secondary.state.commit(5).await.unwrap();
            assert_eq!(primary.state.durable_progress().await.unwrap(),
                DurableApplicationProgress { applied_lsn: 11, committed_lsn: 10 });
            assert_eq!(secondary.state.durable_progress().await.unwrap(),
                DurableApplicationProgress { applied_lsn: peer_tail, committed_lsn: 5 });
            let epoch = Epoch::new(0, if case == "promotion" { 2 } else { 1 });
            let descriptor = ConfigurationDescriptor::new(
                epoch, primary.identity.replica_id,
                vec![
                    ConfigurationMember { identity: primary.identity.clone(), role: ReplicaRole::Primary },
                    ConfigurationMember { identity: secondary.identity.clone(), role: ReplicaRole::ActiveSecondary },
                ], 2,
            );
            for (local, peer, role, advertised) in [
                (&secondary, &primary, ReplicaRole::ActiveSecondary, 11),
                (&primary, &secondary, ReplicaRole::Primary, 5),
            ] {
                let configuration = PublicConfiguration {
                    current: descriptor.clone(), previous: None,
                    replicas: vec![PublicReplicaDescriptor {
                        build_id: OperationId::default(),
                        identity: peer.identity.clone(),
                        replication_address: format!("http://{}", peer.address),
                        process_session_id: peer.handle().process_session_id(),
                        storage_incarnation: Some(peer.storage.clone()),
                        role: if role == ReplicaRole::Primary { ReplicaRole::ActiveSecondary } else { ReplicaRole::Primary },
                        current_progress: advertised,
                        catch_up_capability: advertised,
                        must_catchup: role == ReplicaRole::Primary,
                    }],
                };
                if role == ReplicaRole::Primary {
                    let handle = local.handle();
                    let session = handle.process_session_id();
                    handle.execute_public_v1(intent(
                        &preview, session.clone(), 1, "sf-d02-open",
                        PublicOperationProgram::Open { replica: local.identity.clone(), existing: true },
                    )).await.unwrap();
                    assert_eq!(scalar_progress(local).await, 11);
                    let report = handle.public_v1_report().await.unwrap();
                    assert_ne!(report.read_status, AccessStatus::Granted);
                    assert_ne!(report.write_status, AccessStatus::Granted);
                    if case == "promotion" {
                        let control = local.application.control.read().await.clone().unwrap();
                        control.change_role(Epoch::new(0, 1), ReplicaRole::ActiveSecondary).await.unwrap();
                        control.update_epoch(Epoch::new(0, 1)).await.unwrap();
                        assert_eq!(scalar_progress(local).await, 11);
                    }
                    for (revision, program) in [
                        (2, PublicOperationProgram::Role { epoch, role }),
                        (3, PublicOperationProgram::Epoch { epoch }),
                        (4, PublicOperationProgram::Configuration(configuration)),
                    ] {
                        handle.execute_public_v1(intent(
                            &preview, session.clone(), revision,
                            &format!("sf-d02-setup-{revision}"), program,
                        )).await.unwrap();
                        assert_eq!(scalar_progress(local).await, 10);
                    }
                    let report = handle.public_v1_report().await.unwrap();
                    assert_eq!((report.progress.first, report.progress.last), (10, 11));
                    assert_ne!(report.read_status, AccessStatus::Granted);
                } else {
                    install_static_authority(
                    &local.handle(), &preview, local.identity.clone(), role, epoch,
                    configuration,
                    ).await;
                    assert_eq!(scalar_progress(local).await, peer_tail);
                }
            }
            if case == "missing" {
                primary.state.operations.lock().unwrap().retain(|operation| operation.lsn != 6);
            }
            let delivered = Arc::new(Mutex::new(Vec::new()));
            let mut operations = secondary.application.control_operations();
            let record = delivered.clone();
            let state = secondary.state.clone();
            let mut consumers = tokio::task::JoinSet::new();
            consumers.spawn(async move {
                while let Some(operation) = operations.recv().await {
                    if let OperationMetadata::Replication { lsn, committed_lsn } = &operation.metadata {
                        record.lock().unwrap().push((*lsn, *committed_lsn));
                    }
                    match operation.apply_to(state.as_ref()).await {
                        Ok(progress) => { operation.acknowledge(progress).unwrap(); }
                        Err(error) => { let _ = operation.reject(error); }
                    }
                }
            });
            if case == "unavailable" { secondary.stop().await; }
            let settlement = primary.application.wait_for_write_quorum().await;
            match case {
                "exact" | "promotion" => {
                    settlement.unwrap();
                    tokio::time::timeout(Duration::from_secs(10), async {
                        while secondary.state.durable_progress().await.unwrap().committed_lsn != 11 {
                            tokio::task::yield_now().await;
                        }
                        assert_eq!(scalar_progress(&primary).await,
                            if matches!(case, "exact" | "promotion") { 11 } else { 10 });
                    }).await.unwrap();
                    assert_eq!(*delivered.lock().unwrap(),
                        vec![(6, 6), (7, 7), (8, 8), (9, 9), (10, 10), (11, 10), (11, 11)]);
                    assert_eq!(primary.state.progress.lock().unwrap().committed_lsn, 11);
                    assert_eq!(secondary.state.apply_calls.load(Ordering::Acquire), 11,
                        "final watermark must not logically reapply 11");
                }
                "divergent" => {
                    assert!(matches!(settlement,
                        Err(RuntimeError::ReplicaRebuildRequired(rebuild))
                        if matches!(rebuild.as_ref(), kuberic_runtime::protocol::public_operations::ReplicaRebuildRequired::RemoteBuildTarget(target)
                            if target.target == secondary.identity
                            && target.reason == kuberic_runtime::protocol::public_operations::ReplicaRebuildReason::IncompatibleRetainedHistory)));
                    assert_eq!(primary.state.progress.lock().unwrap().committed_lsn, 10);
                    assert_eq!(secondary.state.progress.lock().unwrap().committed_lsn, 5);
                }
                "missing" => {
                    assert!(matches!(settlement,
                        Err(RuntimeError::ReplicaRebuildRequired(rebuild))
                        if matches!(rebuild.as_ref(), kuberic_runtime::protocol::public_operations::ReplicaRebuildRequired::LocalProvider(local)
                            if local.provider.replica == primary.identity
                            && local.reason == kuberic_runtime::protocol::public_operations::ReplicaRebuildReason::IncompatibleRetainedHistory)));
                    assert_eq!(primary.state.progress.lock().unwrap().committed_lsn, 10);
                    assert!(delivered.lock().unwrap().is_empty());
                }
                "unavailable" => {
                    assert!(matches!(settlement, Err(RuntimeError::ReconfigurationPending)));
                    assert_eq!(primary.state.progress.lock().unwrap().committed_lsn, 10);
                }
                _ => unreachable!(),
            }
            primary.stop().await;
            if case != "unavailable" { secondary.stop().await; }
            consumers.abort_all();
            while consumers.join_next().await.is_some() {}
            primary.cleanup();
            secondary.cleanup();
        }
    }).await.unwrap();
}
