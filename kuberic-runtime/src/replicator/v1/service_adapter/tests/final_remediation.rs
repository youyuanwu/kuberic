use super::*;
use crate::replicator::{
    ReplicaInformation, ReplicaSetConfiguration, ReplicaSetQuorumMode, TestingTcpRoute,
    TestingTcpRouteKind as Cut,
};

struct Gate {
    armed: AtomicBool,
    entered: Semaphore,
    release: Semaphore,
}

impl Gate {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            armed: AtomicBool::new(true),
            entered: Semaphore::new(0),
            release: Semaphore::new(0),
        })
    }

    async fn entered(&self) {
        tokio::time::timeout(Duration::from_secs(5), self.entered.acquire())
            .await
            .unwrap()
            .unwrap()
            .forget();
    }
}

struct Recovery {
    pair: Pair,
    source: ReplicatorInterfaces,
    target: ReplicatorInterfaces,
    fresh: Arc<Transport>,
    fresh_access: Arc<Access>,
    binding: Binding,
    predecessor: Binding,
}

fn peer(publication: &ReplicaPublication, role: ReplicaRole) -> ReplicaPeerDescription {
    ReplicaPeerDescription {
        identity: publication.scope.storage.replica.clone(),
        replication_address: publication.replication_address.clone(),
        process_session_id: publication.scope.process_session_id.clone(),
        storage_incarnation: publication.scope.storage.clone(),
        role,
        current_progress: 0,
        catch_up_capability: 0,
        is_up: true,
        is_ready: true,
        must_catchup: false,
    }
}

async fn copied(target: &ReplicatorInterfaces) {
    let mut stream = target
        .state_replicator()
        .unwrap()
        .copy_operations()
        .unwrap();
    loop {
        let operation = tokio::time::timeout(Duration::from_secs(5), stream.get_operation())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let last = operation.end_of_stream;
        if !last {
            assert_eq!(operation.data, Bytes::from_static(b"snapshot"));
        }
        operation.acknowledge().unwrap();
        if last {
            break;
        }
    }
}

impl Recovery {
    async fn new() -> Self {
        let pair = Pair::new().await;
        let provider = Arc::new(Provider::new());
        let (source, old_target) = pair.engines(provider, Arc::new(Provider::new())).await;
        let mut info = information(2);
        info.replication_address = pair.binding.target.replication_address.clone();
        info.storage_incarnation = Some(pair.binding.target.scope.storage.clone());
        let primary = source.primary_replicator().unwrap();
        let build = tokio::spawn(async move { primary.build_replica(info).await });
        copied(&old_target).await;
        build.await.unwrap().unwrap();
        let predecessor = pair.source.sessions.binding_for(2).unwrap();
        let configuration = ConfigurationDescriptor::new(
            epoch(1),
            pair.source.local().identity.replica_id,
            vec![
                ConfigurationMember {
                    identity: pair.source.local().identity.clone(),
                    role: ReplicaRole::Primary,
                },
                ConfigurationMember {
                    identity: pair.target.local().identity.clone(),
                    role: ReplicaRole::ActiveSecondary,
                },
            ],
            2,
        );
        let original_target = pair.binding.target.clone();
        let mut primary_publication = pair.binding.source.clone();
        primary_publication.scope.configuration = ReplicaConfigurationScope {
            current: configuration.clone(),
            previous: None,
            replicas: vec![peer(&original_target, ReplicaRole::ActiveSecondary)],
        };
        let mut target_publication = original_target;
        target_publication.scope.configuration = ReplicaConfigurationScope {
            current: configuration.clone(),
            previous: None,
            replicas: vec![peer(&primary_publication, ReplicaRole::Primary)],
        };
        target_publication.role = ReplicaRole::ActiveSecondary;
        old_target
            .replicator()
            .change_role(epoch(1), ReplicaRole::ActiveSecondary)
            .await
            .unwrap();
        source
            .primary_replicator()
            .unwrap()
            .update_current_replica_set_configuration(ReplicaSetConfiguration {
                configuration,
                replicas: vec![ReplicaInformation {
                    identity: pair.target.local().identity.clone(),
                    replication_address: pair.target.local().address.clone(),
                    process_session_id: pair.target.local().process_session_id.clone(),
                    storage_incarnation: Some(pair.target.local().storage.clone()),
                    role: ReplicaRole::ActiveSecondary,
                    current_progress: 0,
                    catch_up_capability: 0,
                    must_catchup: false,
                }],
            })
            .await
            .unwrap();
        pair.publish(primary_publication.clone(), target_publication.clone());
        old_target.replicator().abort();
        old_target.replicator().close().await.unwrap();
        pair.target.drain().await.unwrap();
        assert_eq!(pair.target.cardinality(), 0);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        target_publication.replication_address =
            format!("http://{}", listener.local_addr().unwrap());
        target_publication.scope.process_session_id =
            ProcessSessionId::new("fresh-recovery-process");
        target_publication.scope.creation_id = ReplicatorCreationId::new("fresh-recovery-creation");
        target_publication.stage = ReplicaPublicationStage::DiscoveryPending;
        target_publication.role = ReplicaRole::None;
        let fresh_access = Pair::access(target_publication.clone());
        let context = Pair::context(&target_publication, fresh_access.clone());
        let registry = MessageRegistry::default();
        let fresh = Transport::test_owner(
            context.clone(),
            super::super::super::super::engine_session::Endpoint::publication(&target_publication),
            pair.source.history().clone(),
            vec![primary_publication.clone()],
            registry.clone(),
        );
        fresh.test_listen(listener);
        let target = test_compose(context, Arc::new(Provider::new()), registry, fresh.clone())
            .await
            .unwrap();
        target.replicator().open().await.unwrap();
        target
            .replicator()
            .change_role(epoch(1), ReplicaRole::ActiveSecondary)
            .await
            .unwrap();
        pair.source.test_publish(vec![target_publication.clone()]);
        let binding = Binding {
            nonce: uuid::Uuid::new_v4().to_string(),
            source: primary_publication,
            target: target_publication,
            recovery: true,
        };
        binding.validate().unwrap();
        Self {
            pair,
            source,
            target,
            fresh,
            fresh_access,
            binding,
            predecessor,
        }
    }

