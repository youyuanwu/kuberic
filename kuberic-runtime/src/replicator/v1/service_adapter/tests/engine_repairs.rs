use super::super::super::engine_session::Control;
use super::super::super::production::{test_compose, test_provider};
use super::*;
use crate::replicator::ReplicatorInterfaces;

#[cfg(feature = "testing")]
#[path = "final_remediation.rs"]
mod final_remediation;

struct Pair {
    source: Arc<Transport>,
    target: Arc<Transport>,
    source_access: Arc<Access>,
    target_access: Arc<Access>,
    source_context: ReplicatorFactoryContext,
    target_context: ReplicatorFactoryContext,
    source_registry: MessageRegistry,
    target_registry: MessageRegistry,
    binding: Binding,
}

impl Pair {
    async fn new() -> Self {
        let (seed, _, initial) = owner(MessageRegistry::default(), false);
        let source_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut source = initial.source;
        let mut target = initial.target;
        source.replication_address = format!("http://{}", source_listener.local_addr().unwrap());
        target.replication_address = format!("http://{}", target_listener.local_addr().unwrap());
        let binding = Binding::new(source.clone(), target.clone()).unwrap();
        let source_access = Self::access(source.clone());
        let target_access = Self::access(target.clone());
        let source_context = Self::context(&source, source_access.clone());
        let target_context = Self::context(&target, target_access.clone());
        let source_registry = MessageRegistry::default();
        let target_registry = MessageRegistry::default();
        let source = Transport::test_owner(
            source_context.clone(),
            binding.primary(),
            seed.history().clone(),
            vec![target.clone()],
            source_registry.clone(),
        );
        let target = Transport::test_owner(
            target_context.clone(),
            binding.secondary(),
            seed.history().clone(),
            vec![binding.source.clone()],
            target_registry.clone(),
        );
        source.selected_role(epoch(1), ReplicaRole::Primary);
        target.selected_role(epoch(1), ReplicaRole::IdleSecondary);
        source.test_listen(source_listener);
        target.test_listen(target_listener);
        Self {
            source,
            target,
            source_access,
            target_access,
            source_context,
            target_context,
            source_registry,
            target_registry,
            binding,
        }
    }
}

