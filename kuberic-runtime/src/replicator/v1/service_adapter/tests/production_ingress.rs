use super::super::super::transport::{Carrier, CarrierLimits, ExchangeService, connect};
use super::super::engine_session::{Binding, Endpoint, Sessions};
use super::super::production_transport::Transport;
use super::facade::{config, facade_config, information, until};
use super::*;
use crate::protocol::public_operations::*;
use crate::protocol::types::*;
use crate::replicator::{
    PartitionAccessView, PartitionInformation, PrimaryReplicator, Replicator,
    ReplicatorFactoryContext,
};
use tokio_stream::wrappers::TcpListenerStream;

struct Access {
    publication: Mutex<ReplicaPublication>,
    validated: Semaphore,
    release: Semaphore,
    pause: AtomicBool,
    withdrawn: AtomicBool,
    faults: AtomicUsize,
    writer_releases: AtomicUsize,
    discovery: Mutex<Option<(String, bool, usize)>>,
}
#[async_trait]
impl PartitionAccessView for Access {
    fn peer_control_endpoint(&self, _: &ReplicaIdentity) -> crate::Result<String> {
        let mut configured = self.discovery.lock().unwrap();
        let (address, unavailable, queries) = configured
            .as_mut()
            .ok_or(RuntimeError::AuthorityNotAdmitted)?;
        *queries += 1;
        Ok(if std::mem::take(unavailable) {
            "http://127.0.0.1:0".to_owned()
        } else {
            address.clone()
        })
    }
    async fn native_credentials(&self) -> crate::Result<Arc<str>> {
        Ok(Arc::from("test-token"))
    }
    fn partition_information(&self) -> PartitionInformation {
        PartitionInformation {
            partition_id: PartitionId::new("pump-test"),
        }
    }
    async fn read_status(&self) -> crate::Result<AccessStatus> {
        Ok(AccessStatus::Granted)
    }
    async fn write_status(&self) -> crate::Result<AccessStatus> {
        Ok(AccessStatus::Granted)
    }
    async fn local_publication(
        &self,
        _: crate::replicator::ReplicatorCreationReservation,
    ) -> crate::Result<Option<ReplicaPublication>> {
        self.validated.add_permits(1);
        if self.pause.swap(false, Ordering::SeqCst) {
            self.release.acquire().await.unwrap().forget();
        }
        if self.withdrawn.load(Ordering::SeqCst) {
            return Ok(None);
        }
        Ok(Some(self.publication.lock().unwrap().clone()))
    }
    async fn report_load(&self, _: Vec<LoadMetric>) -> crate::Result<()> {
        Ok(())
    }
    async fn report_fault(&self, _: FaultType) -> crate::Result<()> {
        self.faults.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    fn native_write_admission(&self) -> crate::Result<Arc<Mutex<()>>> {
        Ok(Arc::new(Mutex::new(())))
    }
    fn native_write_status(&self) -> AccessStatus {
        AccessStatus::Granted
    }
}

fn native_endpoint(id: i64) -> Endpoint {
    let info = information(id);
    Endpoint {
        creation_id: ReplicatorCreationId::new(format!("creation-{id}")),
        storage: ReplicaStorageIncarnationContext {
            replica: info.identity.clone(),
            storage: ReplicaStorageIdentity::Persistent {
                pod_uid: PodUid::new(format!("pod-{id}")),
                pvc_uid: PvcUid::new(format!("pvc-{id}")),
            },
            storage_generation: StorageGeneration::new(format!("storage-{id}")),
        },
        identity: info.identity,
        process_session_id: info.process_session_id,
        address: info.replication_address,
    }
}

fn publication(history: &HistoryContext, id: i64) -> ReplicaPublication {
    let endpoint = native_endpoint(id);
    ReplicaPublication {
        format: ORDINARY_ACTION_FORMAT,
        scope: ReplicaScope {
            history: history.clone(),
            storage: endpoint.storage,
            process_session_id: endpoint.process_session_id,
            creation_id: endpoint.creation_id,
            epoch: epoch(1),
            configuration: ReplicaConfigurationScope {
                current: config(1, 1, &[]).configuration,
                previous: None,
                replicas: Vec::new(),
            },
        },
        replication_address: endpoint.address,
        generation: 1,
        stage: ReplicaPublicationStage::AchievedScope,
        role: if id == 1 {
            ReplicaRole::Primary
        } else {
            ReplicaRole::IdleSecondary
        },
    }
}

fn owner(registry: MessageRegistry, pause: bool) -> (Arc<Transport>, Arc<Access>, Binding) {
    let binding = PublicV1LifecycleBinding {
        identity: PublicV1Identity::new(1),
        resource_uid: ResourceUid::new("pump-test"),
        spec_generation: 1,
        state_persistence: StatePersistence::Persisted,
    };
    let initialization =
        derive_history_initialization_id(&binding.resource_uid, PublicProtocolGeneration::new(1));
    let mut history = derive_bootstrap_history(
        &binding.resource_uid,
        PublicProtocolGeneration::new(1),
        &initialization,
    );
    history.data_loss_number = 1;
    let source = publication(&history, 1);
    let target = publication(&history, 2);
    let access = Arc::new(Access {
        publication: Mutex::new(source.clone()),
        validated: Semaphore::new(0),
        release: Semaphore::new(0),
        pause: AtomicBool::new(pause),
        withdrawn: AtomicBool::new(false),
        faults: AtomicUsize::new(0),
        writer_releases: AtomicUsize::new(0),
        discovery: Mutex::new(None),
    });
    let context = ReplicatorFactoryContext::new(
        crate::replicator::RuntimeHostToken::new(),
        native_endpoint(1).identity,
        ProcessSessionId::new("pump-process"),
        access.clone(),
        binding,
        "127.0.0.1:0".parse().unwrap(),
    )
    .for_creation(crate::replicator::ReplicatorCreationReservation::new(
        crate::replicator::RuntimeHostToken::new(),
    ));
    let native = Binding::new(source, target.clone()).unwrap();
    let owner = Transport::test_owner(
        context,
        native_endpoint(1),
        history.clone(),
        vec![target, publication(&history, 3)],
        registry,
    );
    owner.selected_role(epoch(1), ReplicaRole::Primary);
    (owner, access, native)
}

#[tokio::test]
async fn m3_admitted_session_is_exact_not_fresh_build_authority_and_retirement_is_monotonic() {
    let (_, _, fresh) = owner(MessageRegistry::default(), false);
    assert!(!fresh.admitted_reconfiguration());
    let mut source = fresh.source;
    let mut target = fresh.target;
    let configuration = ConfigurationDescriptor::new(
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
        current_progress: 5,
        catch_up_capability: 3,
        is_up: true,
        is_ready: true,
        must_catchup: false,
    };
    source.scope.configuration.current = configuration.clone();
    target.scope.configuration.current = configuration;
    source.scope.configuration.replicas = vec![peer(&target, ReplicaRole::ActiveSecondary)];
    target.scope.configuration.replicas = vec![peer(&source, ReplicaRole::Primary)];
    source.stage = ReplicaPublicationStage::DiscoveryPending;
    source.role = ReplicaRole::None;
    target.role = ReplicaRole::ActiveSecondary;
    let binding = Binding::new(source, target).unwrap();
    assert!(binding.admitted_reconfiguration());
    assert!(
        RemoteReplicaSelection {
            original_member: None,
            source: binding.source.clone(),
            target: binding.target.clone(),
            target_control_address: "http://localhost:1".into(),
            generation: 1,
        }
        .validate()
        .is_err()
    );
    for crossed in [
        "history", "process", "storage", "address", "epoch", "previous",
    ] {
        let mut invalid = binding.clone();
        match crossed {
            "history" => invalid.target.scope.history.history_id = HistoryId::new("other"),
            "process" => invalid.target.scope.process_session_id = ProcessSessionId::new("other"),
            "storage" => {
                invalid.target.scope.storage.storage_generation = StorageGeneration::new("other")
            }
            "address" => invalid.target.replication_address.push_str("other"),
            "epoch" => invalid.target.scope.epoch.configuration_number += 1,
            "previous" => {
                invalid.target.scope.configuration.previous =
                    Some(invalid.target.scope.configuration.current.clone())
            }
            _ => unreachable!(),
        }
        assert!(invalid.validate().is_err(), "{crossed}");
    }
    let sessions = Sessions::default();
    sessions.reserve(&binding).unwrap();
    sessions.activate_target(&binding).unwrap();
    sessions.complete(&binding).unwrap();
    let old_fence = sessions.fence(&binding);
    sessions.retire_all();
    assert!(sessions.retiring_receiver(&binding));
    assert!(old_fence.check().is_err());
    sessions.role_receiver_settled().unwrap();
    assert_eq!(sessions.inbound(), Some(binding.clone()));
    sessions.settled(&binding).unwrap();
    sessions.role_receiver_settled().unwrap();
    assert!(sessions.inbound().is_none());
    assert!(sessions.reserve(&binding).is_err());
    assert!(sessions.active(&binding).is_err());
    assert!(old_fence.check().is_err());
    let mut successor = binding.clone();
    successor.nonce = uuid::Uuid::new_v4().to_string();
    sessions.reserve(&successor).unwrap();
    sessions.activate_target(&successor).unwrap();
    sessions.abort();
    assert!(!sessions.retiring_receiver(&successor));
    assert!(sessions.active(&binding).is_err());
}

struct Pump {
    remote: Carrier,
    _remote_ingress: super::super::super::transport::Ingress,
    local: Carrier,
    stop: tokio::sync::oneshot::Sender<()>,
    server: tokio::task::JoinHandle<Result<(), tonic::transport::Error>>,
    driver: tokio::task::JoinHandle<super::super::super::transport::Terminal>,
}

impl Pump {
    async fn start(owner: &Arc<Transport>, binding: Binding, peer: i64) -> Self {
        owner.sessions.propose(&binding).unwrap();
        owner.sessions.activate_source(&binding).unwrap();
        Self::start_route(owner, binding, route(1, peer)).await
    }