    fn route(&self, gates: Vec<(Cut, Arc<Gate>)>) -> TestingTcpRoute {
        TestingTcpRoute::register_with_kind(
            self.pair.source.local().identity.instance_id.to_string(),
            move |kind, _, address| {
                let gate = gates
                    .iter()
                    .find(|(cut, _)| kind == *cut)
                    .map(|(_, gate)| gate.clone());
                Box::pin(async move {
                    if let Some(gate) = gate
                        && gate.armed.swap(false, Ordering::SeqCst)
                    {
                        gate.entered.add_permits(1);
                        gate.release.acquire().await.unwrap().forget();
                    }
                    Ok(address)
                })
            },
        )
        .unwrap()
    }

    async fn complete(&self) {
        copied(&self.target).await;
        self.fresh
            .control(&self.binding, Control::Recovered)
            .await
            .unwrap();
        assert!(self.pair.source.sessions.completed(&self.binding));
        assert!(self.fresh.sessions.completed(&self.binding));
        let mut publication = self.binding.target.clone();
        publication.stage = ReplicaPublicationStage::AchievedScope;
        publication.role = ReplicaRole::ActiveSecondary;
        publication.generation += 1;
        *self.fresh_access.publication.lock().unwrap() = publication.clone();
        self.pair.source.test_publish(vec![publication]);
        let endpoint = self.binding.secondary();
        self.source
            .primary_replicator()
            .unwrap()
            .update_current_replica_set_configuration(ReplicaSetConfiguration {
                configuration: self.binding.source.scope.configuration.current.clone(),
                replicas: vec![ReplicaInformation {
                    identity: endpoint.identity,
                    replication_address: endpoint.address,
                    process_session_id: endpoint.process_session_id,
                    storage_incarnation: Some(endpoint.storage),
                    role: ReplicaRole::ActiveSecondary,
                    current_progress: 0,
                    catch_up_capability: 0,
                    must_catchup: false,
                }],
            })
            .await
            .unwrap();
    }

    async fn write(&self) {
        let mut stream = self
            .target
            .state_replicator()
            .unwrap()
            .replication_operations()
            .unwrap();
        let write = self
            .source
            .state_replicator()
            .unwrap()
            .begin_replicate(Bytes::from_static(b"winner-still-replicates"))
            .unwrap();
        let completion = tokio::spawn(write.completion);
        let operation = tokio::time::timeout(Duration::from_secs(5), stream.get_operation())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(
            operation.data,
            Bytes::from_static(b"winner-still-replicates")
        );
        assert_eq!(operation.lsn, write.lsn);
        assert!(
            !completion.is_finished(),
            "quorum requires the winning receiver's ACK"
        );
        operation.acknowledge().unwrap();
        tokio::time::timeout(Duration::from_secs(5), completion)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        self.source
            .primary_replicator()
            .unwrap()
            .wait_for_catch_up_quorum(ReplicaSetQuorumMode::All)
            .await
            .unwrap();
    }

