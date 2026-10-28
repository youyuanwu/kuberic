use super::*;

fn publish(recovery: &Recovery, source: ReplicaPublication, target: ReplicaPublication) {
    *recovery.pair.source_access.publication.lock().unwrap() = source.clone();
    *recovery.fresh_access.publication.lock().unwrap() = target.clone();
    recovery.pair.source.test_publish(vec![target]);
    recovery.fresh.test_publish(vec![source]);
}

fn current(recovery: &Recovery, publication: &ReplicaPublication) -> ReplicaSetConfiguration {
    ReplicaSetConfiguration {
        configuration: publication.scope.configuration.current.clone(),
        replicas: vec![ReplicaInformation {
            identity: recovery.fresh.local().identity.clone(),
            replication_address: recovery.fresh.local().address.clone(),
            process_session_id: recovery.fresh.local().process_session_id.clone(),
            storage_incarnation: Some(recovery.fresh.local().storage.clone()),
            role: ReplicaRole::ActiveSecondary,
            current_progress: 0,
            catch_up_capability: 0,
            must_catchup: false,
        }],
    }
}
async fn established() -> Recovery {
    let recovery = Recovery::new().await;
    recovery
        .fresh
        .control(&recovery.binding, Control::Recover)
        .await
        .unwrap();
    recovery.complete().await;
    recovery
}

fn publications(recovery: &Recovery) -> (ReplicaPublication, ReplicaPublication) {
    let mut source = recovery
        .pair
        .source_access
        .publication
        .lock()
        .unwrap()
        .clone();
    let mut target = recovery.fresh_access.publication.lock().unwrap().clone();
    source.scope.configuration.replicas = vec![peer(&target, ReplicaRole::ActiveSecondary)];
    target.scope.configuration.replicas = vec![peer(&source, ReplicaRole::Primary)];
    (source, target)
}

#[tokio::test]
async fn native_private_effect_source_stop_during_confirmation_preserves_actual_receiver_query() {
    let recovery = established().await;
    let secondary = recovery.fresh.test_production().drainage_secondary();
    let identity = secondary.transport_identity().unwrap();
    let credit = recovery.fresh.native_credit_count();
    let gate = Gate::new();
    let _route = recovery.route(vec![(Cut::ConfirmReceived, gate.clone())]);
    let owner = recovery.fresh.clone();
    let binding = recovery.binding.clone();
    let confirmation = tokio::spawn(async move { owner.confirm_effect(&binding).await });
    gate.entered().await;
    recovery.source.replicator().abort();
    gate.release.add_permits(1);
    let result = tokio::time::timeout(Duration::from_secs(5), confirmation)
        .await
        .unwrap()
        .unwrap();
    let query = recovery.target.replicator().current_progress().await;
    assert!(
        matches!(&result, Err(RuntimeError::PeerUnavailable(_))),
        "actual stopped source is unavailable, not a live permission rejection: {result:?}; actual receiver query: {query:?}"
    );
    assert!(recovery.fresh.sessions.active(&recovery.binding).is_err());
    assert!(recovery.fresh.local_receiver_drain(&recovery.binding));
    assert!(secondary.same_instance(&recovery.fresh.test_production().drainage_secondary()));
    assert_eq!(secondary.transport_identity().unwrap(), identity);
    let progress = query.unwrap();
    recovery
        .target
        .replicator()
        .update_epoch(epoch(2))
        .await
        .unwrap();
    assert_eq!(
        recovery
            .target
            .replicator()
            .current_progress()
            .await
            .unwrap(),
        progress
    );
    recovery
        .target
        .replicator()
        .catch_up_capability()
        .await
        .unwrap();
    assert_eq!(recovery.fresh.native_credit_count(), credit);
    recovery.finish().await;
}

#[tokio::test]
async fn native_private_effect_live_source_crossed_nonce_remains_permission_rejected() {
    let recovery = established().await;
    let mut crossed = recovery.binding.clone();
    crossed.nonce = uuid::Uuid::new_v4().to_string();
    assert!(matches!(
        recovery.fresh.control(&crossed, Control::Confirm).await,
        Err(RuntimeError::AuthorityNotAdmitted)
    ));
    recovery
        .fresh
        .confirm_effect(&recovery.binding)
        .await
        .unwrap();
    recovery.write().await;
    recovery.finish().await;
}