#[tokio::test]
async fn m2_recovery_session_rejects_stale_identity_epoch_and_successor_before_effects() {
    let mut pair = Pair::new().await;
    let mut source = pair.binding.source.clone();
    let mut target = pair.binding.target.clone();
    let current = ConfigurationDescriptor::new(
        source.scope.epoch,
        source.scope.storage.replica.replica_id,
        vec![
            ConfigurationMember {
                identity: source.scope.storage.replica.clone(),
                role: ReplicaRole::Primary,
            },
            ConfigurationMember {
                identity: target.scope.storage.replica.clone(),
                role: ReplicaRole::ActiveSecondary,
            },
        ],
        2,
    );
    let peer = |publication: &ReplicaPublication, role| ReplicaPeerDescription {
        identity: publication.scope.storage.replica.clone(),
        replication_address: publication.replication_address.clone(),
        process_session_id: publication.scope.process_session_id.clone(),
        storage_incarnation: publication.scope.storage.clone(),
        role,
        current_progress: -1,
        catch_up_capability: -1,
        is_up: true,
        is_ready: true,
        must_catchup: false,
    };
    let mut predecessor = peer(&target, ReplicaRole::ActiveSecondary);
    predecessor.process_session_id = ProcessSessionId::new("contained-predecessor");
    source.scope.configuration = ReplicaConfigurationScope {
        current: current.clone(),
        previous: None,
        replicas: vec![predecessor],
    };
    target.scope.configuration = ReplicaConfigurationScope {
        current,
        previous: None,
        replicas: vec![peer(&source, ReplicaRole::Primary)],
    };
    target.stage = ReplicaPublicationStage::DiscoveryPending;
    target.role = ReplicaRole::None;
    pair.binding = Binding {
        nonce: uuid::Uuid::new_v4().to_string(),
        source: source.clone(),
        target: target.clone(),
        recovery: true,
    };
    pair.publish(source, target);
    pair.target
        .selected_role(pair.binding.epoch(), ReplicaRole::ActiveSecondary);
    pair.establish().await;
    pair.target
        .control(&pair.binding, Control::Confirm)
        .await
        .unwrap();
    pair.source.sessions.complete(&pair.binding).unwrap();
    let before = pair.source.sessions.inspect();
    for fault in 0..7 {
        let mut stale = pair.binding.clone();
        match fault {
            0 => {
                stale.target.scope.process_session_id =
                    ProcessSessionId::new("contained-predecessor")
            }
            1 => stale.target.scope.creation_id = ReplicatorCreationId::new("old-creation"),
            2 => {
                stale.target.scope.storage.storage_generation =
                    StorageGeneration::new("other-storage")
            }
            3 => {
                stale.target.scope.history.history_id =
                    crate::protocol::types::HistoryId::new("other-history")
            }
            4 => stale.target.scope.epoch.configuration_number += 1,
            5 => stale.target.replication_address.push_str("stale-endpoint"),
            _ => stale.nonce = uuid::Uuid::new_v4().to_string(),
        }
        assert!(pair.target.control(&stale, Control::Confirm).await.is_err());
        assert_eq!(pair.source.sessions.inspect(), before);
    }
    let sender = pair.source.acknowledgements();
    let permit = sender.reserve_owned().await.unwrap();
    let (credited, observed) = tokio::sync::oneshot::channel();
    pair.source_access.validated.forget_permits(usize::MAX);
    pair.source_access.pause.store(true, Ordering::SeqCst);
    pair.source
        .sessions
        .effect(&pair.binding, || {
            pair.source.queue_ack(
                pair.binding.clone(),
                permit,
                Box::new(move || {
                    let _ = credited.send(());
                    Ok(())
                }),
            )
        })
        .unwrap()
        .unwrap();
    validated(&pair.source_access).await;
    pair.source.sessions.retire(&pair.binding).unwrap();
    assert!(pair.source.sessions.predecessor_completed(&pair.binding));
    assert!(!pair.source.sessions.completed(&pair.binding));
    pair.source_access.release.add_permits(1);
    assert!(
        observed.await.is_err(),
        "retired ACK must not credit its native callback"
    );
    assert!(pair.target.confirm_effect(&pair.binding).await.is_err());
    let mut successor = pair.binding.clone();
    successor.nonce = uuid::Uuid::new_v4().to_string();
    assert!(pair.target.sessions.reserve(&successor).is_err());
    assert!(
        pair.target
            .sessions
            .effect(&pair.binding, || panic!("late ACK/effect"))
            .is_err()
    );
    assert!(!pair.target.sessions.completed(&pair.binding));
    pair.finish().await;
}

impl Pair {
    fn access(publication: ReplicaPublication) -> Arc<Access> {
        Arc::new(Access {
            publication: Mutex::new(publication),
            validated: Semaphore::new(0),
            release: Semaphore::new(0),
            pause: AtomicBool::new(false),
            withdrawn: AtomicBool::new(false),
            faults: AtomicUsize::new(0),
            writer_releases: AtomicUsize::new(0),
        })
    }

    fn context(publication: &ReplicaPublication, access: Arc<Access>) -> ReplicatorFactoryContext {
        ReplicatorFactoryContext::new(
            crate::replicator::RuntimeHostToken::new(),
            publication.scope.storage.replica.clone(),
            publication.scope.process_session_id.clone(),
            access,
            PublicV1LifecycleBinding {
                identity: PublicV1Identity::new(1),
                resource_uid: publication.scope.history.resource_uid.clone(),
                spec_generation: 1,
                state_persistence: StatePersistence::Persisted,
            },
            "127.0.0.1:0".parse().unwrap(),
        )
        .for_creation(crate::replicator::ReplicatorCreationReservation::new(
            crate::replicator::RuntimeHostToken::new(),
        ))
    }

