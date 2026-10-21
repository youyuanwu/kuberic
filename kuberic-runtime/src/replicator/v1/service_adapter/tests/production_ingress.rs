use super::super::super::transport::{Carrier, CarrierLimits, ExchangeService, connect};
use super::super::production_transport::Transport;
use super::facade::{config, facade_config, information, until};
use super::*;
use crate::host::native_transport::{NativeBinding, NativeEndpoint};
use crate::protocol::public_operations::*;
use crate::protocol::types::*;
use crate::replicator::{
    PartitionAccessView, PartitionInformation, PrimaryReplicator, PublicInboundBuildAdmission,
    Replicator, ReplicatorFactoryContext,
};
use tokio_stream::wrappers::TcpListenerStream;

struct Access {
    validated: Semaphore,
    release: Semaphore,
    pause: AtomicBool,
    faults: AtomicUsize,
}

#[async_trait]
impl PartitionAccessView for Access {
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
    async fn validate_native_binding(&self, _: &NativeBinding) -> crate::Result<()> {
        // Controlled authority seam: native Remove is deliberately independent
        // of an already successful journal/authentication decision.
        self.validated.add_permits(1);
        if self.pause.swap(false, Ordering::SeqCst) {
            self.release.acquire().await.unwrap().forget();
        }
        Ok(())
    }
    async fn classify_replacement_inbound_build_authority(
        &self,
        _: &PublicInboundBuildAuthority,
    ) -> crate::Result<PublicInboundBuildAdmission> {
        Err(RuntimeError::AuthorityNotAdmitted)
    }
    async fn install_replacement_inbound_build_authority(
        &self,
        _: &PublicInboundBuildAuthority,
    ) -> crate::Result<()> {
        Err(RuntimeError::AuthorityNotAdmitted)
    }
    async fn revoke_replacement_inbound_build_authority(&self) -> crate::Result<()> {
        Ok(())
    }
    fn release_provider_writer(&self) {}
    async fn report_load(&self, _: Vec<LoadMetric>) -> crate::Result<()> {
        Ok(())
    }
    async fn report_fault(&self, _: FaultType) -> crate::Result<()> {
        self.faults.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

fn native_endpoint(id: i64) -> NativeEndpoint {
    let info = information(id);
    NativeEndpoint {
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

fn owner(registry: MessageRegistry, pause: bool) -> (Arc<Transport>, Arc<Access>, NativeBinding) {
    let access = Arc::new(Access {
        validated: Semaphore::new(0),
        release: Semaphore::new(0),
        pause: AtomicBool::new(pause),
        faults: AtomicUsize::new(0),
    });
    let binding = PublicV1LifecycleBinding {
        identity: PublicV1Identity::new(1),
        resource_uid: ResourceUid::new("pump-test"),
        spec_generation: 1,
        state_persistence: StatePersistence::Persisted,
    };
    let initialization =
        derive_history_initialization_id(&binding.resource_uid, PublicProtocolGeneration::new(1));
    let history = derive_bootstrap_history(
        &binding.resource_uid,
        PublicProtocolGeneration::new(1),
        &initialization,
    );
    let context = ReplicatorFactoryContext::new(
        crate::replicator::RuntimeHostToken::new(),
        native_endpoint(1).identity,
        ProcessSessionId::new("pump-process"),
        access.clone(),
        binding,
        "127.0.0.1:0".parse().unwrap(),
    );
    let native = NativeBinding {
        history,
        epoch: epoch(1),
        current_configuration_id: config(1, 1, &[]).configuration.configuration_id,
        previous_configuration_id: None,
        primary: native_endpoint(1),
        secondary: native_endpoint(2),
        build: None,
    };
    (
        Transport::test_owner(context, native_endpoint(1), registry),
        access,
        native,
    )
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
    async fn start(owner: &Arc<Transport>, binding: NativeBinding, peer: i64) -> Self {
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
        let (local, ingress) = Carrier::new(route(1, peer), limits).unwrap();
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
        let (remote, remote_ingress) = Carrier::new(route(peer, 1), limits).unwrap();
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
    live_binding.secondary = native_endpoint(3);
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