#[tokio::test]
async fn native_private_effect_warm_continue_cancel_after_native_advance_does_not_repeat_or_accept_old_ack()
 {
    // Publications are explicit host-context fixtures; copy, native roots,
    // transport generations, continuation and ACK owners below are actual.
    let recovery = established().await;
    let secondary = recovery.fresh.test_production().drainage_secondary();
    let old_ack = secondary.transport_ack();
    let old_identity = secondary.transport_identity().unwrap();
    let gate = Gate::new();
    let advances = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let _route = TestingTcpRoute::register_with_kind(
        recovery.fresh.local().identity.instance_id.to_string(),
        {
            let gate = gate.clone();
            let advances = advances.clone();
            move |kind, _, endpoint| {
                let gate = gate.clone();
                let advances = advances.clone();
                Box::pin(async move {
                    if kind == Cut::ReceiverReplaced {
                        advances.fetch_add(1, Ordering::SeqCst);
                        gate.entered.add_permits(1);
                        gate.release.acquire().await.unwrap().forget();
                    }
                    Ok(endpoint)
                })
            }
        },
    )
    .unwrap();
    let (mut source, mut target) = publications(&recovery);
    for publication in [&mut source, &mut target] {
        publication.scope.epoch = epoch(2);
        publication.scope.configuration.current.epoch = epoch(2);
        publication.scope.configuration.current.configuration_id =
            publication.scope.configuration.current.expected_id();
    }
    publish(&recovery, source.clone(), target);
    recovery
        .source
        .replicator()
        .update_epoch(epoch(2))
        .await
        .unwrap();
    recovery
        .target
        .replicator()
        .update_epoch(epoch(2))
        .await
        .unwrap();
    let config = current(&recovery, &source);
    let retried_config = config.clone();
    let primary = recovery.source.primary_replicator().unwrap();
    let operation = tokio::spawn(async move {
        primary
            .update_current_replica_set_configuration(config)
            .await
    });
    gate.entered().await;
    assert!(secondary.same_instance(&recovery.fresh.test_production().drainage_secondary()));
    assert_ne!(secondary.transport_identity().unwrap(), old_identity);
    let binding = recovery.pair.source.sessions.binding_for(2).unwrap();
    assert!(recovery.fresh.sessions.active(&binding).is_err());
    let owner = recovery.pair.source.clone();
    let selected = binding.clone();
    let observer = tokio::spawn(async move { owner.control(&selected, Control::Continue).await });
    tokio::task::yield_now().await;
    assert!(!observer.is_finished());
    observer.abort();
    assert!(observer.await.unwrap_err().is_cancelled());
    operation.abort();
    assert!(operation.await.unwrap_err().is_cancelled());
    let primary = recovery.source.primary_replicator().unwrap();
    let mut retry = tokio::spawn(async move {
        primary
            .update_current_replica_set_configuration(retried_config)
            .await
    });
    assert!(
        tokio::time::timeout(Duration::from_millis(50), &mut retry)
            .await
            .is_err(),
        "protected configuration retry must await the retained actual owner"
    );
    gate.release.add_permits(1);
    tokio::time::timeout(Duration::from_secs(5), retry)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    recovery
        .pair
        .source
        .control(&binding, Control::Continue)
        .await
        .unwrap();
    assert_eq!(advances.load(Ordering::SeqCst), 1);
    let pending = recovery.fresh.pending_sends(&binding);
    assert!(
        recovery.fresh.ack(
            &crate::replicator::v1::service_adapter::production_transport::route(
                &binding,
                recovery.fresh.local()
            )
            .unwrap(),
            old_ack
        )
    );
    assert_eq!(
        recovery.fresh.pending_sends(&binding),
        pending,
        "old native ACK generation is dropped before carrier ingress"
    );
    assert!(
        recovery.fresh.ack(
            &crate::replicator::v1::service_adapter::production_transport::route(
                &binding,
                recovery.fresh.local()
            )
            .unwrap(),
            secondary.transport_ack(),
        )
    );
    assert!(recovery.fresh.pending_sends(&binding) > pending);
    recovery.write().await;
    recovery.source.replicator().abort();
    recovery.target.replicator().abort();
    recovery.source.replicator().close().await.unwrap();
    recovery.target.replicator().close().await.unwrap();
    recovery.pair.source.drain().await.unwrap();
    recovery.fresh.drain().await.unwrap();
}