    async fn start_route(owner: &Arc<Transport>, binding: Binding, native_route: Route) -> Self {
        let limits = CarrierLimits {
            outbound_count: 32,
            outbound_bytes: 8192,
            inbound_count: 32,
            inbound_bytes: 8192,
            max_wire_bytes: 4096,
            buffers: BufferLimits {
                max_segments: 1,
                max_payload_bytes: 1024,
            },
        };
        let (local, ingress) = Carrier::new(native_route.clone(), limits).unwrap();
        owner.spawn(
            owner
                .clone()
                .run_ingress(binding, Arc::new(AtomicBool::new(true)), ingress),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = format!("http://{}", listener.local_addr().unwrap());
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let (binding, _) = local.bind().unwrap();
        let server = tokio::spawn(async move {
            tonic::transport::Server::builder()
                .add_service(ExchangeService::new(binding).into_service(4096))
                .serve_with_incoming_shutdown(TcpListenerStream::new(listener), async {
                    let _ = stopped.await;
                })
                .await
        });
        let (remote, remote_ingress) = Carrier::new(native_route.reversed(), limits).unwrap();
        let (binding, _) = remote.bind().unwrap();
        let channel = tonic::transport::Endpoint::from_shared(address)
            .unwrap()
            .connect()
            .await
            .unwrap();
        let driver = connect(binding, channel).await.unwrap();
        let driver = tokio::spawn(driver.run());
        Self {
            remote,
            _remote_ingress: remote_ingress,
            local,
            stop,
            server,
            driver,
        }
    }
    fn send(&self, peer: i64, received: Lsn, applied: Lsn) {
        self.remote
            .try_send(
                route(peer, 1).envelope(wire::envelope::Body::ReplicationAck(
                    wire::ReplicationAck {
                        replication_received: received,
                        replication_applied: applied,
                        copy_received: -1,
                        copy_applied: -1,
                        error: 0,
                    },
                )),
                None,
            )
            .unwrap();
    }
    async fn stop(self) {
        self.remote.shutdown();
        self.local.shutdown();
        let _ = self.stop.send(());
        self.driver.await.unwrap();
        self.server.await.unwrap().unwrap();
    }
}

async fn validated(access: &Access) {
    tokio::time::timeout(Duration::from_secs(5), access.validated.acquire())
        .await
        .unwrap_or_else(|_| {
            panic!(
                "pump stopped; reported faults={}",
                access.faults.load(Ordering::SeqCst)
            )
        })
        .unwrap()
        .forget();
}

#[tokio::test]
async fn production_pump_drops_late_retired_ack_and_continues_live_peer_progress() {
    let node = lone();
    let provider = node.provider.clone();
    provider.copy_gate.forget_permits(100);
    let registry = node.registry.clone();
    let configuration = facade_config(&node, &[(2, false), (3, true)]);
    let source = Facade::start(node.adapter, configuration).unwrap();
    source.open().await.unwrap();
    source
        .change_role(epoch(1), ReplicaRole::Primary)
        .await
        .unwrap();
    source
        .update_current_replica_set_configuration(config(1, 2, &[(3, 0, false)]))
        .await
        .unwrap();
    let build_owner = source.clone();
    let build = tokio::spawn(async move { build_owner.build_replica(information(2)).await });
    until(|| source.service().idle_replica_ids().unwrap().contains(&2)).await;
    let (owner, access, binding) = owner(registry, true);
    let retired = Pump::start(&owner, binding.clone(), 2).await;
    retired.send(2, 0, 0);
    validated(&access).await;
    source.remove_replica(ReplicaId::new(2)).await.unwrap();
    access.release.add_permits(1);
    retired.send(2, 0, 0);
    // The same retained pump must continue after the first retired delivery.
    validated(&access).await;
    assert!(owner.check_health().is_ok());
    assert_eq!(access.faults.load(Ordering::SeqCst), 0);
    let mut live_binding = binding;
    live_binding.nonce = uuid::Uuid::new_v4().to_string();
    live_binding.target = publication(live_binding.history(), 3);
    let live = Pump::start(&owner, live_binding, 3).await;
    let mut write = source
        .service()
        .begin_replicate(Bytes::from_static(b"live"))
        .unwrap();
    live.send(3, write.lsn, write.lsn);
    tokio::time::timeout(Duration::from_secs(5), write.completion.wait())
        .await
        .unwrap()
        .unwrap();
    assert!(owner.check_health().is_ok());
    // A malformed ACK for the still-live peer must retain failure and fault.
    live.send(3, write.lsn + 100, write.lsn + 100);
    until(|| access.faults.load(Ordering::SeqCst) == 1).await;
    assert!(owner.check_health().is_err());
    provider.copy_gate.add_permits(1);
    let build_result = build.await.unwrap();
    assert!(
        build_result.is_err(),
        "Remove must terminate the unestablished Build"
    );
    retired.stop().await;
    live.stop().await;
    owner.abort();
    assert!(
        owner.drain().await.is_err(),
        "real failure must stay latched"
    );
    source.abort();
    source.join().await.unwrap();
}

#[tokio::test]
async fn production_pump_keeps_retained_closed_primary_ack_fatal() {
    let mut node = lone();
    node.adapter.open().await.unwrap();
    node.adapter
        .change_role(epoch(1), Role::Primary)
        .await
        .unwrap();
    // Retain the registered ingress's actual Primary target across native
    // closure, before the role owner refreshes/unregisters that target.
    let primary = node.adapter.owner.primary().unwrap();
    primary.close(Duration::ZERO).unwrap();
    let envelope =
        route(2, 1).envelope(wire::envelope::Body::ReplicationAck(wire::ReplicationAck {
            replication_received: 0,
            replication_applied: 0,
            copy_received: -1,
            copy_applied: -1,
            error: 0,
        }));
    assert!(matches!(
        node.registry.dispatch(&envelope),
        Err(super::super::super::ingress::IngressError::Primary(
            PrimaryError::Closed
        ))
    ));
    let (owner, access, binding) = owner(node.registry.clone(), false);
    let pump = Pump::start(&owner, binding, 2).await;
    pump.remote.try_send(envelope, None).unwrap();
    until(|| access.faults.load(Ordering::SeqCst) == 1).await;
    assert!(owner.check_health().is_err());
    pump.stop().await;
    owner.abort();
    assert!(
        owner.drain().await.is_err(),
        "closed-owner dispatch must stay latched"
    );
    node.adapter.close(Duration::ZERO).await.unwrap();
}

#[tokio::test]
async fn production_worker_errors_and_panics_remain_faults() {
    for failure in 0..3 {
        let (owner, access, _) = owner(MessageRegistry::default(), false);
        owner.spawn(async move {
            assert!(failure != 1, "forced unexpected worker panic");
            if failure == 2 {
                Err(RuntimeError::Closed)
            } else {
                Err(RuntimeError::ControlPlane("forced worker failure".into()))
            }
        });
        until(|| access.faults.load(Ordering::SeqCst) == 1).await;
        assert!(owner.check_health().is_err());
        owner.abort();
        assert!(owner.drain().await.is_err());
    }
}

#[tokio::test]
async fn actual_builtin_failed_drain_never_publishes_positive_containment() {
    for lost in [false, true] {
        let node = lone();
        let configuration = facade_config(&node, &[]);
        let facade = Facade::start(node.adapter, configuration).unwrap();
        facade.open().await.unwrap();
        let (transport, access, _) = owner(node.registry, false);
        let context = ReplicatorFactoryContext::new(
            crate::replicator::RuntimeHostToken::new(),
            transport.local().identity.clone(),
            transport.local().process_session_id.clone(),
            access.clone(),
            PublicV1LifecycleBinding {
                identity: PublicV1Identity::new(1),
                resource_uid: ResourceUid::new("pump-test"),
                spec_generation: 1,
                state_persistence: StatePersistence::Persisted,
            },
            "127.0.0.1:0".parse().unwrap(),
        );
        let control =
            super::super::production::test_control(context, facade.clone(), transport.clone());
        transport.spawn(async move {
            assert!(!lost, "lost actual builtin transport worker");
            Err(RuntimeError::ControlPlane(
                "actual builtin transport failure".into(),
            ))
        });
        until(|| access.faults.load(Ordering::SeqCst) == 1).await;
        control.abort();
        let first = control.close().await.unwrap_err().to_string();
        assert_eq!(control.close().await.unwrap_err().to_string(), first);
        assert_eq!(access.writer_releases.load(Ordering::SeqCst), 0);
        assert_eq!(transport.cardinality(), 0);
        assert_eq!(facade.retained_task_count(), 0);
    }
}

#[tokio::test]
async fn production_pump_keeps_non_ack_unknown_session_fatal() {
    let node = lone();
    let registry = node.registry.clone();
    let configuration = facade_config(&node, &[(2, true)]);
    let source = Facade::start(node.adapter, configuration).unwrap();
    source.open().await.unwrap();
    source
        .change_role(epoch(1), ReplicaRole::Primary)
        .await
        .unwrap();
    source
        .update_current_replica_set_configuration(config(1, 1, &[]))
        .await
        .unwrap();
    let envelope = route(2, 1).envelope(wire::envelope::Body::CopyContextOperation(
        wire::CopyContextOperation {
            metadata: Some(metadata(1)),
            buffers: Some(wire::Buffers {
                segments: vec![Bytes::from_static(b"context")],
            }),
            is_last: false,
        },
    ));
    assert!(matches!(
        registry.dispatch(&envelope),
        Err(super::super::super::ingress::IngressError::Primary(
            PrimaryError::UnknownSession(2)
        ))
    ));
    let (owner, access, binding) = owner(registry, false);
    let pump = Pump::start(&owner, binding, 2).await;
    pump.remote.try_send(envelope, None).unwrap();
    until(|| access.faults.load(Ordering::SeqCst) == 1).await;
    assert!(owner.check_health().is_err());
    pump.stop().await;
    owner.abort();
    assert!(owner.drain().await.is_err());
    source.close().await.unwrap();
    source.join().await.unwrap();
}

#[path = "engine_repairs.rs"]
mod engine_repairs;
