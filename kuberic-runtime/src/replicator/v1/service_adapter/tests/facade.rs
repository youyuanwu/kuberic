// Copyright (c) Microsoft Corporation. All rights reserved.
// Licensed under the MIT License. See LICENSES/service-fabric-MIT.txt.
//
// Actual-owner protected-facade lifetime vectors; no production host selection.

use super::super::facade::{Facade, FacadeConfig, FacadeError, PeerBinding};
use super::*;
use crate::protocol::types::{
    AgentGeneration, ConfigurationDescriptor, ConfigurationMember, ProcessSessionId, ReplicaId,
    ReplicaIdentity, ReplicaInstanceId, ReplicaRole,
};
use crate::replicator::{
    PrimaryReplicator, ReplicaInformation, ReplicaSetConfiguration, ReplicaSetQuorumMode,
    Replicator,
};
use std::future::Future;

#[test]
fn selected_completion_cancellation_and_sender_failure_remain_distinct() {
    use super::super::super::replica_manager::CompletionError;
    assert!(matches!(
        RuntimeError::from(FacadeError::Adapter(AdapterError::Completion(
            CompletionError::OperationCanceled,
        ))),
        RuntimeError::OperationCancelled,
    ));
    assert!(matches!(
        RuntimeError::from(FacadeError::Adapter(AdapterError::Completion(
            CompletionError::ObjectClosed,
        ))),
        RuntimeError::Closed,
    ));
    assert!(matches!(
        RuntimeError::from(FacadeError::Adapter(AdapterError::CompletionDropped)),
        RuntimeError::ControlPlane(_),
    ));
}

fn identity(id: i64) -> ReplicaIdentity {
    ReplicaIdentity {
        replica_id: ReplicaId::new(id),
        instance_id: ReplicaInstanceId::new(format!("instance-{id}")),
        agent_generation: AgentGeneration::new(format!("agent-{id}")),
    }
}

pub(super) fn information(id: i64) -> ReplicaInformation {
    let mut info = ReplicaInformation::idle_build(identity(id), format!("replica-{id}"));
    info.process_session_id = ProcessSessionId::new(format!("process-{id}"));
    info
}

pub(super) fn config(
    from: i64,
    quorum: u32,
    peers: &[(i64, Lsn, bool)],
) -> ReplicaSetConfiguration {
    let members = std::iter::once(ConfigurationMember {
        identity: identity(from),
        role: ReplicaRole::Primary,
    })
    .chain(peers.iter().map(|&(id, _, _)| ConfigurationMember {
        identity: identity(id),
        role: ReplicaRole::ActiveSecondary,
    }))
    .collect();
    ReplicaSetConfiguration {
        configuration: ConfigurationDescriptor::new(
            epoch(1),
            ReplicaId::new(from),
            members,
            quorum,
        ),
        replicas: peers
            .iter()
            .map(|&(id, progress, must_catchup)| {
                let mut info = information(id);
                info.role = ReplicaRole::ActiveSecondary;
                info.current_progress = progress;
                info.must_catchup = must_catchup;
                info
            })
            .collect(),
    }
}

pub(super) fn facade_config<P: StateProvider + 'static>(
    node: &Node<P>,
    peers: &[(i64, bool)],
) -> FacadeConfig {
    let bindings = peers
        .iter()
        .map(|&(id, persisted)| {
            let (build, callbacks) = node.build_parts(id, persisted);
            PeerBinding {
                information: information(id),
                conservative_progress: false,
                fresh_checkpoint: None,
                build,
                callbacks,
            }
        })
        .collect();
    FacadeConfig {
        local: identity(node.id),
        peers: bindings,
        resolve_peer: None,
        close_quorum_wait: Duration::from_secs(5),
    }
}

fn start(node: Node, peers: &[(i64, bool)]) -> Arc<Facade> {
    let config = facade_config(&node, peers);
    Facade::start(node.adapter, config).unwrap()
}

pub(super) async fn until(mut ready: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while !ready() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("actual owner reached the requested boundary");
}

pub(super) async fn deliver<T>(
    bus: &Bus,
    registry: &MessageRegistry,
    future: impl Future<Output = T>,
) -> T {
    tokio::pin!(future);
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            bus.deliver(registry);
            tokio::select! {
                biased;
                result = &mut future => return result,
                () = tokio::task::yield_now() => {}
            }
        }
    })
    .await
    .expect("actual owner completed with delivered messages")
}