#[tokio::test]
async fn native_private_effect_overlapping_configuration_preflights_share_one_continuation_owner() {
    let recovery = established().await;
    let secondary = recovery.fresh.test_production().drainage_secondary();
    let old_identity = secondary.transport_identity().unwrap();
    let gate = Gate::new();
    let checks = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let advances = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let _source_route = TestingTcpRoute::register_with_kind(
        recovery
            .pair
            .source
            .local()
            .identity
            .instance_id
            .to_string(),
        {
            let gate = gate.clone();
            let checks = checks.clone();
            move |kind, _, endpoint| {
                let gate = gate.clone();
                let checks = checks.clone();
                Box::pin(async move {
                    if kind == Cut::ContinuationChecked {
                        checks.fetch_add(1, Ordering::SeqCst);
                        gate.entered.add_permits(1);
                        gate.release.acquire().await.unwrap().forget();
                    }
                    Ok(endpoint)
                })
            }
        },
    )
    .unwrap();
    let _target_route = TestingTcpRoute::register_with_kind(
        recovery.fresh.local().identity.instance_id.to_string(),
        {
            let advances = advances.clone();
            move |kind, _, endpoint| {
                let advances = advances.clone();
                Box::pin(async move {
                    if kind == Cut::ReceiverReplaced {
                        advances.fetch_add(1, Ordering::SeqCst);
                    }
                    Ok(endpoint)
                })
            }
        },
    )
    .unwrap();
    let (mut source, mut target) = publications(&recovery);
    for publication in [&mut source, &mut target] {
        publication.scope.epoch = epoch(2);
        publication.scope.configuration.current.epoch = epoch(2);
        publication.scope.configuration.current.configuration_id =
            publication.scope.configuration.current.expected_id();
    }
    publish(&recovery, source.clone(), target);
    recovery
        .source
        .replicator()
        .update_epoch(epoch(2))
        .await
        .unwrap();
    recovery
        .target
        .replicator()
        .update_epoch(epoch(2))
        .await
        .unwrap();
    let config = current(&recovery, &source);
    let primary = recovery.source.primary_replicator().unwrap();
    let first_config = config.clone();
    let first = tokio::spawn(async move {
        primary
            .update_current_replica_set_configuration(first_config)
            .await
    });
    gate.entered().await;
    let primary = recovery.source.primary_replicator().unwrap();
    let second_config = config.clone();
    let mut second = tokio::spawn(async move {
        primary
            .update_current_replica_set_configuration(second_config)
            .await
    });
    assert!(
        tokio::time::timeout(Duration::from_millis(50), &mut second)
            .await
            .is_err(),
        "overlap must observe the retained whole Configuration owner"
    );
    assert_eq!(checks.load(Ordering::SeqCst), 1);
    assert_eq!(advances.load(Ordering::SeqCst), 0);
    assert!(!first.is_finished());
    assert!(!second.is_finished());
    gate.release.add_permits(1);
    for operation in [first, second] {
        tokio::time::timeout(Duration::from_secs(5), operation)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }
    assert_eq!(advances.load(Ordering::SeqCst), 1);
    assert_eq!(checks.load(Ordering::SeqCst), 1);
    assert_ne!(secondary.transport_identity().unwrap(), old_identity);
    let selected = recovery.pair.source.sessions.binding_for(2).unwrap();
    recovery
        .pair
        .source
        .confirm_effect(&selected)
        .await
        .unwrap();
    recovery
        .source
        .primary_replicator()
        .unwrap()
        .update_current_replica_set_configuration(config)
        .await
        .unwrap();
    recovery.write().await;
    assert_eq!(advances.load(Ordering::SeqCst), 1);
    recovery.source.replicator().abort();
    recovery.target.replicator().abort();
    recovery.source.replicator().close().await.unwrap();
    recovery.target.replicator().close().await.unwrap();
    recovery.pair.source.drain().await.unwrap();
    recovery.fresh.drain().await.unwrap();
}

#[tokio::test]
async fn native_private_effect_same_primary_joint_current_keeps_root_nonce_and_ack_generation_then_fences_removal()
 {
    let recovery = established().await;
    let binding = recovery.pair.source.sessions.binding_for(2).unwrap();
    let incarnation = recovery.pair.source.native_incarnation(&binding).unwrap();
    let secondary = recovery.fresh.test_production().drainage_secondary();
    let identity = secondary.transport_identity().unwrap();
    let (mut source, mut target) = publications(&recovery);
    source.scope.configuration.previous = Some(source.scope.configuration.current.clone());
    source.stage = ReplicaPublicationStage::DiscoveryPending;
    source.role = ReplicaRole::None;
    publish(&recovery, source.clone(), target.clone());
    let primary = recovery.source.primary_replicator().unwrap();
    let config = current(&recovery, &source);
    primary
        .update_catch_up_replica_set_configuration(config.clone(), config.clone())
        .await
        .unwrap();
    source.scope.configuration.previous = None;
    target.scope.configuration.previous = None;
    publish(&recovery, source.clone(), target.clone());
    primary
        .update_current_replica_set_configuration(config)
        .await
        .unwrap();
    source.stage = ReplicaPublicationStage::AchievedScope;
    source.role = ReplicaRole::Primary;
    target.stage = ReplicaPublicationStage::AchievedScope;
    target.role = ReplicaRole::ActiveSecondary;
    publish(&recovery, source.clone(), target);
    let retained = recovery.pair.source.sessions.binding_for(2).unwrap();
    assert_eq!(retained.nonce, binding.nonce);
    assert_eq!(
        recovery.pair.source.native_incarnation(&retained).unwrap(),
        incarnation
    );
    assert!(secondary.same_instance(&recovery.fresh.test_production().drainage_secondary()));
    assert_eq!(secondary.transport_identity().unwrap(), identity);
    recovery
        .pair
        .source
        .confirm_effect(&retained)
        .await
        .unwrap();
    source
        .scope
        .configuration
        .current
        .members
        .retain(|member| member.identity == recovery.pair.source.local().identity);
    source.scope.configuration.current.write_quorum = 1;
    source.scope.configuration.current.configuration_id =
        source.scope.configuration.current.expected_id();
    source.scope.configuration.replicas.clear();
    *recovery.pair.source_access.publication.lock().unwrap() = source.clone();
    primary
        .update_current_replica_set_configuration(ReplicaSetConfiguration {
            configuration: source.scope.configuration.current,
            replicas: Vec::new(),
        })
        .await
        .unwrap();
    assert!(recovery.pair.source.sessions.active(&retained).is_err());
    assert!(recovery.pair.source.fence(&retained).check().is_err());
    recovery.source.replicator().abort();
    recovery.target.replicator().abort();
    recovery.source.replicator().close().await.unwrap();
    recovery.target.replicator().close().await.unwrap();
    recovery.pair.source.drain().await.unwrap();
    recovery.fresh.drain().await.unwrap();
}