    fn publish(&self, source: ReplicaPublication, target: ReplicaPublication) {
        *self.source_access.publication.lock().unwrap() = source.clone();
        *self.target_access.publication.lock().unwrap() = target.clone();
        self.source.test_publish(vec![target]);
        self.target.test_publish(vec![source]);
    }

    async fn establish(&self) {
        self.source.sessions.propose(&self.binding).unwrap();
        self.source.establish(&self.binding).await.unwrap();
        self.target.confirm_effect(&self.binding).await.unwrap();
    }

    async fn engines(
        &self,
        source: Arc<dyn StateProvider>,
        target: Arc<dyn StateProvider>,
    ) -> (ReplicatorInterfaces, ReplicatorInterfaces) {
        self.engines_at(source, target, epoch(1)).await
    }

    async fn engines_at(
        &self,
        source: Arc<dyn StateProvider>,
        target: Arc<dyn StateProvider>,
        target_epoch: Epoch,
    ) -> (ReplicatorInterfaces, ReplicatorInterfaces) {
        let source = test_compose(
            self.source_context.clone(),
            source,
            self.source_registry.clone(),
            self.source.clone(),
        )
        .await
        .unwrap();
        let target = test_compose(
            self.target_context.clone(),
            target,
            self.target_registry.clone(),
            self.target.clone(),
        )
        .await
        .unwrap();
        source.replicator().open().await.unwrap();
        target.replicator().open().await.unwrap();
        source
            .replicator()
            .change_role(epoch(1), ReplicaRole::Primary)
            .await
            .unwrap();
        target
            .replicator()
            .change_role(target_epoch, ReplicaRole::IdleSecondary)
            .await
            .unwrap();
        (source, target)
    }

    async fn finish(self) {
        self.source.abort();
        self.target.abort();
        self.source.drain().await.unwrap();
        self.target.drain().await.unwrap();
        assert_eq!(self.source.cardinality(), 0);
        assert_eq!(self.target.cardinality(), 0);
    }
}

#[tokio::test]
async fn repair_final_completion_requires_both_achieved_but_active_pending_continues() {
    let pair = Pair::new().await;
    pair.establish().await;
    for source_pending in [true, false] {
        let mut source = pair.binding.source.clone();
        let mut target = pair.binding.target.clone();
        let pending = if source_pending {
            &mut source
        } else {
            &mut target
        };
        pending.generation += 1;
        pending.stage = ReplicaPublicationStage::DiscoveryPending;
        pending.role = ReplicaRole::None;
        pair.publish(source, target);
        pair.target.confirm_effect(&pair.binding).await.unwrap();
        assert!(
            pair.source
                .authorize_completion(&pair.binding)
                .await
                .is_err()
        );
        assert!(
            pair.source
                .control(&pair.binding, Control::Complete)
                .await
                .is_err()
        );
        assert!(pair.source.sessions.active(&pair.binding).is_ok());
        assert!(pair.target.sessions.active(&pair.binding).is_ok());
    }
    pair.publish(pair.binding.source.clone(), pair.binding.target.clone());
    pair.source
        .authorize_completion(&pair.binding)
        .await
        .unwrap();
    pair.source
        .control(&pair.binding, Control::Complete)
        .await
        .unwrap();
    pair.source.sessions.complete(&pair.binding).unwrap();
    pair.target.confirm_effect(&pair.binding).await.unwrap();
    pair.finish().await;
}