async fn primary(facade: &Arc<Facade>) {
    let control: Arc<dyn Replicator> = facade.clone();
    let primary: Arc<dyn PrimaryReplicator> = facade.clone();
    assert_eq!(control.open().await.unwrap(), "replica-1");
    control
        .change_role(epoch(1), ReplicaRole::Primary)
        .await
        .unwrap();
    primary
        .update_current_replica_set_configuration(config(1, 1, &[]))
        .await
        .unwrap();
}

#[tokio::test]
async fn dropped_opened_query_retains_one_callback_while_capability_and_busy_calls_complete() {
    let node = lone();
    let provider = node.provider.clone();
    provider.progress_gate.forget_permits(100);
    provider.progress.store(-17, Ordering::SeqCst);
    let facade = start(node, &[]);
    let control: Arc<dyn Replicator> = facade.clone();
    assert!(control.current_progress().await.is_err());
    assert_eq!(control.open().await.unwrap(), "replica-1");
    {
        let query = control.current_progress();
        tokio::pin!(query);
        assert!(query.as_mut().now_or_never().is_none());
        until(|| provider.progress_calls.load(Ordering::SeqCst) == 1).await;
        assert_eq!(control.catch_up_capability().await.unwrap(), 0);
        assert!(matches!(
            control.close().await,
            Err(RuntimeError::ReconfigurationPending)
        ));
    }
    provider.progress_gate.add_permits(1);
    assert_eq!(control.current_progress().await.unwrap(), 0);
    assert_eq!(provider.progress_calls.load(Ordering::SeqCst), 1);
    assert!(control.open().await.is_err());
    control.close().await.unwrap();
    facade.join().await.unwrap();
    facade.join().await.unwrap();
    assert!(control.current_progress().await.is_err());
}

#[tokio::test]
async fn dropped_role_epoch_and_data_loss_calls_keep_provider_work_and_native_admission() {
    let node = lone();
    let provider = node.provider.clone();
    provider.epoch_gate.forget_permits(100);
    let facade = start(node, &[]);
    let control: Arc<dyn Replicator> = facade.clone();
    let primary: Arc<dyn PrimaryReplicator> = facade.clone();
    control.open().await.unwrap();
    {
        let change = control.change_role(epoch(1), ReplicaRole::Primary);
        tokio::pin!(change);
        assert!(change.as_mut().now_or_never().is_none());
        until(|| provider.epochs.lock().unwrap().len() == 1).await;
        assert!(matches!(
            control.change_role(epoch(2), ReplicaRole::Primary).await,
            Err(RuntimeError::ReconfigurationPending)
        ));
        assert!(control.current_progress().await.is_err());
    }
    provider.epoch_gate.add_permits(1);
    // Owner progress is selected before the queued query after gate release.
    assert_eq!(control.current_progress().await.unwrap(), 0);
    primary
        .update_current_replica_set_configuration(config(1, 1, &[]))
        .await
        .unwrap();
    {
        let update = control.update_epoch(epoch(2));
        tokio::pin!(update);
        assert!(update.as_mut().now_or_never().is_none());
        until(|| provider.epochs.lock().unwrap().len() == 2).await;
        assert_eq!(control.current_progress().await.unwrap(), 0);
        assert_eq!(control.catch_up_capability().await.unwrap(), 0);
        assert!(matches!(
            control.close().await,
            Err(RuntimeError::ReconfigurationPending)
        ));
    }
    provider.epoch_gate.add_permits(1);
    assert_eq!(control.current_progress().await.unwrap(), 0);
    provider.loss_gate.forget_permits(100);
    provider.changed.store(true, Ordering::SeqCst);
    provider.progress.store(7, Ordering::SeqCst);
    {
        let loss = primary.on_data_loss();
        tokio::pin!(loss);
        assert!(loss.as_mut().now_or_never().is_none());
        until(|| provider.loss_calls.load(Ordering::SeqCst) == 1).await;
        assert!(primary.on_data_loss().await.is_err());
        assert!(control.current_progress().await.is_err());
    }
    provider.loss_gate.add_permits(1);
    assert_eq!(control.current_progress().await.unwrap(), 7);
    assert_eq!(provider.loss_calls.load(Ordering::SeqCst), 1);
    provider.changed.store(false, Ordering::SeqCst);
    provider.loss_gate.add_permits(1);
    assert!(!primary.on_data_loss().await.unwrap());
    assert_eq!(
        provider.epochs.lock().unwrap().as_slice(),
        &[(epoch(1), 0), (epoch(2), 0)]
    );
    control.close().await.unwrap();
    facade.join().await.unwrap();
}