    async fn finish(self) {
        self.source.replicator().abort();
        self.target.replicator().abort();
        self.source.replicator().close().await.unwrap();
        self.target.replicator().close().await.unwrap();
        self.fresh.drain().await.unwrap();
        assert_eq!(self.fresh.cardinality(), 0);
        assert_eq!(
            self.pair.source.test_production().inspect()["recoveryWork"],
            0
        );
        self.pair.finish().await;
    }
}

#[tokio::test]
async fn m2_recover_overlapping_rpcs_keep_winner_peer_carrier_ack_and_quorum_progress() {
    let recovery = Recovery::new().await;
    let received = Gate::new();
    let discovery = Gate::new();
    let _route = recovery.route(vec![
        (Cut::RecoveryReceived, received.clone()),
        (Cut::RecoveryDiscovery, discovery.clone()),
    ]);
    let mut loser = recovery.binding.clone();
    loser.nonce = uuid::Uuid::new_v4().to_string();
    let target = recovery.fresh.clone();
    let losing_request =
        tokio::spawn(async move { target.control(&loser, Control::Recover).await });
    received.entered().await;
    recovery
        .fresh
        .control(&recovery.binding, Control::Recover)
        .await
        .unwrap();
    discovery.entered().await;
    let retained = recovery.pair.source.test_production().inspect();
    recovery
        .fresh
        .control(&recovery.binding, Control::Recover)
        .await
        .unwrap();
    let mut conflict = recovery.binding.clone();
    conflict.nonce = uuid::Uuid::new_v4().to_string();
    assert!(
        recovery
            .fresh
            .control(&conflict, Control::Recover)
            .await
            .is_err()
    );
    assert_eq!(recovery.pair.source.test_production().inspect(), retained);
    discovery.release.add_permits(1);
    recovery.complete().await;
    recovery.write().await;
    let peers = recovery.pair.source.inspect_peers();
    let target_peers = recovery.fresh.inspect_peers();
    let source_acks = recovery.pair.source.inspect_acks();
    let target_acks = recovery.fresh.inspect_acks();
    let source_sessions = recovery.pair.source.sessions.inspect();
    let target_sessions = recovery.fresh.sessions.inspect();
    let native_peers = recovery.pair.source.test_production().inspect()["peers"].clone();
    received.release.add_permits(1);
    assert!(losing_request.await.unwrap().is_err());
    // Exercise the original stale join against the actual winning carrier too.
    assert!(
        recovery
            .pair
            .source
            .join_predecessor(&recovery.predecessor)
            .await
            .is_err()
    );
    assert_eq!(recovery.pair.source.inspect_peers(), peers);
    assert_eq!(recovery.fresh.inspect_peers(), target_peers);
    assert_eq!(recovery.pair.source.inspect_acks(), source_acks);
    assert_eq!(recovery.fresh.inspect_acks(), target_acks);
    assert_eq!(recovery.pair.source.sessions.inspect(), source_sessions);
    assert_eq!(recovery.fresh.sessions.inspect(), target_sessions);
    assert_eq!(
        recovery.pair.source.test_production().inspect()["peers"],
        native_peers
    );
    assert_eq!(
        recovery.pair.source.test_production().inspect()["recoverySpawned"],
        1
    );
    recovery.write().await;
    assert_eq!(recovery.pair.source.inspect_peers(), peers);
    assert_eq!(recovery.pair.source.inspect_acks(), source_acks);
    assert_eq!(recovery.fresh.inspect_acks(), target_acks);
    recovery.finish().await;
}

#[tokio::test]
async fn m2_recover_post_discovery_generation_fences_retired_work_and_joins() {
    let recovery = Recovery::new().await;
    let discovery = Gate::new();
    let _route = recovery.route(vec![(Cut::RecoveryDiscovery, discovery.clone())]);
    recovery
        .fresh
        .control(&recovery.binding, Control::Recover)
        .await
        .unwrap();
    discovery.entered().await;
    let predecessor = recovery.pair.source.inspect_peers();
    recovery
        .pair
        .source
        .sessions
        .retire(&recovery.binding)
        .unwrap();
    discovery.release.add_permits(1);
    until(|| recovery.pair.source.test_production().inspect()["recoveryWork"] == 0).await;
    assert!(
        recovery
            .fresh
            .control(&recovery.binding, Control::Recovered)
            .await
            .is_err()
    );
    assert_eq!(recovery.pair.source.inspect_peers(), predecessor);
    assert!(
        recovery
            .pair
            .source
            .sessions
            .completed(&recovery.predecessor)
    );
    assert!(recovery.fresh.sessions.inbound().is_none());
    recovery.finish().await;
}