#[tokio::test]
async fn repair_failed_confirmation_quarantines_without_revival_or_receiver_reuse() {
    for fault in 0..4 {
        let pair = Pair::new().await;
        pair.establish().await;
        let mut incompatible = pair.binding.source.clone();
        match fault {
            0 => incompatible.scope.creation_id = ReplicatorCreationId::new("successor"),
            1 => incompatible.role = ReplicaRole::ActiveSecondary,
            2 => {
                incompatible.scope.configuration = ReplicaConfigurationScope {
                    current: config(1, 2, &[(3, 0, false)]).configuration,
                    previous: None,
                    replicas: Vec::new(),
                }
            }
            _ => pair.source_access.withdrawn.store(true, Ordering::SeqCst),
        }
        pair.publish(incompatible, pair.binding.target.clone());
        assert!(pair.target.confirm_effect(&pair.binding).await.is_err());
        pair.source_access.withdrawn.store(false, Ordering::SeqCst);
        pair.publish(pair.binding.source.clone(), pair.binding.target.clone());
        assert!(pair.target.confirm_effect(&pair.binding).await.is_err());
        let mut next = pair.binding.clone();
        next.nonce = uuid::Uuid::new_v4().to_string();
        assert!(pair.target.sessions.reserve(&next).is_err());
        assert_eq!(pair.target.sessions.inbound(), Some(pair.binding.clone()));
        assert!(pair.target.sessions.active(&pair.binding).is_err());
        pair.finish().await;
    }
}

#[tokio::test]
async fn repair_target_context_acquisition_reconfirms_after_await_and_quarantines() {
    let pair = Pair::new().await;
    pair.establish().await;
    let provider = Arc::new(Provider::new());
    provider.context_gate.forget_permits(100);
    let wrapped = test_provider(provider.clone(), pair.target.clone());
    let (result, observer) = tokio::sync::oneshot::channel();
    pair.target.spawn(async move {
        let outcome = wrapped.get_copy_context().await;
        let _ = result.send(outcome.is_err());
        Ok(())
    });
    until(|| provider.context_calls.load(Ordering::SeqCst) == 1).await;
    pair.source.sessions.retire(&pair.binding).unwrap();
    assert_eq!(
        pair.source.selected_lifecycle(),
        Some((epoch(1), ReplicaRole::Primary))
    );
    // No Retire is delivered: the target must discover retirement at the remote cut.
    assert!(pair.target.sessions.active(&pair.binding).is_ok());
    provider.context_gate.add_permits(1);
    assert!(observer.await.unwrap());
    assert_eq!(provider.context_resumed.load(Ordering::SeqCst), 1);
    assert!(pair.target.sessions.active(&pair.binding).is_err());
    pair.finish().await;
}

#[tokio::test]
async fn repair_target_stream_poll_reconfirms_and_cannot_revive_after_source_retirement() {
    let pair = Pair::new().await;
    pair.establish().await;
    let entered = Arc::new(Semaphore::new(0));
    let release = Arc::new(Semaphore::new(0));
    let polls = Arc::new(AtomicUsize::new(0));
    let stream_entered = entered.clone();
    let stream_release = release.clone();
    let stream_polls = polls.clone();
    let provider_stream = futures::stream::once(async move {
        stream_polls.fetch_add(1, Ordering::SeqCst);
        stream_entered.add_permits(1);
        stream_release.acquire().await.unwrap().forget();
        Ok(Bytes::from_static(b"context"))
    });
    let mut stream = pair
        .target
        .confirmed_stream(pair.binding.clone(), Box::pin(provider_stream));
    let (result, observer) = tokio::sync::oneshot::channel();
    pair.target.spawn(async move {
        let item = stream.next().await.unwrap();
        assert!(stream.next().await.is_none());
        let _ = result.send(item.is_err());
        Ok(())
    });
    entered.acquire().await.unwrap().forget();
    pair.source.sessions.retire(&pair.binding).unwrap();
    release.add_permits(1);
    assert!(observer.await.unwrap());
    assert_eq!(polls.load(Ordering::SeqCst), 1);
    assert!(pair.target.sessions.active(&pair.binding).is_err());
    assert!(
        pair.source
            .control(&pair.binding, Control::Reserve)
            .await
            .is_err()
    );
    pair.finish().await;
}