#[tokio::test]
async fn trait_build_copy_configuration_and_catchup_progress_without_caller_driving() {
    let bus = Bus::default();
    let registry = MessageRegistry::default();
    let source_node = Node::new(1, true, false, bus.clone(), registry.clone());
    let provider = source_node.provider.clone();
    let source = start(source_node, &[(2, true)]);
    let target = start(
        Node::new(2, true, false, bus.clone(), registry.clone()),
        &[],
    );
    primary(&source).await;
    target.open().await.unwrap();
    target
        .change_role(epoch(1), ReplicaRole::IdleSecondary)
        .await
        .unwrap();
    let mut copy = target.service().copy_stream().unwrap();
    let mut stream = target.service().replication_stream().unwrap();
    {
        let build = source.build_replica(information(2));
        tokio::pin!(build);
        assert!(build.as_mut().now_or_never().is_none());
        let snapshot = deliver(&bus, &registry, take(&mut copy)).await;
        assert_eq!(snapshot.data().as_ref(), b"snapshot");
        assert!(build.as_mut().now_or_never().is_none());
        snapshot.acknowledge().unwrap();
        deliver(&bus, &registry, build.as_mut()).await.unwrap();
    }
    assert_eq!(provider.copy_calls.load(Ordering::SeqCst), 1);
    source
        .update_catch_up_replica_set_configuration(config(1, 2, &[(2, 0, true)]), config(1, 1, &[]))
        .await
        .unwrap();
    target
        .change_role(epoch(1), ReplicaRole::ActiveSecondary)
        .await
        .unwrap();
    let mut write = source
        .service()
        .begin_replicate(Bytes::from_static(b"one"))
        .unwrap();
    assert_eq!(write.lsn, 1);
    let operation = {
        let catchup = source.wait_for_catch_up_quorum(ReplicaSetQuorumMode::All);
        tokio::pin!(catchup);
        assert!(catchup.as_mut().now_or_never().is_none());
        let operation = deliver(&bus, &registry, take(&mut stream)).await;
        assert_eq!(source.current_progress().await.unwrap(), 0);
        assert!(write.completion.wait().now_or_never().is_none());
        // Dropping the caller leaves the actual catch-up request driver-rooted.
        operation
    };
    operation.acknowledge().unwrap();
    deliver(&bus, &registry, write.completion.wait())
        .await
        .unwrap();
    source
        .wait_for_catch_up_quorum(ReplicaSetQuorumMode::WriteQuorum)
        .await
        .unwrap();
    source
        .update_current_replica_set_configuration(config(1, 2, &[(2, 1, false)]))
        .await
        .unwrap();
    assert_eq!(source.current_progress().await.unwrap(), 1);
    assert_eq!(target.current_progress().await.unwrap(), 1);
    assert_eq!(provider.progress.load(Ordering::SeqCst), 0);
    let mut pending = source
        .service()
        .begin_replicate(Bytes::from_static(b"two"))
        .unwrap();
    {
        let close = source.close();
        tokio::pin!(close);
        assert!(close.as_mut().now_or_never().is_none());
        let operation = deliver(&bus, &registry, take(&mut stream)).await;
        assert!(source.current_progress().await.is_err());
        assert!(source.service().begin_replicate(Bytes::new()).is_err());
        operation.acknowledge().unwrap();
        // Abandon Close's waiter; the actual owner must still finish Close.
    }
    deliver(&bus, &registry, pending.completion.wait())
        .await
        .unwrap();
    source.join().await.unwrap();
    target.close().await.unwrap();
    target.join().await.unwrap();
}

