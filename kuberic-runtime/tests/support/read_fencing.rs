use super::*;

#[tokio::test]
async fn sf11_committed_prefix_6_through_10_precedes_ambiguous_11_and_rejects_bad_history() {
    tokio::time::timeout(Duration::from_secs(90), async {
        for case in ["exact", "divergent", "missing", "unavailable"] {
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
            let epoch = Epoch::new(0, 1);
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
                install_static_authority(
                    &local.handle(), &preview, local.identity.clone(), role, epoch,
                    PublicConfiguration {
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
                    },
                ).await;
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
                "exact" => {
                    settlement.unwrap();
                    tokio::time::timeout(Duration::from_secs(10), async {
                        while secondary.state.durable_progress().await.unwrap().committed_lsn != 11 {
                            tokio::task::yield_now().await;
                        }
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