#[tokio::test]
async fn repair_epoch_transition_fences_before_provider_await_but_keeps_creation_open() {
    let pair = Pair::new().await;
    let provider = Arc::new(Provider::new());
    let (source, target) = pair
        .engines(provider.clone(), Arc::new(Provider::new()))
        .await;
    pair.establish().await;
    provider.epoch_gate.forget_permits(100);
    let control = source.replicator();
    let change = tokio::spawn(async move { control.update_epoch(epoch(2)).await });
    until(|| {
        provider
            .epochs
            .lock()
            .unwrap()
            .iter()
            .any(|(selected, _)| *selected == epoch(2))
    })
    .await;
    assert!(pair.source.sessions.active(&pair.binding).is_err());
    assert!(pair.source.sessions.activate_source(&pair.binding).is_err());
    assert!(pair.source.authorize(&pair.binding).await.is_err());
    provider.epoch_gate.add_permits(1);
    change.await.unwrap().unwrap();
    let mut next_source = pair.binding.source.clone();
    let mut next_target = pair.binding.target.clone();
    for publication in [&mut next_source, &mut next_target] {
        publication.generation += 1;
        publication.scope.epoch = epoch(2);
        let current = &publication.scope.configuration.current;
        publication.scope.configuration.current = ConfigurationDescriptor::new(
            epoch(2),
            current.primary_id,
            current.members.clone(),
            current.write_quorum,
        );
    }
    // Callback settlement is not settlement of the old session or target receiver.
    let next = Binding::new(next_source.clone(), next_target.clone()).unwrap();
    assert!(pair.source.sessions.proposal(next.clone()).is_err());
    assert!(pair.target.sessions.reserve(&next).is_err());
    assert!(pair.source.sessions.activate_source(&pair.binding).is_err());
    source.replicator().abort();
    target.replicator().abort();
    source.replicator().close().await.unwrap();
    target.replicator().close().await.unwrap();
    pair.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repair_ingress_and_build_have_no_primary_session_lock_inversion() {
    let mut node = lone();
    node.adapter.open().await.unwrap();
    node.adapter
        .change_role(epoch(1), Role::Primary)
        .await
        .unwrap();
    let primary = node.adapter.owner.primary().unwrap();
    let (transport, access, binding) = owner(node.registry.clone(), false);
    transport.sessions.propose(&binding).unwrap();
    transport.sessions.activate_source(&binding).unwrap();
    let (config, mut callbacks) = node.build_parts(2, true);
    callbacks.fence = Some(transport.fence(&binding));
    let primary_locked = std::sync::Barrier::new(2);
    let session_locked = std::sync::Barrier::new(2);
    let envelope = route(2, 1).envelope(wire::envelope::Body::CopyContextOperation(
        wire::CopyContextOperation {
            metadata: Some(metadata(1)),
            buffers: Some(wire::Buffers {
                segments: vec![Bytes::from_static(b"context")],
            }),
            is_last: false,
        },
    ));
    std::thread::scope(|scope| {
        let build = scope.spawn(|| {
            primary.begin_build_with_cut(config, node.provider.clone(), callbacks, || {
                primary_locked.wait();
                session_locked.wait();
            })
        });
        let ingress = scope.spawn(|| {
            primary_locked.wait();
            transport.sessions.effect(&binding, || {
                session_locked.wait();
                node.registry.dispatch(&envelope)
            })
        });
        let build = build.join().unwrap().unwrap();
        assert_eq!(
            ingress.join().unwrap().unwrap().unwrap(),
            Disposition::Processed
        );
        build.cancel().unwrap();
    });
    let configuration = facade_config(&node, &[]);
    let facade = Facade::start(node.adapter, configuration).unwrap();
    let control = super::super::super::production::test_control(
        Pair::context(&binding.source, access),
        facade.clone(),
        transport.clone(),
    );
    control.abort();
    control.close().await.unwrap();
    control.close().await.unwrap();
    assert_eq!(facade.retained_task_count(), 0);
    assert_eq!(transport.cardinality(), 0);
}

#[tokio::test]
async fn repair_epoch_advance_allows_actual_new_build_in_the_same_creation() {
    let pair = Pair::new().await;
    let (source, target) = pair
        .engines_at(
            Arc::new(Provider::new()),
            Arc::new(Provider::new()),
            epoch(2),
        )
        .await;
    pair.source.sessions.propose(&pair.binding).unwrap();
    source.replicator().update_epoch(epoch(2)).await.unwrap();
    let mut source_publication = pair.binding.source.clone();
    let mut target_publication = pair.binding.target.clone();
    for publication in [&mut source_publication, &mut target_publication] {
        publication.generation += 1;
        publication.scope.epoch = epoch(2);
        let current = &publication.scope.configuration.current;
        publication.scope.configuration.current = ConfigurationDescriptor::new(
            epoch(2),
            current.primary_id,
            current.members.clone(),
            current.write_quorum,
        );
    }
    pair.publish(source_publication.clone(), target_publication.clone());
    assert!(
        pair.source
            .control(&pair.binding, Control::Reserve)
            .await
            .is_err()
    );
    assert!(pair.source.sessions.activate_source(&pair.binding).is_err());
    pair.source.sessions.settled(&pair.binding).unwrap();
    let mut info = information(2);
    info.replication_address = target_publication.replication_address.clone();
    info.storage_incarnation = Some(target_publication.scope.storage.clone());
    let mut admitted = info.clone();
    let primary = source.primary_replicator().unwrap();
    let build = tokio::spawn(async move { primary.build_replica(info).await });
    let mut operations = target
        .state_replicator()
        .unwrap()
        .copy_operations()
        .unwrap();
    loop {
        let operation = tokio::time::timeout(Duration::from_secs(5), operations.get_operation())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let last = operation.end_of_stream;
        operation.acknowledge().unwrap();
        if last {
            break;
        }
    }
    tokio::time::timeout(Duration::from_secs(5), build)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(pair.source.sessions.active(&pair.binding).is_err());
    assert_eq!(
        pair.source.local().creation_id,
        source_publication.scope.creation_id
    );
    admitted.role = ReplicaRole::ActiveSecondary;
    admitted.current_progress = 0;
    admitted.catch_up_capability = 0;
    let mut members = source_publication
        .scope
        .configuration
        .current
        .members
        .clone();
    members.push(ConfigurationMember {
        identity: admitted.identity.clone(),
        role: ReplicaRole::ActiveSecondary,
    });
    let configuration = ConfigurationDescriptor::new(
        epoch(2),
        pair.source.local().identity.replica_id,
        members,
        2,
    );
    target
        .replicator()
        .change_role(epoch(2), ReplicaRole::ActiveSecondary)
        .await
        .unwrap();
    source
        .primary_replicator()
        .unwrap()
        .update_current_replica_set_configuration(crate::replicator::ReplicaSetConfiguration {
            configuration: configuration.clone(),
            replicas: vec![admitted.clone()],
        })
        .await
        .unwrap();
    let scope = ReplicaConfigurationScope {
        current: configuration,
        previous: None,
        replicas: vec![ReplicaPeerDescription {
            identity: admitted.identity,
            replication_address: admitted.replication_address,
            process_session_id: admitted.process_session_id,
            storage_incarnation: admitted.storage_incarnation.unwrap(),
            role: admitted.role,
            current_progress: admitted.current_progress,
            catch_up_capability: admitted.catch_up_capability,
            is_up: true,
            is_ready: true,
            must_catchup: admitted.must_catchup,
        }],
    };
    source_publication.scope.configuration = scope.clone();
    target_publication.scope.configuration = scope;
    target_publication.scope.configuration.replicas = vec![ReplicaPeerDescription {
        identity: pair.binding.source.scope.storage.replica.clone(),
        replication_address: pair.binding.source.replication_address.clone(),
        process_session_id: pair.binding.source.scope.process_session_id.clone(),
        storage_incarnation: pair.binding.source.scope.storage.clone(),
        role: ReplicaRole::Primary,
        current_progress: 0,
        catch_up_capability: 0,
        is_up: true,
        is_ready: true,
        must_catchup: false,
    }];
    target_publication.role = ReplicaRole::ActiveSecondary;
    source_publication.stage = ReplicaPublicationStage::DiscoveryPending;
    source_publication.role = ReplicaRole::None;
    pair.publish(source_publication, target_publication);
    let live = pair.target.sessions.inbound().unwrap();
    pair.target.confirm_effect(&live).await.unwrap();
    assert!(pair.source.authorize_completion(&live).await.is_err());
    source.replicator().abort();
    target.replicator().abort();
    source.replicator().close().await.unwrap();
    target.replicator().close().await.unwrap();
    pair.finish().await;
}

#[tokio::test]
async fn repair_handed_out_operation_cannot_ack_after_source_retirement() {
    let pair = Pair::new().await;
    let (source, target) = pair
        .engines(Arc::new(Provider::new()), Arc::new(Provider::new()))
        .await;
    pair.establish().await;
    let mut info = information(2);
    info.replication_address = pair.binding.target.replication_address.clone();
    info.storage_incarnation = Some(pair.binding.target.scope.storage.clone());
    let primary = source.primary_replicator().unwrap();
    let build = tokio::spawn(async move { primary.build_replica(info).await });
    let mut operations = target
        .state_replicator()
        .unwrap()
        .copy_operations()
        .unwrap();
    let operation = tokio::time::timeout(Duration::from_secs(5), operations.get_operation())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(operation.data, Bytes::from_static(b"snapshot"));
    pair.source.sessions.retire(&pair.binding).unwrap();
    assert_eq!(
        pair.source.selected_lifecycle(),
        Some((epoch(1), ReplicaRole::Primary))
    );
    operation.acknowledge().unwrap();
    until(|| pair.target.sessions.active(&pair.binding).is_err()).await;
    assert!(operation.acknowledge().is_err());
    assert!(
        !build.is_finished(),
        "the unacknowledged native boundary cannot complete"
    );
    assert!(
        pair.target
            .sessions
            .reserve(
                &Binding::new(pair.binding.source.clone(), pair.binding.target.clone(),).unwrap()
            )
            .is_err()
    );
    source.replicator().abort();
    target.replicator().abort();
    assert!(build.await.unwrap().is_err());
    source.replicator().close().await.unwrap();
    // Failed confirmation is a retained transport fault, not positive containment.
    assert!(target.replicator().close().await.is_err());
    pair.source.abort();
    pair.target.abort();
    pair.source.drain().await.unwrap();
    assert!(pair.target.drain().await.is_err());
    assert_eq!(pair.target.cardinality(), 0);
}

#[tokio::test]
async fn repair_issued_context_provider_is_joined_not_dropped_when_target_aborts() {
    let pair = Pair::new().await;
    pair.establish().await;
    let provider = Arc::new(Provider::new());
    provider.context_gate.forget_permits(100);
    let wrapped = test_provider(provider.clone(), pair.target.clone());
    let (result, observer) = tokio::sync::oneshot::channel();
    pair.target.spawn(async move {
        let _ = result.send(wrapped.get_copy_context().await.is_err());
        Ok(())
    });
    until(|| provider.context_calls.load(Ordering::SeqCst) == 1).await;
    let target = pair.target.clone();
    let close = tokio::spawn(async move { target.drain().await });
    until(|| pair.target.sessions.active(&pair.binding).is_err()).await;
    assert!(!close.is_finished());
    assert_eq!(provider.context_resumed.load(Ordering::SeqCst), 0);
    provider.context_gate.add_permits(1);
    assert!(observer.await.unwrap());
    close.await.unwrap().unwrap();
    assert_eq!(provider.context_resumed.load(Ordering::SeqCst), 1);
    pair.finish().await;
}