#[tokio::test]
async fn m2_recover_rpc_budget_bounds_results_and_work_with_invalid_retired_duplicates() {
    let recovery = Recovery::new().await;
    let mut invalid_scope = recovery.binding.clone();
    invalid_scope.source.generation += 1;
    recovery
        .fresh
        .control(&invalid_scope, Control::Recover)
        .await
        .unwrap();
    until(|| recovery.pair.source.test_production().inspect()["recoveryWork"] == 0).await;
    let failure = recovery
        .fresh
        .control(&invalid_scope, Control::Recovered)
        .await
        .unwrap_err()
        .to_string();
    assert!(!failure.is_empty());
    recovery
        .pair
        .source
        .retire_remote(&invalid_scope)
        .await
        .unwrap();
    let before = recovery.pair.source.test_production().inspect();
    assert_eq!(before["retainedResults"], 2);
    assert_eq!(before["recoverySpawned"], 1);
    assert_eq!(before["recoveryWork"], 0);
    assert_eq!(
        recovery.pair.source.sessions.binding_for(2),
        Some(recovery.predecessor.clone())
    );
    // Fill the same creation's budget through its real authenticated Retire RPC.
    let mut tombstone = recovery.predecessor.clone();
    tombstone.target = tombstone.source.clone();
    tombstone.target.role = ReplicaRole::IdleSecondary;
    tombstone.source = recovery.predecessor.target.clone();
    tombstone.source.role = ReplicaRole::Primary;
    let current = ConfigurationDescriptor::new(
        epoch(1),
        tombstone.source.scope.storage.replica.replica_id,
        vec![ConfigurationMember {
            identity: tombstone.source.scope.storage.replica.clone(),
            role: ReplicaRole::Primary,
        }],
        1,
    );
    for publication in [&mut tombstone.source, &mut tombstone.target] {
        publication.scope.configuration = ReplicaConfigurationScope {
            current: current.clone(),
            previous: None,
            replicas: Vec::new(),
        };
    }
    let context = Pair::context(&tombstone.source, Pair::access(tombstone.source.clone()));
    let sender = Transport::test_owner(
        context,
        tombstone.primary(),
        recovery.pair.source.history().clone(),
        Vec::new(),
        MessageRegistry::default(),
    );
    let capacity = recovery.pair.source.sessions.inspect()["capacity"]
        .as_u64()
        .unwrap() as usize;
    while recovery.pair.source.sessions.inspect()["entries"]
        .as_array()
        .unwrap()
        .len()
        < capacity
    {
        tombstone.nonce = uuid::Uuid::new_v4().to_string();
        tombstone.validate().unwrap();
        sender.control(&tombstone, Control::Retire).await.unwrap();
    }
    let sessions = recovery.pair.source.sessions.inspect();
    let peers = recovery.pair.source.inspect_peers();
    let mut requests = Vec::new();
    for index in 0..capacity {
        let target = recovery.fresh.clone();
        let mut binding = if index % 3 == 0 {
            invalid_scope.clone()
        } else {
            recovery.binding.clone()
        };
        if index % 3 != 0 {
            binding.nonce = uuid::Uuid::new_v4().to_string();
            if index % 3 == 1 {
                binding.source.generation += 1;
            }
        }
        requests.push(tokio::spawn(async move {
            assert!(target.control(&binding, Control::Recover).await.is_err());
        }));
    }
    for request in requests {
        request.await.unwrap();
    }
    assert_eq!(recovery.pair.source.test_production().inspect(), before);
    assert_eq!(recovery.pair.source.sessions.inspect(), sessions);
    assert_eq!(recovery.pair.source.inspect_peers(), peers);
    assert!(
        recovery
            .pair
            .source
            .sessions
            .completed(&recovery.predecessor)
    );
    assert_eq!(
        recovery
            .fresh
            .control(&invalid_scope, Control::Recovered)
            .await
            .unwrap_err()
            .to_string(),
        failure
    );
    sender.drain().await.unwrap();
    recovery.finish().await;
}