#[tokio::test]
async fn native_private_effect_failure_after_real_receiver_advance_is_memoized_without_an_active_shortcut()
 {
    let recovery = established().await;
    let secondary = recovery.fresh.test_production().drainage_secondary();
    let old = secondary.transport_identity().unwrap();
    let advances = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let _route = TestingTcpRoute::register_with_kind(
        recovery.fresh.local().identity.instance_id.to_string(),
        {
            let advances = advances.clone();
            move |kind, _, endpoint| {
                let advances = advances.clone();
                Box::pin(async move {
                    if kind == Cut::ReceiverReplaced {
                        advances.fetch_add(1, Ordering::SeqCst);
                        return Err(std::io::Error::other(
                            "synthetic negative: after actual native receiver advance",
                        ));
                    }
                    Ok(endpoint)
                })
            }
        },
    )
    .unwrap();
    let (mut source, mut target) = publications(&recovery);
    for publication in [&mut source, &mut target] {
        publication.scope.epoch = epoch(2);
        publication.scope.configuration.current.epoch = epoch(2);
        publication.scope.configuration.current.configuration_id =
            publication.scope.configuration.current.expected_id();
    }
    publish(&recovery, source.clone(), target);
    recovery
        .source
        .replicator()
        .update_epoch(epoch(2))
        .await
        .unwrap();
    recovery
        .target
        .replicator()
        .update_epoch(epoch(2))
        .await
        .unwrap();
    let result = recovery
        .source
        .primary_replicator()
        .unwrap()
        .update_current_replica_set_configuration(current(&recovery, &source))
        .await;
    assert!(result.is_err());
    let selected = recovery.pair.source.sessions.binding_for(2).unwrap();
    let after = secondary.transport_identity().unwrap();
    assert_ne!(after, old);
    for _ in 0..2 {
        assert!(
            recovery
                .pair
                .source
                .control(&selected, Control::Continue)
                .await
                .is_err()
        );
    }
    assert_eq!(secondary.transport_identity().unwrap(), after);
    assert_eq!(advances.load(Ordering::SeqCst), 1);
    assert!(recovery.fresh.sessions.active(&selected).is_err());
    assert!(
        recovery
            .fresh
            .check_health()
            .unwrap_err()
            .to_string()
            .contains("after actual native receiver advance")
    );
    recovery.source.replicator().abort();
    recovery.target.replicator().abort();
    let source_failure = recovery
        .source
        .replicator()
        .close()
        .await
        .unwrap_err()
        .to_string();
    assert!(
        source_failure.contains("continuation receiver"),
        "{source_failure}"
    );
    let first = recovery
        .target
        .replicator()
        .close()
        .await
        .unwrap_err()
        .to_string();
    assert!(first.contains("after actual native receiver advance"));
    assert_eq!(
        recovery
            .target
            .replicator()
            .close()
            .await
            .unwrap_err()
            .to_string(),
        first
    );
    assert!(
        recovery
            .pair
            .source
            .drain()
            .await
            .unwrap_err()
            .to_string()
            .contains("continuation receiver")
    );
    assert!(
        recovery
            .fresh
            .drain()
            .await
            .unwrap_err()
            .to_string()
            .contains("after actual native receiver advance")
    );
}
