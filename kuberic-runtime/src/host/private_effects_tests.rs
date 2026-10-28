use super::*;
use crate::replicator::{TestingTcpRoute, TestingTcpRouteKind as Cut};

async fn selected_idle_build() -> (Fixture, Fixture, ReplicaGoal, ReplicaInformation) {
    let source = native_fixture().await;
    let target = native_fixture_for(2).await;
    source.link(&target);
    target.link(&source);
    source.submit(&source.goal).await.unwrap();
    source.achieved(&source.goal).await;
    target.submit(&target.goal).await.unwrap();
    target.achieved(&target.goal).await;
    target.acknowledge(&target.goal).await;
    let source_publication = source.snapshot().await.publication.unwrap();
    let target_publication = target.snapshot().await.publication.unwrap();
    let information = ReplicaInformation {
        identity: target_publication.scope.storage.replica.clone(),
        process_session_id: target_publication.scope.process_session_id.clone(),
        storage_incarnation: Some(target_publication.scope.storage.clone()),
        replication_address: target_publication.replication_address.clone(),
        role: ReplicaRole::IdleSecondary,
        current_progress: crate::application::INVALID_LSN,
        catch_up_capability: crate::application::INVALID_LSN,
        must_catchup: false,
    };
    let mut build = source.goal.clone();
    build.operation_id = OperationId::new("creation-owned-build");
    build.revision = 2;
    build.desired = ReplicaGoalKind::Build {
        remote: Box::new(RemoteReplicaSelection {
            source: source_publication,
            target: target_publication,
            target_control_address: target.address.clone(),
            generation: 1,
            original_member: None,
        }),
    };
    (source, target, build, information)
}

async fn native_fixture_for(id: i64) -> Fixture {
    let fixture = Fixture::new(id, false).await;
    fixture
        .probe
        .use_default_factory
        .store(true, Ordering::Release);
    fixture
}

async fn copy_end(target: &Fixture) {
    let writer = target.probe.native_writer.lock().unwrap().clone().unwrap();
    let mut stream = writer.copy_operations().unwrap();
    loop {
        let operation = tokio::time::timeout(Duration::from_secs(5), stream.get_operation())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let end = operation.end_of_stream;
        operation.acknowledge().unwrap();
        if end {
            break;
        }
    }
}

async fn finish_faulted(fixture: Fixture, expected: &str) {
    fixture.runtime.withdraw_ordinary();
    fixture.shutdown.send_replace(true);
    if let Err(error) = tokio::time::timeout(Duration::from_secs(10), fixture.server)
        .await
        .unwrap()
        .unwrap()
    {
        assert!(error.to_string().contains(expected), "{error}");
    }
    let error = fixture.runtime.shutdown_and_drain().await.unwrap_err();
    assert!(error.to_string().contains(expected), "{error}");
    let again = fixture.runtime.shutdown_and_drain().await.unwrap_err();
    assert!(again.to_string().contains(expected), "{again}");
}

struct NativeGroup {
    source: Fixture,
    promoted: Fixture,
    receiver: Fixture,
    original: ConfigurationDescriptor,
    publications: Vec<ReplicaPublication>,
    promoted_stream: OperationStream,
    receiver_stream: OperationStream,
    route: TestingTcpRoute,
    old_ack: serde_json::Value,
}

fn member_configuration(
    publications: &[ReplicaPublication],
    local: i64,
    current: &ConfigurationDescriptor,
    previous: Option<ConfigurationDescriptor>,
    old_down: bool,
    progress: i64,
) -> ReplicaConfigurationScope {
    ReplicaConfigurationScope {
        current: current.clone(),
        previous,
        replicas: publications
            .iter()
            .filter(|publication| publication.scope.storage.replica.replica_id.value() != local)
            .map(|publication| {
                let id = publication.scope.storage.replica.replica_id;
                ReplicaPeerDescription {
                    identity: publication.scope.storage.replica.clone(),
                    replication_address: publication.replication_address.clone(),
                    process_session_id: publication.scope.process_session_id.clone(),
                    storage_incarnation: publication.scope.storage.clone(),
                    role: if id == current.primary_id {
                        ReplicaRole::Primary
                    } else {
                        ReplicaRole::ActiveSecondary
                    },
                    current_progress: progress,
                    catch_up_capability: 0,
                    is_up: !old_down || id.value() != 1,
                    is_ready: !old_down || id.value() != 1,
                    must_catchup: false,
                }
            })
            .collect(),
    }
}