#[tokio::test]
async fn close_finishes_before_held_build_provider_but_join_and_cancelled_build_stay_rooted() {
    let bus = Bus::default();
    let registry = MessageRegistry::default();
    let node = Node::new(1, true, false, bus.clone(), registry.clone());
    let provider = node.provider.clone();
    provider.copy_gate.forget_permits(100);
    let source = start(node, &[(2, true)]);
    let target = start(
        Node::new(2, true, false, bus.clone(), registry.clone()),
        &[],
    );
    primary(&source).await;
    target.open().await.unwrap();
    target
        .change_role(epoch(1), ReplicaRole::IdleSecondary)
        .await
        .unwrap();
    {
        let build = source.build_replica(information(2));
        tokio::pin!(build);
        assert!(build.as_mut().now_or_never().is_none());
        deliver(
            &bus,
            &registry,
            until(|| provider.copy_calls.load(Ordering::SeqCst) == 1),
        )
        .await;
        // Waiter drop is not native Build cancellation.
    }
    source.close().await.unwrap();
    assert!(source.join().now_or_never().is_none());
    assert!(source.current_progress().await.is_err());
    provider.copy_gate.add_permits(1);
    source.join().await.unwrap();
    assert_eq!(provider.copy_calls.load(Ordering::SeqCst), 1);
    target.close().await.unwrap();
    target.join().await.unwrap();
}

#[tokio::test]
async fn abort_is_synchronous_for_service_admission_and_retains_provider_callback_until_join() {
    let node = lone();
    let provider = node.provider.clone();
    let registry = node.registry.clone();
    let source = start(node, &[]);
    primary(&source).await;
    let service = source.service();
    provider.epoch_gate.forget_permits(99);
    let update = source.update_epoch(epoch(2));
    tokio::pin!(update);
    assert!(update.as_mut().now_or_never().is_none());
    until(|| provider.epochs.lock().unwrap().len() == 2).await;
    source.abort();
    source.abort();
    assert!(service.begin_replicate(Bytes::new()).is_err());
    assert!(source.current_progress().await.is_err());
    assert!(update.await.is_err());
    assert!(source.join().now_or_never().is_none());
    provider.epoch_gate.add_permits(1);
    source.join().await.unwrap();
    assert_eq!(provider.epochs.lock().unwrap().len(), 2);
    let probe = route(2, 1).envelope(wire::envelope::Body::RequestAck(wire::RequestAck {}));
    assert_eq!(
        registry.dispatch(&probe).unwrap(),
        Disposition::Dropped(DropReason::Unregistered)
    );
}

#[tokio::test(start_paused = true)]
async fn retained_driver_retries_actual_sender_without_any_control_caller() {
    let bus = Bus::default();
    let node = Node::new(1, true, false, bus.clone(), MessageRegistry::default());
    let source = start(node, &[(2, true)]);
    primary(&source).await;
    source
        .update_current_replica_set_configuration(config(1, 2, &[(2, 0, false)]))
        .await
        .unwrap();
    let mut write = source
        .service()
        .begin_replicate(Bytes::from_static(b"retry"))
        .unwrap();
    let first = bus.0.lock().unwrap().pop_front().unwrap();
    assert!(matches!(
        first.body,
        Some(wire::envelope::Body::ReplicationOperation(_))
    ));
    until(|| bus.0.lock().unwrap().is_empty()).await;
    tokio::time::advance(Duration::from_secs(1)).await;
    until(|| !bus.0.lock().unwrap().is_empty()).await;
    assert!(bus.0.lock().unwrap().iter().any(|message| matches!(
        &message.body, Some(wire::envelope::Body::ReplicationOperation(operation))
        if operation.metadata.as_ref().unwrap().sequence == write.lsn
    )));
    source.abort();
    assert!(write.completion.wait().await.is_err());
    source.join().await.unwrap();
}

