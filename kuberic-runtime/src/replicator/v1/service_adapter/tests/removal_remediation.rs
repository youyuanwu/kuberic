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