async fn actual_native_group() -> NativeGroup {
    let route = TestingTcpRoute::register_with_kind("pod-3".into(), |_, _, endpoint| {
        Box::pin(async move { Ok(endpoint) })
    })
    .unwrap();
    let mut source = native_fixture().await;
    let mut promoted = native_fixture_for(2).await;
    let mut receiver = native_fixture_for(3).await;
    source._root.disable_cleanup(true);
    promoted._root.disable_cleanup(true);
    receiver._root.disable_cleanup(true);
    eprintln!(
        "retained general roots: {:?}, {:?}, {:?}",
        source._root.path(),
        promoted._root.path(),
        receiver._root.path()
    );
    for local in [&source, &promoted, &receiver] {
        for remote in [&source, &promoted, &receiver] {
            if local.goal.scope.storage.replica != remote.goal.scope.storage.replica {
                local.link(remote);
            }
        }
        local.submit(&local.goal).await.unwrap();
        local.achieved(&local.goal).await;
        local.acknowledge(&local.goal).await;
    }
    let original_source = source.snapshot().await.publication.unwrap();
    for (revision, target) in [(2, &promoted), (3, &receiver)] {
        let mut build = source.goal.clone();
        build.operation_id = OperationId::new(format!("general-real-build-{revision}"));
        build.revision = revision;
        build.desired = ReplicaGoalKind::Build {
            remote: Box::new(RemoteReplicaSelection {
                source: original_source.clone(),
                target: target.snapshot().await.publication.unwrap(),
                target_control_address: target.address.clone(),
                generation: 1,
                original_member: None,
            }),
        };
        source.submit(&build).await.unwrap();
        copy_end(target).await;
        source.achieved(&build).await;
        source.acknowledge(&build).await;
    }
    let original = ConfigurationDescriptor::new(
        Epoch::new(0, 1),
        replica(1).replica_id,
        vec![
            ConfigurationMember {
                identity: replica(1),
                role: ReplicaRole::Primary,
            },
            ConfigurationMember {
                identity: replica(2),
                role: ReplicaRole::ActiveSecondary,
            },
            ConfigurationMember {
                identity: replica(3),
                role: ReplicaRole::ActiveSecondary,
            },
        ],
        2,
    );
    let mut publications = Vec::new();
    for fixture in [&source, &promoted, &receiver] {
        publications.push(fixture.snapshot().await.publication.unwrap());
    }
    let configuration = |local: i64,
                         current: &ConfigurationDescriptor,
                         previous: Option<ConfigurationDescriptor>,
                         old_down: bool,
                         progress: i64| {
        member_configuration(&publications, local, current, previous, old_down, progress)
    };
    for fixture in [&source, &promoted, &receiver] {
        let mut admit = fixture.goal.clone();
        admit.operation_id = OperationId::new("general-original-joint");
        admit.revision = if fixture.goal.role() == Some(ReplicaRole::Primary) {
            4
        } else {
            2
        };
        admit.scope.configuration = configuration(
            fixture.goal.scope.storage.replica.replica_id.value(),
            &original,
            Some(original_source.scope.configuration.current.clone()),
            false,
            0,
        );
        admit.desired = ReplicaGoalKind::Reconfigure {
            role: if fixture.goal.role() == Some(ReplicaRole::Primary) {
                ReplicaRole::Primary
            } else {
                ReplicaRole::ActiveSecondary
            },
            catch_up: None,
        };
        fixture.submit(&admit).await.unwrap();
        fixture.achieved(&admit).await;
        fixture.acknowledge(&admit).await;
    }
    for fixture in [&source, &promoted, &receiver] {
        let mut current = fixture.goal.clone();
        let primary = fixture.goal.role() == Some(ReplicaRole::Primary);
        current.operation_id = OperationId::new("general-original-current");
        current.revision = if primary { 5 } else { 3 };
        current.scope.configuration = configuration(
            fixture.goal.scope.storage.replica.replica_id.value(),
            &original,
            None,
            false,
            0,
        );
        current.desired = ReplicaGoalKind::Reconfigure {
            role: if primary {
                ReplicaRole::Primary
            } else {
                ReplicaRole::ActiveSecondary
            },
            catch_up: primary.then_some(PublicCatchUpMode::WriteQuorum),
        };
        fixture.submit(&current).await.unwrap();
        fixture.achieved(&current).await;
        fixture.acknowledge(&current).await;
    }
    let source_writer = source.probe.native_writer.lock().unwrap().clone().unwrap();
    let promoted_writer = promoted
        .probe
        .native_writer
        .lock()
        .unwrap()
        .clone()
        .unwrap();
    let receiver_writer = receiver
        .probe
        .native_writer
        .lock()
        .unwrap()
        .clone()
        .unwrap();
    let mut promoted_stream = promoted_writer.replication_operations().unwrap();
    let mut receiver_stream = receiver_writer.replication_operations().unwrap();
    let first = source_writer
        .begin_replicate(bytes::Bytes::from_static(b"before-general-election"))
        .unwrap();
    assert_eq!(first.lsn, 1);
    for stream in [&mut promoted_stream, &mut receiver_stream] {
        let operation = tokio::time::timeout(Duration::from_secs(5), stream.get_operation())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(operation.lsn, 1);
        operation.acknowledge().unwrap();
    }
    first.completion.await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while route.inspect_native().unwrap()["receivers"][0]["replication"]["applied"] != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let old = route.inspect_native().unwrap();
    let old_ack = old["peers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|peer| peer["current"] == true)
        .unwrap()["ackOwner"]
        .clone();
    NativeGroup {
        source,
        promoted,
        receiver,
        original,
        publications,
        promoted_stream,
        receiver_stream,
        route,
        old_ack,
    }
}

#[tokio::test]
async fn native_general_same_members_higher_fence_without_normal_result_serves_real_next_write() {
    let NativeGroup {
        source,
        promoted,
        receiver,
        original,
        publications,
        mut promoted_stream,
        mut receiver_stream,
        route,
        old_ack,
    } = actual_native_group().await;
    let promoted_writer = promoted
        .probe
        .native_writer
        .lock()
        .unwrap()
        .clone()
        .unwrap();
    let configuration = |local, current: &ConfigurationDescriptor, previous, old_down, progress| {
        member_configuration(&publications, local, current, previous, old_down, progress)
    };
    assert!(source.snapshot().await.normal_handoff_result().is_none());
    source.finish().await;
    let selected = ConfigurationDescriptor::new(
        Epoch::new(0, 2),
        replica(2).replica_id,
        vec![
            ConfigurationMember {
                identity: replica(1),
                role: ReplicaRole::ActiveSecondary,
            },
            ConfigurationMember {
                identity: replica(2),
                role: ReplicaRole::Primary,
            },
            ConfigurationMember {
                identity: replica(3),
                role: ReplicaRole::ActiveSecondary,
            },
        ],
        original.write_quorum,
    );
    let mut equal = promoted.goal.clone();
    equal.operation_id = OperationId::new("general-unqualified-equal-fence");
    equal.revision = 4;
    let mut equal_current = selected.clone();
    equal_current.epoch = original.epoch;
    equal_current.configuration_id = equal_current.expected_id();
    equal.scope.configuration = configuration(2, &equal_current, Some(original.clone()), true, 1);
    equal.desired = ReplicaGoalKind::Reconfigure {
        role: ReplicaRole::Primary,
        catch_up: Some(PublicCatchUpMode::WriteQuorum),
    };
    let before_equal = promoted.store.load_state().await.unwrap();
    assert!(
        promoted.submit(&equal).await.is_err(),
        "unqualified equality must not acquire general recovery"
    );
    assert_eq!(promoted.store.load_state().await.unwrap(), before_equal);
    let mut selected_queries = Vec::new();
    for fixture in [&promoted, &receiver] {
        let mut query = fixture.goal.clone();
        query.operation_id = OperationId::new("general-higher-query");
        query.revision = 4;
        query.scope.epoch = selected.epoch;
        query.scope.configuration = configuration(
            fixture.goal.scope.storage.replica.replica_id.value(),
            &selected,
            Some(original.clone()),
            true,
            1,
        );
        query.desired = ReplicaGoalKind::Discover;
        query.scope.configuration.replicas.clear();
        fixture.submit(&query).await.unwrap();
        let queried = match futures::FutureExt::catch_unwind(std::panic::AssertUnwindSafe(
            fixture.achieved(&query),
        ))
        .await
        {
            Ok(snapshot) => snapshot,
            Err(error) => {
                eprintln!(
                    "actual general query native diagnostic: {}",
                    route.inspect_native().unwrap()
                );
                std::panic::resume_unwind(error);
            }
        };
        let status = queried.native_status.unwrap();
        assert_eq!(status.progress.value(), Some(1));
        selected_queries.push(status);
        fixture.acknowledge(&query).await;
    }
    let promotion_consumer = tokio::spawn(async move {
        while let Some(operation) = promoted_stream.get_operation().await.unwrap() {
            assert!(
                operation.end_of_stream,
                "promotion must drain actual EOS, not invented data"
            );
            operation.acknowledge().unwrap();
        }
    });
    for fixture in [&receiver, &promoted] {
        let primary = fixture.goal.scope.storage.replica.replica_id.value() == 2;
        let mut joint = fixture.goal.clone();
        joint.operation_id = OperationId::new("general-higher-joint");
        joint.revision = 5;
        joint.scope.epoch = selected.epoch;
        joint.scope.configuration = configuration(
            fixture.goal.scope.storage.replica.replica_id.value(),
            &selected,
            Some(original.clone()),
            true,
            1,
        );
        for peer in &mut joint.scope.configuration.replicas {
            if let Some(query) = selected_queries
                .iter()
                .find(|query| query.input.scope.storage.replica == peer.identity)
            {
                peer.current_progress = query.progress.value().unwrap();
                peer.catch_up_capability = query.capability.value().unwrap();
            }
        }
        joint.desired = ReplicaGoalKind::Reconfigure {
            role: if primary {
                ReplicaRole::Primary
            } else {
                ReplicaRole::ActiveSecondary
            },
            catch_up: primary.then_some(PublicCatchUpMode::WriteQuorum),
        };
        fixture.submit(&joint).await.unwrap();
        fixture.achieved(&joint).await;
        fixture.acknowledge(&joint).await;
    }
    tokio::time::timeout(Duration::from_secs(5), promotion_consumer)
        .await
        .unwrap()
        .unwrap();
    for fixture in [&promoted, &receiver] {
        let primary = fixture.goal.scope.storage.replica.replica_id.value() == 2;
        let mut current = fixture.goal.clone();
        current.operation_id = OperationId::new("general-higher-current");
        current.revision = 6;
        current.scope.epoch = selected.epoch;
        current.scope.configuration = configuration(
            fixture.goal.scope.storage.replica.replica_id.value(),
            &selected,
            None,
            true,
            1,
        );
        for peer in &mut current.scope.configuration.replicas {
            if let Some(query) = selected_queries
                .iter()
                .find(|query| query.input.scope.storage.replica == peer.identity)
            {
                peer.current_progress = query.progress.value().unwrap();
                peer.catch_up_capability = query.capability.value().unwrap();
            }
        }
        current.desired = ReplicaGoalKind::Reconfigure {
            role: if primary {
                ReplicaRole::Primary
            } else {
                ReplicaRole::ActiveSecondary
            },
            catch_up: primary.then_some(PublicCatchUpMode::WriteQuorum),
        };
        fixture.submit(&current).await.unwrap();
        fixture.achieved(&current).await;
        fixture.acknowledge(&current).await;
    }
    let continued = route.inspect_native().unwrap();
    let peer = continued["peers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|peer| peer["current"] == true)
        .unwrap();
    assert_eq!(
        peer["ackOwner"], old_ack,
        "general reformation must retain the actual copied native root"
    );
    let write = promoted_writer
        .begin_replicate(bytes::Bytes::from_static(b"after-general-election"))
        .unwrap();
    assert_eq!(write.lsn, 2);
    let operation = tokio::time::timeout(Duration::from_secs(5), receiver_stream.get_operation())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(operation.lsn, 2);
    assert_eq!(
        operation.data,
        bytes::Bytes::from_static(b"after-general-election")
    );
    let mut completion = Box::pin(write.completion);
    assert!(
        tokio::time::timeout(Duration::from_millis(30), &mut completion)
            .await
            .is_err(),
        "selected progress cannot acknowledge a future LSN without its native consumer"
    );
    operation.acknowledge().unwrap();
    tokio::time::timeout(Duration::from_secs(5), completion)
        .await
        .unwrap()
        .unwrap();
    let state = promoted.store.load_state().await.unwrap();
    let input = state.public_v1_operation_state.as_ref().unwrap().operations
        [&OperationId::new("general-higher-joint")]
        .ordinary
        .as_ref()
        .unwrap()
        .native_input
        .as_ref()
        .unwrap();
    assert!(
        input.source.is_none(),
        "general dispatch must not require an old normal source result"
    );
    promoted.finish().await;
    receiver.finish().await;
}

#[tokio::test]
async fn native_normal_at_tail_first_incoming_keeps_no_initial_ack_then_requires_real_client_ack() {
    actual_normal_members(false, false, None, None).await;
}

#[tokio::test]
async fn native_normal_write_quorum_unmarked_copied_l_below_b_replays_same_root_without_all_wait() {
    actual_normal_members(true, false, None, None).await;
}

#[tokio::test]
async fn native_normal_first_association_retains_configuration_across_observer_loss_and_overlap() {
    actual_normal_members(false, true, None, None).await;
}

#[tokio::test]
async fn native_normal_first_association_partial_error_is_sticky_and_cannot_reconfirm_active() {
    actual_normal_members(false, false, Some(Cut::ReceiverReplaced), None).await;
}

#[tokio::test]
async fn native_normal_configuration_joins_old_outgoing_task_across_observer_loss_before_first_association()
 {
    actual_normal_members(false, true, None, Some(false)).await;
}

#[tokio::test]
async fn native_normal_configuration_retains_actual_old_outgoing_join_failure_before_first_association()
 {
    actual_normal_members(false, false, None, Some(true)).await;
}

#[tokio::test]
async fn native_normal_first_incoming_peer_constructor_failure_retains_failed_owner() {
    actual_normal_members(false, false, Some(Cut::PeerCreating), None).await;
}

#[tokio::test]
async fn native_normal_first_incoming_installed_peer_failure_joins_actual_partial_handles() {
    actual_normal_members(false, false, Some(Cut::PeerInstalled), None).await;
}

async fn actual_normal_members(
    replay: bool,
    observer_loss: bool,
    partial_failure: Option<Cut>,
    join_cut: Option<bool>,
) {
    let entered = Arc::new(Semaphore::new(0));
    let release = Arc::new(Semaphore::new(0));
    let associations = Arc::new(AtomicUsize::new(0));
    let hold_old_ack = Arc::new(AtomicBool::new(false));
    let partial_armed = Arc::new(AtomicBool::new(false));
    let former_route = TestingTcpRoute::register_with_kind("pod-1".into(), {
        let entered = entered.clone();
        let release = release.clone();
        let associations = associations.clone();
        let hold_old_ack = hold_old_ack.clone();
        let partial_armed = partial_armed.clone();
        move |kind, peer, endpoint| {
            let entered = entered.clone();
            let release = release.clone();
            let associations = associations.clone();
            let hold_old_ack = hold_old_ack.clone();
            let partial_armed = partial_armed.clone();
            Box::pin(async move {
                if join_cut.is_some() && kind == Cut::BeforeDispatch && peer == "pod-3"
                    && hold_old_ack.swap(false, Ordering::SeqCst)
                {
                    associations.fetch_add(1, Ordering::SeqCst);
                    entered.add_permits(1);
                    release.acquire().await.unwrap().forget();
                    if join_cut == Some(true) {
                        return Err(std::io::Error::other("synthetic negative: actual first association partial effect: old outgoing task"));
                    }
                }
                if join_cut.is_none() && (observer_loss && kind == Cut::ReceiverReplaced
                    || partial_armed.load(Ordering::SeqCst) && partial_failure == Some(kind)) {
                    associations.fetch_add(1, Ordering::SeqCst);
                    entered.add_permits(1);
                    if partial_failure.is_some() {
                        return Err(std::io::Error::other("synthetic negative: actual first association partial effect"));
                    }
                    release.acquire().await.unwrap().forget();
                }
                Ok(endpoint)
            })
        }
    }).unwrap();
    let new_route = TestingTcpRoute::register_with_kind("pod-2".into(), |_, _, endpoint| {
        Box::pin(async move { Ok(endpoint) })
    })
    .unwrap();
    let NativeGroup {
        source,
        promoted,
        receiver,
        original,
        publications,
        mut promoted_stream,
        mut receiver_stream,
        route,
        old_ack,
    } = actual_native_group().await;
    let old_writer = source.probe.native_writer.lock().unwrap().clone().unwrap();
    let writer = promoted
        .probe
        .native_writer
        .lock()
        .unwrap()
        .clone()
        .unwrap();
    if join_cut.is_some() {
        hold_old_ack.store(true, Ordering::SeqCst);
        let write = old_writer
            .begin_replicate(bytes::Bytes::from_static(b"before-real-old-outgoing-join"))
            .unwrap();
        assert_eq!(write.lsn, 2);
        for stream in [&mut promoted_stream, &mut receiver_stream] {
            let operation = tokio::time::timeout(Duration::from_secs(5), stream.get_operation())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            assert_eq!(operation.lsn, 2);
            operation.acknowledge().unwrap();
        }
        tokio::time::timeout(Duration::from_secs(5), write.completion)
            .await
            .unwrap()
            .unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while route.inspect_native().unwrap()["receivers"][0]["replication"]["applied"] != 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }
    let refreshed = ConfigurationDescriptor::new(
        Epoch::new(0, 2),
        replica(1).replica_id,
        original.members.clone(),
        original.write_quorum,
    );
    let selected = ConfigurationDescriptor::new(
        Epoch::new(0, 2),
        replica(2).replica_id,
        vec![
            ConfigurationMember {
                identity: replica(1),
                role: ReplicaRole::ActiveSecondary,
            },
            ConfigurationMember {
                identity: replica(2),
                role: ReplicaRole::Primary,
            },
            ConfigurationMember {
                identity: replica(3),
                role: ReplicaRole::ActiveSecondary,
            },
        ],
        original.write_quorum,
    );
    let configuration = |local, current: &ConfigurationDescriptor, previous, boundary| {
        let mut configuration =
            member_configuration(&publications, local, current, previous, false, 1);
        for peer in &mut configuration.replicas {
            peer.current_progress = if peer.identity.replica_id.value() == 3 && join_cut.is_none() {
                1
            } else {
                boundary
            };
            peer.catch_up_capability = 1;
            peer.must_catchup =
                current.primary_id.value() == 2 && peer.identity.replica_id.value() == 2;
        }
        configuration
    };
    let boundary = if replay || join_cut.is_some() { 2 } else { 1 };
    for fixture in [&receiver, &promoted] {
        let mut prepare = fixture.goal.clone();
        prepare.operation_id = OperationId::new("normal-actual-preparation");
        prepare.revision = 4;
        prepare.scope.epoch = selected.epoch;
        prepare.scope.configuration = configuration(
            fixture.goal.scope.storage.replica.replica_id.value(),
            &refreshed,
            Some(original.clone()),
            boundary,
        );
        prepare.desired = ReplicaGoalKind::Reconfigure {
            role: ReplicaRole::ActiveSecondary,
            catch_up: None,
        };
        fixture.submit(&prepare).await.unwrap();
        fixture.achieved(&prepare).await;
        fixture.acknowledge(&prepare).await;
        if replay && fixture.goal.scope.storage.replica.replica_id.value() == 3 {
            let write = old_writer
                .begin_replicate(bytes::Bytes::from_static(b"normal-unmarked-missing-prefix"))
                .unwrap();
            assert_eq!(write.lsn, boundary);
            let operation =
                tokio::time::timeout(Duration::from_secs(5), promoted_stream.get_operation())
                    .await
                    .unwrap()
                    .unwrap()
                    .unwrap();
            assert_eq!(operation.lsn, boundary);
            operation.acknowledge().unwrap();
            tokio::time::timeout(Duration::from_secs(5), write.completion)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                route.inspect_native().unwrap()["receivers"][0]["replication"]["applied"],
                1
            );
        }
    }
    let mut handoff = source.goal.clone();
    handoff.operation_id = OperationId::new("normal-actual-full-source-result");
    handoff.revision = 6;
    handoff.scope.epoch = selected.epoch;
    handoff.scope.configuration = configuration(1, &selected, Some(original.clone()), boundary);
    handoff.desired = ReplicaGoalKind::Reconfigure {
        role: ReplicaRole::ActiveSecondary,
        catch_up: Some(PublicCatchUpMode::WriteQuorum),
    };
    source.submit(&handoff).await.unwrap();
    if let Some(fail) = join_cut {
        super::super::entered(&entered).await;
        let held = former_route.inspect_native().unwrap();
        assert!(held["sessions"]["receiver"].is_null());
        assert!(source.snapshot().await.normal_handoff_result().is_none());
        let state = source.store.load_state().await.unwrap();
        let selected =
            &state.public_v1_operation_state.as_ref().unwrap().operations[&handoff.operation_id];
        assert_eq!(
            selected.lifecycle.in_flight,
            Some(I::RefreshedConfiguration)
        );
        assert_eq!(selected.lifecycle.outcomes.len(), 2);
        let (current, previous) = crate::host::public_lifecycle::selected_replica_sets(
            &handoff,
            "RefreshedConfiguration",
        )
        .unwrap();
        let previous = previous.unwrap();
        let native = source.probe.native_primary.lock().unwrap().clone().unwrap();
        let mut retry = None;
        if !fail {
            let observer = tokio::spawn({
                let native = native.clone();
                let current = current.clone();
                let previous = previous.clone();
                async move {
                    native
                        .update_catch_up_replica_set_configuration(current, previous)
                        .await
                }
            });
            tokio::time::sleep(Duration::from_millis(30)).await;
            assert!(
                !observer.is_finished(),
                "held real old Peer task cannot count as joined"
            );
            observer.abort();
            assert!(observer.await.unwrap_err().is_cancelled());
            retry = Some(tokio::spawn({
                let native = native.clone();
                let current = current.clone();
                let previous = previous.clone();
                async move {
                    native
                        .update_catch_up_replica_set_configuration(current, previous)
                        .await
                }
            }));
            tokio::time::sleep(Duration::from_millis(30)).await;
            assert!(!retry.as_ref().unwrap().is_finished());
        }
        release.add_permits(1);
        if fail {
            tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    if source.snapshot().await.goals.iter().any(|entry| {
                        entry.goal == handoff
                            && matches!(&entry.status, ReplicaGoalStatus::Failed(_))
                    }) {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            let failed = former_route.inspect_native().unwrap();
            assert!(
                failed["failure"]
                    .as_str()
                    .unwrap()
                    .contains("actual first association partial effect")
            );
            assert!(source.snapshot().await.normal_handoff_result().is_none());
            assert!(
                native
                    .update_catch_up_replica_set_configuration(current, previous)
                    .await
                    .is_err()
            );
            assert_eq!(associations.load(Ordering::SeqCst), 1);
            for fixture in [&source, &promoted, &receiver] {
                fixture.runtime.withdraw_ordinary();
                fixture
                    .runtime
                    .public_lifecycle_callbacks()
                    .unwrap()
                    .replicator
                    .abort();
            }
            finish_partial_association(source, true).await;
            finish_partial_association(promoted, false).await;
            finish_partial_association(receiver, false).await;
            return;
        }
        tokio::time::timeout(Duration::from_secs(5), retry.unwrap())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(associations.load(Ordering::SeqCst), 1);
    }
    let demoted = source.achieved(&handoff).await;
    source.acknowledge(&handoff).await;
    let full = source.snapshot().await;
    if std::env::var_os("KUBERIC_M3_NATIVE_SCOPE_DIAGNOSTICS").is_some() {
        eprintln!(
            "normal-native-producer-result: {}",
            serde_json::to_string(&full).unwrap()
        );
    }
    assert_eq!(full.normal_handoff_result().unwrap().goal, handoff);
    assert_eq!(
        full.native_lifecycle
            .as_ref()
            .unwrap()
            .deactivation
            .as_ref()
            .unwrap()
            .catch_up_lsn,
        boundary
    );
    assert!(demoted.write_status != AccessStatus::Granted);
    let before = former_route.inspect_native().unwrap();
    assert!(
        before["sessions"]["receiver"].is_null(),
        "former Primary must have no inbound predecessor"
    );
    let demotion_root = before["receivers"][0]["owner"].clone();
    let mut demotion_copy = old_writer.copy_operations().unwrap();
    assert!(
        tokio::time::timeout(Duration::from_secs(5), demotion_copy.get_operation())
            .await
            .unwrap()
            .unwrap()
            .is_none(),
        "from-Primary copy EOS has no numbered copy data"
    );
    let mut former_stream = old_writer.replication_operations().unwrap();
    partial_armed.store(true, Ordering::SeqCst);
    let mut partial_failed = false;
    let promotion_consumer = tokio::spawn(async move {
        while let Some(operation) = promoted_stream.get_operation().await.unwrap() {
            assert!(operation.end_of_stream);
            operation.acknowledge().unwrap();
        }
    });
    for fixture in [&receiver, &promoted] {
        let mut query = fixture.goal.clone();
        query.operation_id = OperationId::new("normal-actual-joint-query");
        query.revision = 5;
        query.scope.epoch = selected.epoch;
        query.scope.configuration = configuration(
            fixture.goal.scope.storage.replica.replica_id.value(),
            &selected,
            Some(original.clone()),
            boundary,
        );
        query.scope.configuration.replicas.clear();
        query.desired = ReplicaGoalKind::Discover;
        fixture.submit(&query).await.unwrap();
        let observed = fixture.achieved(&query).await;
        assert_eq!(
            observed.native_status.unwrap().progress.value(),
            Some(
                if fixture.goal.scope.storage.replica.replica_id.value() == 3 && join_cut.is_none()
                {
                    1
                } else {
                    boundary
                }
            )
        );
        fixture.acknowledge(&query).await;
        let mut joint = query.clone();
        joint.operation_id = OperationId::new("normal-actual-joint");
        joint.revision = 6;
        joint.scope.configuration = configuration(
            fixture.goal.scope.storage.replica.replica_id.value(),
            &selected,
            Some(original.clone()),
            boundary,
        );
        let primary = fixture.goal.scope.storage.replica.replica_id.value() == 2;
        joint.desired = ReplicaGoalKind::Reconfigure {
            role: if primary {
                ReplicaRole::Primary
            } else {
                ReplicaRole::ActiveSecondary
            },
            catch_up: primary.then_some(PublicCatchUpMode::WriteQuorum),
        };
        fixture.submit(&joint).await.unwrap();
        if primary && partial_failure.is_some() {
            super::super::entered(&entered).await;
            tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    if promoted.snapshot().await.goals.iter().any(|entry| {
                        entry.goal == joint
                            && matches!(&entry.status, ReplicaGoalStatus::Failed(error)
                            if error.contains("actual first association partial effect"))
                    }) {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            let failed = former_route.inspect_native().unwrap();
            assert_eq!(failed["receivers"][0]["owner"], demotion_root);
            assert!(
                failed["failure"]
                    .as_str()
                    .unwrap()
                    .contains("actual first association partial effect")
            );
            assert!(
                failed["peers"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .all(|peer| peer["current"] != true)
            );
            let before = promoted.store.load_state().await.unwrap();
            let _retained_failed_rpc = promoted.submit(&joint).await;
            let (current, previous) =
                crate::host::public_lifecycle::selected_replica_sets(&joint, "Configuration")
                    .unwrap();
            let native = promoted
                .probe
                .native_primary
                .lock()
                .unwrap()
                .clone()
                .unwrap();
            assert!(
                native
                    .update_catch_up_replica_set_configuration(current, previous.unwrap())
                    .await
                    .is_err()
            );
            assert_eq!(promoted.store.load_state().await.unwrap(), before);
            assert_eq!(associations.load(Ordering::SeqCst), 1);
            assert_eq!(
                former_route.inspect_native().unwrap()["effects"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter(|effect| effect["effect"] == "Control(Reform)")
                    .count(),
                1
            );
            let mut final_goal = joint.clone();
            final_goal.operation_id = OperationId::new("normal-failed-joint-cannot-finalize");
            final_goal.revision += 1;
            final_goal.scope.configuration.previous = None;
            assert!(promoted.submit(&final_goal).await.is_err());
            assert_eq!(promoted.store.load_state().await.unwrap(), before);
            for fixture in [&source, &promoted, &receiver] {
                fixture.runtime.withdraw_ordinary();
                fixture
                    .runtime
                    .public_lifecycle_callbacks()
                    .unwrap()
                    .replicator
                    .abort();
            }
            partial_failed = true;
            break;
        }
        let mut retry = None;
        if primary && observer_loss && join_cut.is_none() {
            super::super::entered(&entered).await;
            let partial = former_route.inspect_native().unwrap();
            assert_eq!(partial["receivers"][0]["owner"], demotion_root);
            if join_cut.is_some() {
                assert!(partial["sessions"]["receiver"].is_null());
                assert!(!partial["retiringPeers"].as_array().unwrap().is_empty());
            } else {
                assert!(partial["sessions"]["receiver"].is_string());
            }
            assert!(
                partial["peers"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .all(|peer| peer["current"] != true)
            );
            let (current, previous) =
                crate::host::public_lifecycle::selected_replica_sets(&joint, "Configuration")
                    .unwrap();
            let previous = previous.unwrap();
            let native = promoted
                .probe
                .native_primary
                .lock()
                .unwrap()
                .clone()
                .unwrap();
            let observer = tokio::spawn({
                let native = native.clone();
                let current = current.clone();
                let previous = previous.clone();
                async move {
                    native
                        .update_catch_up_replica_set_configuration(current, previous)
                        .await
                }
            });
            tokio::time::sleep(Duration::from_millis(30)).await;
            assert!(
                !observer.is_finished(),
                "partial first association is not successful Configuration"
            );
            observer.abort();
            assert!(observer.await.unwrap_err().is_cancelled());
            let mut crossed = current.clone();
            crossed.replicas[0].current_progress += 1;
            let failure = native
                .update_catch_up_replica_set_configuration(crossed.clone(), previous.clone())
                .await
                .unwrap_err()
                .to_string();
            assert!(
                failure.contains("admitted") || failure.contains("configuration"),
                "{failure}"
            );
            assert_eq!(
                native
                    .update_catch_up_replica_set_configuration(crossed, previous.clone())
                    .await
                    .unwrap_err()
                    .to_string(),
                failure,
                "changed exact input has its retained error, not Active success"
            );
            retry = Some(tokio::spawn(async move {
                native
                    .update_catch_up_replica_set_configuration(current, previous)
                    .await
            }));
            tokio::time::sleep(Duration::from_millis(30)).await;
            assert!(!retry.as_ref().unwrap().is_finished());
            release.add_permits(1);
        }
        let joint_result = match futures::FutureExt::catch_unwind(std::panic::AssertUnwindSafe(
            fixture.achieved(&joint),
        ))
        .await
        {
            Ok(snapshot) => snapshot,
            Err(error) => {
                eprintln!(
                    "actual normal new source: {}",
                    new_route.inspect_native().unwrap()
                );
                eprintln!(
                    "actual normal former Primary: {}",
                    former_route.inspect_native().unwrap()
                );
                eprintln!(
                    "actual normal copied receiver: {}",
                    route.inspect_native().unwrap()
                );
                std::panic::resume_unwind(error);
            }
        };
        assert!(
            joint_result
                .goals
                .iter()
                .any(|entry| entry.goal == joint && entry.status == ReplicaGoalStatus::Achieved)
        );
        fixture.acknowledge(&joint).await;
        if let Some(retry) = retry {
            tokio::time::timeout(Duration::from_secs(5), retry)
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            assert_eq!(associations.load(Ordering::SeqCst), 1);
            let finished = former_route.inspect_native().unwrap();
            assert!(finished["failure"].is_null());
            assert_eq!(
                finished["effects"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter(|effect| effect["effect"] == "Control(Reform)")
                    .count(),
                1
            );
        }
    }
    tokio::time::timeout(Duration::from_secs(5), promotion_consumer)
        .await
        .unwrap()
        .unwrap();
    if partial_failed {
        finish_partial_association(promoted, false).await;
        finish_partial_association(source, true).await;
        finish_partial_association(receiver, false).await;
        return;
    }
    assert_eq!(
        former_route.inspect_native().unwrap()["receivers"][0]["owner"],
        demotion_root
    );
    let continued = route.inspect_native().unwrap();
    assert_eq!(
        continued["peers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|peer| peer["current"] == true)
            .unwrap()["ackOwner"],
        old_ack
    );
    assert_eq!(
        continued["receivers"][0]["replication"]["applied"],
        if join_cut.is_some() { 2 } else { 1 },
        "unmarked L<B may not become an All-follower pre-wait"
    );
    let sender = new_route.inspect_native().unwrap();
    for peer in sender["peers"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|peer| peer["current"] == true)
    {
        if !replay || peer["binding"]["target"]["scope"]["storage"]["replica"]["replicaId"] == 1 {
            assert_eq!(peer["initialNativeAck"], -1);
            assert_eq!(peer["processedNativeAck"], -1);
        }
    }
    for fixture in [&source, &promoted, &receiver] {
        let mut current = fixture.goal.clone();
        current.operation_id = OperationId::new("normal-actual-current-end");
        current.revision = 7;
        current.scope.epoch = selected.epoch;
        current.scope.configuration = configuration(
            fixture.goal.scope.storage.replica.replica_id.value(),
            &selected,
            None,
            boundary,
        );
        let primary = fixture.goal.scope.storage.replica.replica_id.value() == 2;
        current.desired = ReplicaGoalKind::Reconfigure {
            role: if primary {
                ReplicaRole::Primary
            } else {
                ReplicaRole::ActiveSecondary
            },
            catch_up: primary.then_some(PublicCatchUpMode::WriteQuorum),
        };
        if primary {
            normal_current_pure_admission_vectors(fixture, &current).await;
        }
        let current_end = (primary && observer_loss).then(|| {
            Arc::new(PublicOperationCut::for_instruction(
                &current,
                "CurrentConfiguration",
                PublicCutPosition::CallbackSuccess,
                6,
            ))
        });
        if let Some(cut) = &current_end {
            fixture.runtime.set_public_operation_cut(Some(cut.clone()));
        }
        fixture.submit(&current).await.unwrap();
        if let Some(cut) = &current_end {
            tokio::time::timeout(Duration::from_secs(5), cut.entered.notified())
                .await
                .unwrap();
            let selected = fixture.store.load_state().await.unwrap();
            let record = &selected
                .public_v1_operation_state
                .as_ref()
                .unwrap()
                .operations[&current.operation_id];
            assert_eq!(record.lifecycle.in_flight, Some(I::CurrentConfiguration));
            assert_eq!(record.lifecycle.outcomes.len(), 6);
            assert!(
                writer
                    .begin_replicate(bytes::Bytes::from_static(b"before-native-current-receipt"))
                    .is_err()
            );
            let calls = fixture
                .probe
                .native_events
                .lock()
                .unwrap()
                .iter()
                .filter(|event| *event == "native.current")
                .count();
            assert_eq!(calls, 1);
            fixture.submit(&current).await.unwrap();
            assert_eq!(
                fixture
                    .probe
                    .native_events
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|event| *event == "native.current")
                    .count(),
                calls
            );
            cut.release_all();
        }
        fixture.achieved(&current).await;
        fixture.acknowledge(&current).await;
        if current_end.is_some() {
            fixture.runtime.set_public_operation_cut(None);
        }
    }
    let after = source.store.load_state().await.unwrap();
    assert!(
        promoted
            .probe
            .native_events
            .lock()
            .unwrap()
            .iter()
            .any(|event| event == "native.current"),
        "normal final acceptance must execute the actual native current/end callback"
    );
    let original_result =
        &after.public_v1_operation_state.as_ref().unwrap().operations[&handoff.operation_id];
    assert_eq!(
        original_result.disposition,
        Some(PublicOperationDisposition::Succeeded)
    );
    assert_eq!(original_result.lifecycle.outcomes.len(), 7);
    assert!(
        original_result.ordinary.as_ref().unwrap().accepted
            && original_result.ordinary.as_ref().unwrap().host_settled
    );
    if replay {
        let operation =
            tokio::time::timeout(Duration::from_secs(5), receiver_stream.get_operation())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
        assert_eq!(operation.lsn, boundary);
        assert_eq!(
            operation.data,
            bytes::Bytes::from_static(b"normal-unmarked-missing-prefix")
        );
        operation.acknowledge().unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while route.inspect_native().unwrap()["receivers"][0]["replication"]["applied"]
                != boundary
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }
    assert!(
        old_writer
            .begin_replicate(bytes::Bytes::from_static(b"obsolete-writer"))
            .is_err()
    );
    let write = writer
        .begin_replicate(bytes::Bytes::from_static(b"new-client-after-normal"))
        .unwrap();
    assert_eq!(write.lsn, boundary + 1);
    let former = tokio::time::timeout(Duration::from_secs(5), former_stream.get_operation())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let copied = tokio::time::timeout(Duration::from_secs(5), receiver_stream.get_operation())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(
        former.data,
        bytes::Bytes::from_static(b"new-client-after-normal")
    );
    assert_eq!(copied.data, former.data);
    assert_eq!(former.lsn, write.lsn);
    let mut completed = Box::pin(write.completion);
    assert!(
        tokio::time::timeout(Duration::from_millis(30), &mut completed)
            .await
            .is_err()
    );
    former.acknowledge().unwrap();
    tokio::time::timeout(Duration::from_secs(5), completed)
        .await
        .unwrap()
        .unwrap();
    copied.acknowledge().unwrap();
    assert_eq!(
        former_route.inspect_native().unwrap()["receivers"][0]["owner"],
        demotion_root
    );
    tokio::time::timeout(Duration::from_secs(5), async {
        while route.inspect_native().unwrap()["receivers"][0]["replication"]["applied"]
            != boundary + 1
            || former_route.inspect_native().unwrap()["ackPending"] != 0
            || route.inspect_native().unwrap()["ackPending"] != 0
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    if observer_loss {
        let state = promoted.store.load_state().await.unwrap();
        let mut next = state.public_v1_operation_state.as_ref().unwrap().operations
            [&OperationId::new("normal-actual-current-end")]
            .intent
            .goal()
            .unwrap()
            .clone();
        next.operation_id = OperationId::new("normal-lost-current-owner");
        next.revision += 1;
        source.runtime.withdraw_ordinary();
        assert!(
            promoted.submit(&next).await.is_err(),
            "normal source history cannot recreate its actual current owner"
        );
        promoted.runtime.withdraw_ordinary();
        assert!(
            promoted.submit(&next).await.is_err(),
            "historical joint cannot recreate the current owner"
        );
        assert_eq!(promoted.store.load_state().await.unwrap(), state);
    }
    promoted.finish().await;
    source.finish().await;
    receiver.finish().await;
}

async fn finish_partial_association(fixture: Fixture, sticky_owner: bool) {
    fixture.shutdown.send_replace(true);
    if let Err(error) = tokio::time::timeout(Duration::from_secs(10), fixture.server)
        .await
        .unwrap()
        .unwrap()
    {
        assert!(
            error
                .to_string()
                .contains("actual first association partial effect"),
            "{error}"
        );
    }
    let result = fixture.runtime.shutdown_and_drain().await;
    if sticky_owner {
        assert!(
            result.is_err(),
            "failed partial owner must retain its actual error"
        );
    }
    if let Err(error) = result {
        assert!(
            error
                .to_string()
                .contains("actual first association partial effect"),
            "{error}"
        );
    }
}

async fn normal_current_pure_admission_vectors(fixture: &Fixture, current: &ReplicaGoal) {
    let state = fixture.store.load_state().await.unwrap();
    crate::host::ordinary_goal::validate_admission(&state, current).unwrap();
    crate::host::ordinary_goal::validate_live_normal_current(&state, current, |facts| {
        fixture.runtime.native_facts_are_live(facts)
    })
    .unwrap();
    assert!(
        crate::host::ordinary_goal::validate_live_normal_current(&state, current, |_| false)
            .is_err()
    );
    let callbacks = fixture.runtime.public_lifecycle_callbacks().unwrap();
    let mut crossed_source = state.clone();
    crossed_source
        .public_v1_operation_state
        .as_mut()
        .unwrap()
        .operations
        .get_mut(&OperationId::new("normal-actual-joint"))
        .unwrap()
        .ordinary
        .as_mut()
        .unwrap()
        .native_input
        .as_mut()
        .unwrap()
        .source
        .as_mut()
        .unwrap()
        .scope
        .creation_id = ReplicatorCreationId::new("wrong-source-root-lifetime");
    assert!(
        crate::host::native_history::validate_normal_current_source(
            &crossed_source,
            current,
            &callbacks
        )
        .await
        .is_err(),
        "pure captured source input is not proof of the actual live source creation"
    );
    let joint_id = OperationId::new("normal-actual-joint");
    for kind in 0..23 {
        let mut crossed = state.clone();
        let operations = &mut crossed
            .public_v1_operation_state
            .as_mut()
            .unwrap()
            .operations;
        if kind == 0 {
            operations.remove(&joint_id);
        } else if kind == 19 {
            operations
                .get_mut(&OperationId::new("normal-actual-preparation"))
                .unwrap()
                .disposition = Some(PublicOperationDisposition::Failed(
                "synthetic negative: old sticky failure".into(),
            ));
        } else if kind == 20 {
            let mut failed = operations[&joint_id].clone();
            let mut other = failed.intent.goal().unwrap().clone();
            other.operation_id = OperationId::new("unrelated-sticky-goal");
            failed.intent = other.intent();
            failed.disposition = Some(PublicOperationDisposition::Failed(
                "synthetic negative: unrelated failure".into(),
            ));
            operations.insert(other.operation_id.clone(), failed);
        } else {
            let joint = operations.get_mut(&joint_id).unwrap();
            match kind {
                1 => {
                    joint.disposition = Some(PublicOperationDisposition::Failed(
                        "synthetic negative: failed joint".into(),
                    ))
                }
                2 => {
                    joint.disposition = Some(PublicOperationDisposition::Ambiguous(
                        "synthetic negative: ambiguous joint".into(),
                    ))
                }
                3 => joint.ordinary.as_mut().unwrap().native_input = None,
                10 => {
                    joint.ordinary.as_mut().unwrap().actions.pop();
                }
                11 => {
                    joint.lifecycle.outcomes.pop();
                }
                12 => joint.ordinary.as_mut().unwrap().accepted = false,
                13 => joint.ordinary.as_mut().unwrap().host_settled = false,
                14 => joint.lifecycle.in_flight = Some(I::NativeComplete),
                15 => joint.superseded_by = Some(OperationId::new("synthetic-supersession")),
                8 | 9 => {
                    let O::NativeLifecycle(facts) = joint.lifecycle.outcomes.last_mut().unwrap()
                    else {
                        unreachable!()
                    };
                    if kind == 8 {
                        facts.application_role = Some(ReplicaRole::ActiveSecondary);
                    } else {
                        facts.applied_primary_fence = Some(Epoch::new(0, 3));
                    }
                }
                _ => {
                    let input = joint
                        .ordinary
                        .as_mut()
                        .unwrap()
                        .native_input
                        .as_mut()
                        .unwrap();
                    match kind {
                        4 => input.source = None,
                        5 => {
                            input
                                .source
                                .as_mut()
                                .unwrap()
                                .deactivation
                                .as_mut()
                                .unwrap()
                                .catch_up_lsn += 1
                        }
                        6 => {
                            input.promotion.as_mut().unwrap().input.operation_id =
                                OperationId::new("wrong-query")
                        }
                        7 => {
                            input.promotion.as_mut().unwrap().epoch =
                                NativeInspectionEpoch::Failed("synthetic failed query".into())
                        }
                        16 => {
                            input.source.as_mut().unwrap().scope.history.history_id =
                                HistoryId::new("wrong-history")
                        }
                        17 => {
                            input
                                .source
                                .as_mut()
                                .unwrap()
                                .scope
                                .storage
                                .storage_generation = StorageGeneration::new("wrong-storage")
                        }
                        18 => {
                            input.lifecycle.scope.creation_id =
                                ReplicatorCreationId::new("wrong-root-lifetime")
                        }
                        21 => {
                            input.promotion.as_mut().unwrap().capability =
                                ProgressQueryResult::NotReady
                        }
                        22 => input.source.as_mut().unwrap().scope.configuration.previous = None,
                        _ => unreachable!(),
                    }
                }
            }
        }
        assert!(
            crate::host::ordinary_goal::validate_admission(&crossed, current).is_err(),
            "pure derived admission vector {kind} is not a service qualification"
        );
        if kind == 1 || kind == 20 {
            let mut higher = current.clone();
            higher.scope.epoch = Epoch::new(0, 3);
            higher.scope.configuration.current.epoch = higher.scope.epoch;
            higher.scope.configuration.current.configuration_id =
                higher.scope.configuration.current.expected_id();
            assert!(
                crate::host::ordinary_goal::validate_admission(&crossed, &higher).is_err(),
                "unrelated higher CurrentOnly cannot erase a failed obligation"
            );
        }
    }
    for kind in 0..7 {
        let mut crossed = current.clone();
        match kind {
            0 => crossed.scope.creation_id = ReplicatorCreationId::new("recreated"),
            1 => crossed.scope.process_session_id = ProcessSessionId::new("recreated"),
            2 => crossed.scope.history.history_id = HistoryId::new("foreign"),
            3 => crossed.scope.storage.storage_generation = StorageGeneration::new("foreign"),
            4 => crossed.revision -= 1,
            5 => crossed.scope.configuration.current.write_quorum -= 1,
            6 => {
                crossed.desired = ReplicaGoalKind::Reconfigure {
                    role: ReplicaRole::Primary,
                    catch_up: Some(PublicCatchUpMode::All),
                }
            }
            _ => unreachable!(),
        }
        assert!(fixture.submit(&crossed).await.is_err());
    }
    assert_eq!(fixture.store.load_state().await.unwrap(), state);
}

#[tokio::test]
async fn native_private_effect_active_before_peer_cancelled_observer_awaits_one_actual_owner() {
    let entered = Arc::new(Semaphore::new(0));
    let release = Arc::new(Semaphore::new(0));
    let calls = Arc::new(AtomicUsize::new(0));
    let route = TestingTcpRoute::register_with_kind("pod-2".into(), {
        let entered = entered.clone();
        let release = release.clone();
        let calls = calls.clone();
        move |kind, _, endpoint| {
            let entered = entered.clone();
            let release = release.clone();
            let calls = calls.clone();
            Box::pin(async move {
                if kind == Cut::TargetActivated {
                    calls.fetch_add(1, Ordering::SeqCst);
                    entered.add_permits(1);
                    release.acquire().await.unwrap().forget();
                }
                Ok(endpoint)
            })
        }
    })
    .unwrap();
    let (source, target, build, information) = selected_idle_build().await;
    source.submit(&build).await.unwrap();
    super::super::entered(&entered).await;
    let state = route.inspect_native().unwrap();
    assert_eq!(state["sessions"]["entries"][0]["phase"], "Active");
    assert!(state["peers"].as_array().unwrap().is_empty());
    let actual_binding = state["sessions"]["entries"][0]["binding"].clone();
    for fixture in [&source, &target] {
        let context = fixture.probe.context.lock().unwrap().clone().unwrap();
        crate::replicator::v1::check_build_binding_context(
            &context,
            &serde_json::to_vec(&actual_binding).unwrap(),
        )
        .await
        .unwrap();
        let mut crossed_nonce = actual_binding.clone();
        crossed_nonce["nonce"] = serde_json::Value::String(uuid::Uuid::new_v4().to_string());
        assert!(
            crate::replicator::v1::check_build_binding_context(
                &context,
                &serde_json::to_vec(&crossed_nonce).unwrap(),
            )
            .await
            .is_err(),
            "synthetic negative: guessed nonce is not the actual registered Build"
        );
        let mut idle_member = actual_binding.clone();
        idle_member["source"]["scope"]["configuration"]["current"]["members"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({
                "identity": target.goal.scope.storage.replica,
                "role": ReplicaRole::ActiveSecondary,
            }));
        assert!(
            crate::replicator::v1::check_build_binding_context(
                &context,
                &serde_json::to_vec(&idle_member).unwrap(),
            )
            .await
            .is_err(),
            "synthetic negative: logical Idle member without canonical original-member purpose"
        );
    }
    let primary = source.probe.native_primary.lock().unwrap().clone().unwrap();
    let observer = tokio::spawn({
        let primary = primary.clone();
        let information = information.clone();
        async move { primary.build_replica(information).await }
    });
    tokio::task::yield_now().await;
    assert!(
        !observer.is_finished(),
        "Active cannot confirm Peer completion"
    );
    observer.abort();
    assert!(observer.await.unwrap_err().is_cancelled());
    release.add_permits(1);
    copy_end(&target).await;
    source.achieved(&build).await;
    primary.build_replica(information).await.unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let state = route.inspect_native().unwrap();
    assert_eq!(state["sessions"]["entries"][0]["phase"], "Completed");
    assert_eq!(state["peers"].as_array().unwrap().len(), 1);
    assert!(state["failure"].is_null());
    assert_eq!(
        state["effects"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["effect"] == "Control(Reserve)")
            .count(),
        1
    );
    source.finish().await;
    target.finish().await;
}

#[tokio::test]
async fn native_private_effect_lost_reserve_reply_reuses_actual_peer_and_build_end() {
    let responses = Arc::new(AtomicUsize::new(0));
    let route = TestingTcpRoute::register_with_kind("pod-2".into(), {
        let responses = responses.clone();
        move |kind, _, endpoint| {
            let responses = responses.clone();
            Box::pin(async move {
                if kind == Cut::ReserveResponse && responses.fetch_add(1, Ordering::SeqCst) == 0 {
                    return Err(std::io::Error::other(
                        "synthetic negative: dropped first actual Reserve reply",
                    ));
                }
                Ok(endpoint)
            })
        }
    })
    .unwrap();
    let (source, target, build, information) = selected_idle_build().await;
    source.submit(&build).await.unwrap();
    copy_end(&target).await;
    source.achieved(&build).await;
    let primary = source.probe.native_primary.lock().unwrap().clone().unwrap();
    primary.build_replica(information).await.unwrap();
    let state = route.inspect_native().unwrap();
    assert!(responses.load(Ordering::SeqCst) >= 2);
    assert_eq!(
        state["effects"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["effect"] == "Peer(false)")
            .count(),
        1
    );
    assert_eq!(state["sessions"]["entries"][0]["phase"], "Completed");
    source.finish().await;
    target.finish().await;
}

#[tokio::test]
async fn native_private_effect_constructor_failure_after_active_is_sticky_and_cannot_reconfirm() {
    let constructors = Arc::new(AtomicUsize::new(0));
    let route = TestingTcpRoute::register_with_kind("pod-2".into(), {
        let constructors = constructors.clone();
        move |kind, _, endpoint| {
            let constructors = constructors.clone();
            Box::pin(async move {
                if kind == Cut::PeerCreating {
                    constructors.fetch_add(1, Ordering::SeqCst);
                    return Err(std::io::Error::other(
                        "synthetic negative: actual Peer constructor cut",
                    ));
                }
                Ok(endpoint)
            })
        }
    })
    .unwrap();
    let (source, target, build, information) = selected_idle_build().await;
    source.submit(&build).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if source
                .snapshot()
                .await
                .goals
                .iter()
                .any(|g| g.goal == build && matches!(g.status, ReplicaGoalStatus::Failed(_)))
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let primary = source.probe.native_primary.lock().unwrap().clone().unwrap();
    assert!(primary.build_replica(information).await.is_err());
    let state = route.inspect_native().unwrap();
    assert_eq!(constructors.load(Ordering::SeqCst), 1);
    assert!(
        state["failure"]
            .as_str()
            .unwrap()
            .contains("actual Peer constructor cut")
    );
    assert!(state["peers"].as_array().unwrap().is_empty());
    assert!(
        !state["sessions"]["entries"][0]["admitted"]
            .as_bool()
            .unwrap()
    );
    source.finish().await;
    finish_faulted(target, "actual Peer constructor cut").await;
}

#[tokio::test]
async fn native_private_effect_actual_ingress_task_failure_keeps_historical_result_and_sticky_join()
{
    let installed = Arc::new(AtomicUsize::new(0));
    let route = TestingTcpRoute::register_with_kind("pod-2".into(), {
        let installed = installed.clone();
        move |kind, _, endpoint| {
            let installed = installed.clone();
            Box::pin(async move {
                if kind == Cut::PeerInstalled {
                    installed.fetch_add(1, Ordering::SeqCst);
                }
                if kind == Cut::BeforeDispatch {
                    return Err(std::io::Error::other(
                        "synthetic negative: actual ingress task cut",
                    ));
                }
                Ok(endpoint)
            })
        }
    })
    .unwrap();
    let (source, target, build, information) = selected_idle_build().await;
    source.submit(&build).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let state = route.inspect_native().unwrap();
            if state["failure"]
                .as_str()
                .is_some_and(|error| error.contains("actual ingress task cut"))
            {
                assert_eq!(installed.load(Ordering::SeqCst), 1);
                assert!(
                    !state["sessions"]["entries"][0]["admitted"]
                        .as_bool()
                        .unwrap()
                );
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let primary = source.probe.native_primary.lock().unwrap().clone().unwrap();
    let observed = tokio::time::timeout(
        Duration::from_millis(100),
        primary.build_replica(information),
    )
    .await;
    assert!(
        !matches!(observed, Ok(Ok(()))),
        "a historical Peer success is not current health or BuildEnd"
    );
    assert!(
        !source
            .snapshot()
            .await
            .remote_operations
            .iter()
            .any(|remote| remote.state == RemoteReplicaState::Ready)
    );
    source.finish().await;
    finish_faulted(target, "actual ingress task cut").await;
}