#[tokio::test]
async fn exact_binding_and_configuration_failures_do_not_start_providers_or_mutate_owners() {
    let node = lone();
    let provider = node.provider.clone();
    let source = start(node, &[(2, true)]);
    primary(&source).await;
    for alter in [0, 1, 2, 3, 4] {
        let mut info = information(2);
        match alter {
            0 => info.identity.instance_id = ReplicaInstanceId::new("replacement"),
            1 => info.process_session_id = ProcessSessionId::new("replacement"),
            2 => info.replication_address = "wrong-address".into(),
            3 => info.current_progress = 0,
            _ => info.role = ReplicaRole::ActiveSecondary,
        }

        assert!(source.build_replica(info).await.is_err());
    }
    assert_eq!(provider.copy_calls.load(Ordering::SeqCst), 0);
    let mut malformed = config(1, 2, &[(2, 0, false)]);
    malformed.replicas.clear();
    assert!(
        source
            .update_current_replica_set_configuration(malformed)
            .await
            .is_err()
    );
    let malformed = config(1, 2, &[(2, 0, true)]);
    assert!(
        source
            .update_current_replica_set_configuration(malformed)
            .await
            .is_err()
    );
    let mut previous = config(1, 2, &[(2, 0, false)]);
    previous.configuration.primary_id = ReplicaId::new(2);
    previous.configuration.epoch = epoch(0);
    for member in &mut previous.configuration.members {
        member.role = if member.identity.replica_id == ReplicaId::new(2) {
            ReplicaRole::Primary
        } else {
            ReplicaRole::ActiveSecondary
        };
    }
    previous.configuration.configuration_id = previous.configuration.expected_id();
    previous.replicas[0].role = ReplicaRole::Primary;
    assert!(
        source
            .update_current_replica_set_configuration(previous.clone())
            .await
            .is_err()
    );
    source
        .update_catch_up_replica_set_configuration(config(1, 2, &[(2, 0, false)]), previous)
        .await
        .unwrap();
    source
        .update_current_replica_set_configuration(config(1, 1, &[]))
        .await
        .unwrap();
    let mut write = source.service().begin_replicate(Bytes::new()).unwrap();
    assert_eq!(write.lsn, 1);
    write.completion.wait().await.unwrap();
    assert!(source.remove_replica(ReplicaId::new(2)).await.is_err());
    source.close().await.unwrap();
    source.join().await.unwrap();
}

#[tokio::test]
async fn historical_pc_can_exclude_new_primary_but_still_requires_remote_apply() {
    let node = lone();
    let registry = node.registry.clone();
    let source = start(node, &[(2, true)]);
    primary(&source).await;
    let current = config(1, 1, &[]);
    let mut old_primary = information(2);
    old_primary.role = ReplicaRole::Primary;
    old_primary.current_progress = 0;
    let previous = ReplicaSetConfiguration {
        configuration: ConfigurationDescriptor::new(
            epoch(0),
            ReplicaId::new(2),
            vec![ConfigurationMember {
                identity: identity(2),
                role: ReplicaRole::Primary,
            }],
            1,
        ),
        replicas: vec![old_primary],
    };
    source
        .update_catch_up_replica_set_configuration(current.clone(), previous.clone())
        .await
        .unwrap();
    let mut write = source
        .service()
        .begin_replicate(Bytes::from_static(b"PC"))
        .unwrap();
    assert!(write.completion.wait().now_or_never().is_none());
    for (applied, completed) in [(0, false), (write.lsn, true)] {
        registry
            .dispatch(&route(2, 1).envelope(wire::envelope::Body::ReplicationAck(
                wire::ReplicationAck {
                    replication_received: write.lsn,
                    replication_applied: applied,
                    copy_received: -1,
                    copy_applied: -1,
                    error: 0,
                },
            )))
            .unwrap();
        if completed {
            write.completion.wait().await.unwrap();
        } else {
            assert!(write.completion.wait().now_or_never().is_none());
        }
    }
    for malformed in 0..6 {
        let mut pc = previous.clone();
        match malformed {
            0 => pc.replicas.clear(),
            1 => pc.replicas[0].process_session_id = ProcessSessionId::new("wrong"),
            2 => pc.configuration.epoch = Epoch::new(-1, 0),
            3 => pc.configuration.write_quorum = 2,
            4 => pc.replicas.push(pc.replicas[0].clone()),
            _ => pc.replicas[0].must_catchup = true,
        }
        if malformed == 2 || malformed == 3 {
            pc.configuration.configuration_id = pc.configuration.expected_id();
        }
        assert!(
            source
                .update_catch_up_replica_set_configuration(current.clone(), pc)
                .await
                .is_err(),
            "historical validation control {malformed}"
        );
    }
    // A valid historical PC is not a valid CC on this local/new primary.
    assert!(
        source
            .update_current_replica_set_configuration(previous)
            .await
            .is_err()
    );
    // Native public current W1 still forbids remote CC members.
    assert!(
        source
            .update_current_replica_set_configuration(config(1, 1, &[(2, 0, false)]))
            .await
            .is_err()
    );
    source
        .update_current_replica_set_configuration(current)
        .await
        .unwrap();
    source.close().await.unwrap();
    source.join().await.unwrap();
}

