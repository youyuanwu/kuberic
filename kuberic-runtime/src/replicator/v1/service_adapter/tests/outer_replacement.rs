use super::*;
use crate::replicator::v1::service_adapter::engine_session::replacement_tests::{
    description, replacement_from,
};

fn information_for(publication: &ReplicaPublication, role: ReplicaRole) -> ReplicaInformation {
    ReplicaInformation {
        identity: publication.scope.storage.replica.clone(),
        process_session_id: publication.scope.process_session_id.clone(),
        storage_incarnation: Some(publication.scope.storage.clone()),
        replication_address: publication.replication_address.clone(),
        role,
        current_progress: crate::application::INVALID_LSN,
        catch_up_capability: crate::application::INVALID_LSN,
        must_catchup: false,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn m3_outer_replacement_real_copy_keeps_old_quorum_pending_until_joint_catchup_and_current_only()
 {
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
    target
        .replicator()
        .change_role(epoch(1), ReplicaRole::ActiveSecondary)
        .await
        .unwrap();
    let original = configuration(&pair, true);
    source
        .primary_replicator()
        .unwrap()
        .update_current_replica_set_configuration(original.clone())
        .await
        .unwrap();
    publish_configuration(&pair, &original, None, 0);
    let old = pair.source.sessions.binding_for(2).unwrap();
    let native = pair.source.test_production().removal_primary();
    let old_root = native.removal_session(2);
    let old_incarnation = old_root.identity().remote_incarnation;
    let mut held = source
        .state_replicator()
        .unwrap()
        .begin_replicate(Bytes::from_static(b"held-before-replacement"))
        .unwrap();
    let mut old_operations = target
        .state_replicator()
        .unwrap()
        .replication_operations()
        .unwrap();
    let old_operation =
        tokio::time::timeout(Duration::from_secs(5), old_operations.get_operation())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    assert_eq!(old_operation.lsn, held.lsn);
    assert!(held.completion.as_mut().now_or_never().is_none());

    source.replicator().update_epoch(epoch(2)).await.unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut selected = replacement_from(pair.binding.source.clone(), pair.binding.target.clone());
    selected.target.replication_address = format!("http://{}", listener.local_addr().unwrap());
    selected.validate().unwrap();
    *pair.source_access.publication.lock().unwrap() = selected.source.clone();
    pair.source.test_publish(vec![selected.target.clone()]);
    let mut prepared = original.clone();
    prepared.configuration = selected.source.scope.configuration.current.clone();
    prepared.replicas[0].current_progress = crate::application::INVALID_LSN;
    prepared.replicas[0].catch_up_capability = crate::application::INVALID_LSN;
    source
        .primary_replicator()
        .unwrap()
        .update_current_replica_set_configuration(prepared)
        .await
        .unwrap();
    assert_eq!(native.inspect_configuration()["current"]["writeQuorum"], 2);
    assert!(held.completion.as_mut().now_or_never().is_none());
    assert_eq!(old_root.identity().remote_incarnation, old_incarnation);

    let access = Pair::access(selected.target.clone());
    let context = Pair::context(&selected.target, access.clone());
    let registry = MessageRegistry::default();
    let fresh = Transport::test_owner(
        context.clone(),
        selected.secondary(),
        pair.source.history().clone(),
        vec![selected.source.clone()],
        registry.clone(),
    );
    fresh.test_listen(listener);
    let engine = test_compose(context, Arc::new(Provider::new()), registry, fresh.clone())
        .await
        .unwrap();
    engine.replicator().open().await.unwrap();
    engine
        .replicator()
        .change_role(epoch(2), ReplicaRole::IdleSecondary)
        .await
        .unwrap();
    let idle = information_for(&selected.target, ReplicaRole::IdleSecondary);
    let primary = source.primary_replicator().unwrap();
    let mut build = tokio::spawn(async move { primary.build_replica(idle).await });
    let mut copy = engine
        .state_replicator()
        .unwrap()
        .copy_operations()
        .unwrap();
    loop {
        let operation = tokio::select! {
            result = &mut build => panic!("fresh Build ended before copy: {result:?}"),
            result = tokio::time::timeout(Duration::from_secs(5), copy.get_operation()) =>
                result.unwrap().unwrap().unwrap(),
        };
        if !operation.end_of_stream {
            assert_eq!(operation.data, Bytes::from_static(b"snapshot"));
        }
        operation.acknowledge().unwrap();
        if operation.end_of_stream {
            break;
        }
    }
    let mut replay = engine
        .state_replicator()
        .unwrap()
        .replication_operations()
        .unwrap();
    let operation = tokio::time::timeout(Duration::from_secs(5), replay.get_operation())
        .await
        .unwrap_or_else(|error| {
            panic!(
                "fresh replication delivery: {error}; source={}, target={}; native={}",
                pair.source.removal_receivers(),
                fresh.removal_receivers(),
                pair.source.test_production().inspect(),
            )
        })
        .unwrap()
        .unwrap();
    assert_eq!(operation.lsn, held.lsn);
    assert_eq!(
        operation.data,
        Bytes::from_static(b"held-before-replacement")
    );
    operation.acknowledge().unwrap();
    tokio::time::timeout(Duration::from_secs(5), build)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let successor = pair.source.sessions.binding_for(2).unwrap();
    assert!(successor.replacement().is_some());
    assert_ne!(successor.nonce, old.nonce);
    assert_ne!(
        pair.source.native_incarnation(&successor).unwrap(),
        old_incarnation
    );
    assert!(pair.source.sessions.active(&old).is_err());
    assert!(
        pair.source.sessions.settled(&old).is_err(),
        "source retirement is not target join"
    );
    assert!(
        pair.target.sessions.active(&old).is_ok(),
        "the remote receiver was not joined"
    );
    assert!(held.completion.as_mut().now_or_never().is_none());
    assert_eq!(native.inspect_configuration()["current"]["writeQuorum"], 2);
    assert!(native.inspect_configuration()["previous"].is_null());

    // Build has reached its real replay target. A later operation now makes the
    // cached joint CC boundary genuinely pending independently of Build.
    let mut after_ready = source
        .state_replicator()
        .unwrap()
        .begin_replicate(Bytes::from_static(b"after-fresh-ready"))
        .unwrap();
    let mut active = information_for(&selected.target, ReplicaRole::ActiveSecondary);
    active.current_progress = held.lsn;
    active.catch_up_capability = held.lsn;
    let current = ReplicaSetConfiguration {
        configuration: ConfigurationDescriptor::new(
            epoch(2),
            pair.source.local().identity.replica_id,
            vec![
                ConfigurationMember {
                    identity: pair.source.local().identity.clone(),
                    role: ReplicaRole::Primary,
                },
                ConfigurationMember {
                    identity: selected.secondary().identity,
                    role: ReplicaRole::ActiveSecondary,
                },
            ],
            2,
        ),
        replicas: vec![active],
    };
    let mut previous = original.clone();
    previous.replicas[0].current_progress = crate::application::INVALID_LSN;
    previous.replicas[0].catch_up_capability = crate::application::INVALID_LSN;
    let mut joint_source = selected.source.clone();
    joint_source.scope.configuration.current = current.configuration.clone();
    joint_source.scope.configuration.previous = Some(previous.configuration.clone());
    let mut fresh_description = description(&selected.target, ReplicaRole::ActiveSecondary);
    fresh_description.current_progress = held.lsn;
    fresh_description.catch_up_capability = held.lsn;
    joint_source
        .scope
        .configuration
        .replicas
        .push(fresh_description);
    joint_source.stage = ReplicaPublicationStage::DiscoveryPending;
    joint_source.role = ReplicaRole::None;
    *pair.source_access.publication.lock().unwrap() = joint_source.clone();
    fresh.test_publish(vec![joint_source.clone()]);
    source
        .primary_replicator()
        .unwrap()
        .update_catch_up_replica_set_configuration(current.clone(), previous)
        .await
        .unwrap();
    assert_eq!(native.inspect_configuration()["current"]["writeQuorum"], 2);
    assert_eq!(native.inspect_configuration()["previous"]["writeQuorum"], 2);
    assert_ne!(
        native.inspect_configuration()["current"]["members"][0]["incarnation"],
        native.inspect_configuration()["previous"]["members"][0]["incarnation"]
    );
    let primary = source.primary_replicator().unwrap();
    let mut wait = Box::pin(primary.wait_for_catch_up_quorum(ReplicaSetQuorumMode::WriteQuorum));
    assert!(wait.as_mut().now_or_never().is_none());
    source.replicator().current_progress().await.unwrap();
    assert!(wait.as_mut().now_or_never().is_none());
    let operation = tokio::time::timeout(Duration::from_secs(5), replay.get_operation())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(operation.lsn, after_ready.lsn);
    operation.acknowledge().unwrap();
    tokio::time::timeout(Duration::from_secs(5), wait)
        .await
        .unwrap()
        .unwrap();
    assert!(held.completion.as_mut().now_or_never().is_none());
    assert!(after_ready.completion.as_mut().now_or_never().is_none());

    joint_source.stage = ReplicaPublicationStage::AchievedScope;
    joint_source.role = ReplicaRole::Primary;
    *pair.source_access.publication.lock().unwrap() = joint_source.clone();
    let mut joint_target = selected.target.clone();
    joint_target.scope.configuration.current = current.configuration.clone();
    joint_target.scope.configuration.previous = joint_source.scope.configuration.previous.clone();
    joint_target.role = ReplicaRole::ActiveSecondary;
    *access.publication.lock().unwrap() = joint_target.clone();
    pair.source.test_publish(vec![joint_target.clone()]);
    fresh.test_publish(vec![joint_source.clone()]);
    engine
        .replicator()
        .change_role(epoch(2), ReplicaRole::ActiveSecondary)
        .await
        .unwrap();
    joint_target.scope.configuration.previous = None;
    joint_target
        .scope
        .configuration
        .replicas
        .retain(|peer| peer.identity != selected.replacement().unwrap().identity);
    *access.publication.lock().unwrap() = joint_target.clone();
    pair.source.test_publish(vec![joint_target]);
    joint_source.scope.configuration.previous = None;
    joint_source
        .scope
        .configuration
        .replicas
        .retain(|peer| peer.identity != selected.replacement().unwrap().identity);
    joint_source.stage = ReplicaPublicationStage::DiscoveryPending;
    joint_source.role = ReplicaRole::None;
    *pair.source_access.publication.lock().unwrap() = joint_source.clone();
    fresh.test_publish(vec![joint_source]);
    source
        .primary_replicator()
        .unwrap()
        .update_current_replica_set_configuration(current)
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), held.completion)
        .await
        .unwrap()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), after_ready.completion)
        .await
        .unwrap()
        .unwrap();
    assert!(native.inspect_configuration()["previous"].is_null());
    assert!(
        source
            .primary_replicator()
            .unwrap()
            .remove_replica(pair.target.local().identity.replica_id)
            .await
            .is_err()
    );
    assert_eq!(pair.source.sessions.binding_for(2), Some(successor));
    drop(old_operation);
    close(&[&source, &target, &engine]).await;
    fresh.drain().await.unwrap();
    pair.finish().await;
}