#[tokio::test]
async fn remove_idle_ends_the_actual_build_without_dropping_its_trait_waiter() {
    let node = lone();
    let provider = node.provider.clone();
    let source = start(node, &[(2, true)]);
    primary(&source).await;
    let build = source.build_replica(information(2));
    tokio::pin!(build);
    assert!(build.as_mut().now_or_never().is_none());
    // A following query observes that the actor selected BeginBuild.
    assert_eq!(source.current_progress().await.unwrap(), 0);
    source.remove_replica(ReplicaId::new(2)).await.unwrap();
    assert!(build.await.is_err());
    assert_eq!(provider.copy_calls.load(Ordering::SeqCst), 0);
    source.close().await.unwrap();
    source.join().await.unwrap();
}

#[tokio::test]
async fn provider_errors_and_driver_abandonment_are_explicit_not_success() {
    let node = lone();
    let provider = node.provider.clone();
    provider.fail_progress.store(true, Ordering::SeqCst);
    let source = start(node, &[]);
    source.open().await.unwrap();
    assert!(
        source
            .current_progress()
            .await
            .unwrap_err()
            .to_string()
            .contains("provider progress")
    );
    provider.fail_progress.store(false, Ordering::SeqCst);
    assert_eq!(source.current_progress().await.unwrap(), 0);
    source.close().await.unwrap();
    source.join().await.unwrap();
    let node = lone();
    let provider = node.provider.clone();
    provider.panic_progress.store(true, Ordering::SeqCst);
    let source = start(node, &[]);
    source.open().await.unwrap();
    assert!(source.current_progress().await.is_err());
    assert!(matches!(source.join().await, Err(FacadeError::Join(_))));
    assert!(matches!(source.join().await, Err(FacadeError::Join(_))));
    assert!(source.current_progress().await.is_err());
    assert!(source.service().begin_replicate(Bytes::new()).is_err());
}

#[tokio::test]
async fn provider_can_reenter_abort_without_deadlock_or_late_control_success() {
    let node = lone();
    let provider = node.provider.clone();
    let source = start(node, &[]);
    primary(&source).await;
    let weak = Arc::downgrade(&source);
    *provider.epoch_callback.lock().unwrap() = Some(Arc::new(move || {
        weak.upgrade().unwrap().abort();
    }));
    assert!(source.update_epoch(epoch(2)).await.is_err());
    source.join().await.unwrap();
    assert!(source.service().begin_replicate(Bytes::new()).is_err());
}

#[tokio::test]
async fn retired_build_provider_progresses_after_demotion_without_close_or_explicit_drain() {
    let bus = Bus::default();
    let registry = MessageRegistry::default();
    let node = Node::new(1, true, false, bus.clone(), registry.clone());
    let provider = node.provider.clone();
    provider.copy_gate.forget_permits(100);
    let source = start(node, &[(2, true)]);
    let target = start(
        Node::new(2, true, false, bus.clone(), registry.clone()),
        &[],
    );
    primary(&source).await;
    target.open().await.unwrap();
    target
        .change_role(epoch(1), ReplicaRole::IdleSecondary)
        .await
        .unwrap();
    let build = source.build_replica(information(2));
    tokio::pin!(build);
    assert!(build.as_mut().now_or_never().is_none());
    deliver(
        &bus,
        &registry,
        until(|| provider.copy_calls.load(Ordering::SeqCst) == 1),
    )
    .await;
    source
        .change_role(epoch(2), ReplicaRole::ActiveSecondary)
        .await
        .unwrap();
    assert!(build.as_mut().now_or_never().is_none());
    provider.copy_gate.add_permits(1);
    assert!(build.await.is_err());
    until(|| provider.copy_resumed.load(Ordering::SeqCst) == 1).await;
    assert_eq!(provider.copy_resumed.load(Ordering::SeqCst), 1);
    assert_eq!(source.current_progress().await.unwrap(), 0);
    source.close().await.unwrap();
    source.join().await.unwrap();
    target.close().await.unwrap();
    target.join().await.unwrap();
}

#[tokio::test]
async fn secondary_stream_provider_barrier_and_eos_close_are_driven_independently() {
    let node = Node::new(2, true, true, Bus::default(), MessageRegistry::default());
    let provider = node.provider.clone();
    let registry = node.registry.clone();
    provider.epoch_gate.forget_permits(100);
    let target = start(node, &[]);
    target.open().await.unwrap();
    target
        .change_role(epoch(1), ReplicaRole::IdleSecondary)
        .await
        .unwrap();
    let mut copy = target.service().copy_stream().unwrap();
    let mut stream = target.service().replication_stream().unwrap();
    let start = wire::envelope::Body::StartCopy(wire::StartCopy {
        epoch: Some(wire_epoch(epoch(1))),
        replica: 2,
        replication_start: 1,
    });
    let end = wire::envelope::Body::CopyOperation(wire::CopyOperation {
        metadata: Some(metadata(1)),
        epoch: Some(wire_epoch(epoch(1))),
        replica: 2,
        buffers: Some(wire::Buffers { segments: vec![] }),
        is_last: true,
    });
    for body in [start, end] {
        assert_eq!(
            registry.dispatch(&route(1, 2).envelope(body)).unwrap(),
            Disposition::Processed
        );
    }
    let copy_end = take(&mut copy).await;
    assert!(copy_end.is_end_of_stream());
    copy_end.acknowledge().unwrap();
    let message = route(1, 2).envelope(replication(
        1,
        epoch(2),
        epoch(2),
        Bytes::from_static(b"barrier"),
        0,
    ));
    assert_eq!(registry.dispatch(&message).unwrap(), Disposition::Processed);
    assert!(stream.next().now_or_never().is_none());
    until(|| provider.epochs.lock().unwrap().len() == 1).await;
    assert!(stream.next().now_or_never().is_none());
    provider.epoch_gate.add_permits(1);
    let operation = take(&mut stream).await;
    assert_eq!(operation.data().as_ref(), b"barrier");
    operation.acknowledge().unwrap();
    let close = target.close();
    tokio::pin!(close);
    assert!(close.as_mut().now_or_never().is_none());
    let end = take(&mut stream).await;
    assert!(end.is_end_of_stream());
    assert!(close.as_mut().now_or_never().is_none());
    end.acknowledge().unwrap();
    close.await.unwrap();
    target.join().await.unwrap();
    assert_eq!(provider.epochs.lock().unwrap().as_slice(), &[(epoch(2), 0)]);
}

#[tokio::test]
async fn legacy_secondary_close_and_retirement_retain_real_context_acquisition_and_errors() {
    for (retire, fail) in [(false, false), (true, false), (false, true)] {
        let node = Node::new(2, true, false, Bus::default(), MessageRegistry::default());
        let provider = node.provider.clone();
        let registry = node.registry.clone();
        provider.context_gate.forget_permits(100);
        provider.fail_context.store(fail, Ordering::SeqCst);
        let target = start(node, &[]);
        target.open().await.unwrap();
        target
            .change_role(epoch(1), ReplicaRole::IdleSecondary)
            .await
            .unwrap();
        let message = route(1, 2).envelope(wire::envelope::Body::StartCopy(wire::StartCopy {
            epoch: Some(wire_epoch(epoch(1))),
            replica: 2,
            replication_start: 1,
        }));
        assert_eq!(registry.dispatch(&message).unwrap(), Disposition::Processed);
        until(|| provider.context_calls.load(Ordering::SeqCst) == 1).await;
        if retire {
            target
                .change_role(epoch(2), ReplicaRole::None)
                .await
                .unwrap();
            provider.context_gate.add_permits(1);
            until(|| provider.context_resumed.load(Ordering::SeqCst) == 1).await;
            target.close().await.unwrap();
        } else {
            target.close().await.unwrap();
            assert!(target.join().now_or_never().is_none());
            provider.context_gate.add_permits(1);
        }
        let result = target.join().await;
        if fail {
            assert!(result.unwrap_err().to_string().contains("GetCopyContext"));
        } else {
            result.unwrap();
        }
        assert_eq!(provider.context_calls.load(Ordering::SeqCst), 1);
        assert_eq!(provider.context_resumed.load(Ordering::SeqCst), 1);
    }
}
