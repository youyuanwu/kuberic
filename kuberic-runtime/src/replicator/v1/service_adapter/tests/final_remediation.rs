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
            continuation: None,
            reformation: false,
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

struct DiscoveryPeer {
    publication: Arc<Mutex<ReplicaPublication>>,
    rejection: Arc<Mutex<Option<tonic::Code>>>,
}

#[tonic::async_trait]
impl crate::control::proto::replica_peer_server::ReplicaPeer for DiscoveryPeer {
    async fn get_status(
        &self,
        _: tonic::Request<crate::control::proto::GetAgentStatusRequest>,
    ) -> std::result::Result<tonic::Response<crate::control::proto::AgentStatusReport>, tonic::Status>
    {
        Err(tonic::Status::unimplemented("discovery fixture"))
    }
    async fn execute(
        &self,
        _: tonic::Request<crate::control::proto::ExecuteCommandRequest>,
    ) -> std::result::Result<
        tonic::Response<crate::control::proto::ExecuteCommandResponse>,
        tonic::Status,
    > {
        Err(tonic::Status::unimplemented("discovery fixture"))
    }
    async fn discover(
        &self,
        request: tonic::Request<crate::control::proto::ReplicaDiscoveryRequest>,
    ) -> std::result::Result<
        tonic::Response<crate::control::proto::ReplicaSnapshotResponse>,
        tonic::Status,
    > {
        if request
            .metadata()
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            != Some("Bearer test-token")
        {
            return Err(tonic::Status::permission_denied(
                "exact test endpoint credentials required",
            ));
        }
        if let Some(code) = *self.rejection.lock().unwrap() {
            return Err(tonic::Status::new(code, "positive discovery rejection"));
        }
        Ok(tonic::Response::new(
            crate::control::proto::ReplicaSnapshotResponse {
                protocol_version: ORDINARY_ACTION_FORMAT,
                canonical_json: serde_json::to_vec(&Some(self.publication.lock().unwrap().clone()))
                    .unwrap(),
            },
        ))
    }
}

#[tokio::test]
async fn m3_typed_discovery_outage_retries_real_recovery_but_crossed_publication_does_not() {
    let recovery = Recovery::new().await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = format!("http://{}", listener.local_addr().unwrap());
    let publication = Arc::new(Mutex::new(recovery.binding.source.clone()));
    let service = DiscoveryPeer {
        publication: publication.clone(),
        rejection: Arc::new(Mutex::new(None)),
    };
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(
                crate::control::proto::replica_peer_server::ReplicaPeerServer::new(service),
            )
            .serve_with_incoming_shutdown(TcpListenerStream::new(listener), async {
                let _ = stopped.await;
            })
            .await
    });
    recovery.fresh.test_publish(Vec::new());
    *recovery.fresh_access.discovery.lock().unwrap() = Some((address, true, 0));
    recovery
        .fresh
        .control(&recovery.binding, Control::Recover)
        .await
        .unwrap();
    recovery.complete().await;
    recovery.write().await;
    assert!(
        recovery
            .fresh_access
            .discovery
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .2
            >= 2,
        "real dial unavailability must precede compatible discovery and recovery"
    );
    publication.lock().unwrap().scope.process_session_id =
        ProcessSessionId::new("crossed-source-process");
    assert!(matches!(
        tokio::time::timeout(
            Duration::from_secs(5),
            recovery.fresh.recovery_peer(
                &recovery.binding.primary().identity,
                &recovery.binding.primary().process_session_id,
                recovery.binding.history(),
            )
        )
        .await
        .unwrap(),
        Err(RuntimeError::AuthorityNotAdmitted)
    ));
    *publication.lock().unwrap() = recovery.binding.source.clone();
    recovery.finish().await;
    stop.send(()).unwrap();
    server.await.unwrap().unwrap();
}

#[tokio::test]
async fn m3_positive_discovery_rpc_rejection_returns_without_outage_retry() {
    let recovery = Recovery::new().await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = format!("http://{}", listener.local_addr().unwrap());
    let rejection = Arc::new(Mutex::new(None));
    let service = DiscoveryPeer {
        publication: Arc::new(Mutex::new(recovery.binding.source.clone())),
        rejection: rejection.clone(),
    };
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(
                crate::control::proto::replica_peer_server::ReplicaPeerServer::new(service),
            )
            .serve_with_incoming_shutdown(TcpListenerStream::new(listener), async {
                let _ = stopped.await;
            })
            .await
    });
    recovery.fresh.test_publish(Vec::new());
    *recovery.fresh_access.discovery.lock().unwrap() = Some((address, false, 0));
    for code in [
        tonic::Code::PermissionDenied,
        tonic::Code::Unauthenticated,
        tonic::Code::InvalidArgument,
        tonic::Code::FailedPrecondition,
    ] {
        *rejection.lock().unwrap() = Some(code);
        let before = recovery
            .fresh_access
            .discovery
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .2;
        assert!(matches!(
            tokio::time::timeout(
                Duration::from_secs(5),
                recovery.fresh.recovery_peer(
                    &recovery.binding.primary().identity,
                    &recovery.binding.primary().process_session_id,
                    recovery.binding.history()
                )
            )
            .await
            .unwrap(),
            Err(RuntimeError::ControlPlane(_))
        ));
        assert_eq!(
            recovery
                .fresh_access
                .discovery
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .2,
            before + 1
        );
    }
    *rejection.lock().unwrap() = None;
    recovery.finish().await;
    stop.send(()).unwrap();
    server.await.unwrap().unwrap();
}

#[tokio::test]
async fn m3_unavailable_source_fences_wire_before_draining_established_local_receipt() {
    unavailable_source_receipts(false).await;
}

#[tokio::test]
async fn m3_unanswered_source_confirmation_drains_finite_receipts_without_wire_credit() {
    unavailable_source_receipts(true).await;
}

async fn unavailable_source_receipts(unanswered: bool) {
    let recovery = Recovery::new().await;
    recovery
        .fresh
        .control(&recovery.binding, Control::Recover)
        .await
        .unwrap();
    recovery.complete().await;
    let gate = Gate::new();
    let remote = Gate::new();
    remote.armed.store(false, Ordering::Release);
    let _source_route =
        unanswered.then(|| recovery.route(vec![(Cut::ConfirmReceived, remote.clone())]));
    let unavailable = Arc::new(AtomicBool::new(false));
    let held = gate.clone();
    let disconnected = unavailable.clone();
    let _route = TestingTcpRoute::register_with_kind(
        recovery.fresh.local().identity.instance_id.to_string(),
        move |kind, _, address| {
            let held = held.clone();
            let disconnected = disconnected.clone();
            Box::pin(async move {
                if kind == Cut::BeforeCredit && held.armed.swap(false, Ordering::SeqCst) {
                    held.entered.add_permits(1);
                    held.release.acquire().await.unwrap().forget();
                }
                if kind == Cut::EngineConfirm && disconnected.load(Ordering::Acquire) && !unanswered
                {
                    return Ok("http://127.0.0.1:0".to_owned());
                }
                Ok(address)
            })
        },
    )
    .unwrap();
    let mut stream = recovery
        .target
        .state_replicator()
        .unwrap()
        .replication_operations()
        .unwrap();
    let before = recovery.fresh.native_credit_count();
    let mut completions = Vec::new();
    let mut last_lsn = 0;
    for index in 0..3 {
        let write = recovery
            .source
            .state_replicator()
            .unwrap()
            .begin_replicate(Bytes::from_static(b"actual-retained-receiver-operation"))
            .unwrap();
        let operation = tokio::time::timeout(Duration::from_secs(5), stream.get_operation())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(operation.lsn, write.lsn);
        assert_eq!(
            operation.data,
            Bytes::from_static(b"actual-retained-receiver-operation")
        );
        last_lsn = write.lsn;
        completions.push(write.completion);
        operation.acknowledge().unwrap();
        if index == 0 {
            gate.entered().await;
        }
    }
    assert_eq!(recovery.fresh.native_credit_count(), before);
    remote.armed.store(unanswered, Ordering::Release);
    unavailable.store(true, Ordering::Release);
    gate.release.add_permits(1);
    if unanswered {
        remote.entered().await;
        tokio::time::timeout(Duration::from_secs(10), async {
            while recovery.fresh.native_credit_count() != before + 3 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the existing five-second wire deadline must settle finite local receipts");
    } else {
        until(|| recovery.fresh.native_credit_count() == before + 3).await;
    }
    assert!(recovery.fresh.sessions.active(&recovery.binding).is_err());
    assert!(recovery.fresh.check_health().is_ok());
    assert!(
        recovery
            .fresh
            .inspect_acks()
            .as_array()
            .unwrap()
            .iter()
            .all(|entry| entry["failure"].is_null())
    );
    for completion in &mut completions {
        assert!(
            tokio::time::timeout(Duration::from_millis(50), completion)
                .await
                .is_err(),
            "local durability drainage must not export an unconfirmed wire ACK"
        );
    }
    remote.release.add_permits(1);
    tokio::time::timeout(
        Duration::from_secs(5),
        recovery.target.replicator().update_epoch(epoch(2)),
    )
    .await
    .expect("all real pre-fence local receipts must drain for a higher epoch")
    .unwrap();
    assert_eq!(
        recovery
            .target
            .replicator()
            .current_progress()
            .await
            .unwrap(),
        last_lsn
    );
    unavailable.store(false, Ordering::Release);
    assert!(
        recovery
            .fresh
            .confirm_effect(&recovery.binding)
            .await
            .is_err()
    );
    assert!(recovery.fresh.check_health().is_ok());
    let mut successor = recovery.binding.clone();
    successor.nonce = uuid::Uuid::new_v4().to_string();
    assert!(recovery.fresh.sessions.reserve(&successor).is_err());
    assert_eq!(recovery.fresh.native_credit_count(), before + 3);
    recovery.finish().await;
    for completion in completions {
        assert!(completion.await.is_err());
    }
}

#[tokio::test]
async fn m3_unavailable_ingress_drops_unconfirmed_frame_without_destroying_retained_receiver() {
    unavailable_owned_work(Some(Cut::BeforeDispatch)).await;
}

#[tokio::test]
async fn m3_unavailable_delivery_drains_only_previously_accepted_native_operation() {
    unavailable_owned_work(Some(Cut::BeforeDelivery)).await;
}

#[tokio::test]
async fn m3_unavailable_receiver_keeps_handed_out_local_durability_receipt_drainable() {
    unavailable_owned_work(None).await;
}

async fn unavailable_owned_work(cut: Option<Cut>) {
    let recovery = Recovery::new().await;
    recovery
        .fresh
        .control(&recovery.binding, Control::Recover)
        .await
        .unwrap();
    recovery.complete().await;
    let gate = Gate::new();
    let unavailable = Arc::new(AtomicBool::new(false));
    let blocked = unavailable.clone();
    let held = gate.clone();
    let _route = TestingTcpRoute::register_with_kind(
        recovery.fresh.local().identity.instance_id.to_string(),
        move |kind, _, address| {
            let held = held.clone();
            let blocked = blocked.clone();
            Box::pin(async move {
                if cut == Some(kind) && held.armed.swap(false, Ordering::SeqCst) {
                    held.entered.add_permits(1);
                    held.release.acquire().await.unwrap().forget();
                }
                Ok(
                    if kind == Cut::EngineConfirm && blocked.load(Ordering::Acquire) {
                        "http://127.0.0.1:0".to_owned()
                    } else {
                        address
                    },
                )
            })
        },
    )
    .unwrap();
    let mut stream = recovery
        .target
        .state_replicator()
        .unwrap()
        .replication_operations()
        .unwrap();
    let read = tokio::spawn(async move { stream.get_operation().await });
    let write = recovery
        .source
        .state_replicator()
        .unwrap()
        .begin_replicate(Bytes::from_static(b"owned-before-source-unavailability"))
        .unwrap();
    let mut completion = write.completion;
    let before = recovery.fresh.native_credit_count();
    if cut.is_some() {
        gate.entered().await;
        unavailable.store(true, Ordering::Release);
        gate.release.add_permits(1);
    }
    if cut == Some(Cut::BeforeDispatch) {
        until(|| recovery.fresh.sessions.active(&recovery.binding).is_err()).await;
        read.abort();
        assert!(matches!(read.await, Err(error) if error.is_cancelled()));
        assert_eq!(recovery.fresh.native_credit_count(), before);
        assert_eq!(
            recovery
                .target
                .replicator()
                .current_progress()
                .await
                .unwrap(),
            0
        );
    } else {
        let operation = tokio::time::timeout(Duration::from_secs(5), read)
            .await
            .unwrap()
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(operation.lsn, write.lsn);
        assert_eq!(
            operation.data,
            Bytes::from_static(b"owned-before-source-unavailability")
        );
        if cut.is_none() {
            unavailable.store(true, Ordering::Release);
            assert!(matches!(
                recovery.fresh.confirm_effect(&recovery.binding).await,
                Err(RuntimeError::PeerUnavailable(_))
            ));
        }
        assert!(recovery.fresh.sessions.active(&recovery.binding).is_err());
        operation.acknowledge().unwrap();
        assert_eq!(recovery.fresh.native_credit_count(), before + 1);
        assert!(operation.acknowledge().is_err());
        assert_eq!(
            recovery
                .target
                .replicator()
                .current_progress()
                .await
                .unwrap(),
            write.lsn
        );
    }
    assert!(recovery.fresh.check_health().is_ok());
    assert!(
        recovery
            .fresh
            .sessions
            .fence(&recovery.binding)
            .check()
            .is_err()
    );
    let mut successor = recovery.binding.clone();
    successor.nonce = uuid::Uuid::new_v4().to_string();
    assert!(recovery.fresh.sessions.reserve(&successor).is_err());
    assert!(
        tokio::time::timeout(Duration::from_millis(50), &mut completion)
            .await
            .is_err(),
        "retained local work is not an unconfirmed source quorum ACK"
    );
    tokio::time::timeout(
        Duration::from_secs(5),
        recovery.target.replicator().update_epoch(epoch(2)),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(recovery.fresh.check_health().is_ok());
    recovery.finish().await;
    assert!(completion.await.is_err());
}

#[tokio::test]
async fn m3_unavailable_source_local_receipt_failure_remains_sticky_through_actual_close() {
    let recovery = Recovery::new().await;
    recovery
        .fresh
        .control(&recovery.binding, Control::Recover)
        .await
        .unwrap();
    recovery.complete().await;
    let _route = TestingTcpRoute::register_with_kind(
        recovery.fresh.local().identity.instance_id.to_string(),
        |kind, _, address| {
            Box::pin(async move {
                Ok(if kind == Cut::EngineConfirm {
                    "http://127.0.0.1:0".to_owned()
                } else {
                    address
                })
            })
        },
    )
    .unwrap();
    let before = recovery.fresh.native_credit_count();
    let permit = recovery
        .fresh
        .acknowledgements()
        .reserve_owned()
        .await
        .unwrap();
    recovery
        .fresh
        .sessions
        .effect(&recovery.binding, || {
            recovery.fresh.queue_ack(
                recovery.binding.clone(),
                permit,
                Box::new(|| {
                    Err(RuntimeError::Application(
                        "actual local receipt descendant failed".into(),
                    ))
                }),
            )
        })
        .unwrap()
        .unwrap();
    until(|| {
        recovery
            .fresh
            .inspect_acks()
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["failure"].is_string())
    })
    .await;
    assert_eq!(recovery.fresh.native_credit_count(), before);
    assert!(recovery.fresh.check_health().is_err());
    assert!(recovery.fresh.sessions.active(&recovery.binding).is_err());
    recovery.target.replicator().abort();
    let failure = recovery
        .target
        .replicator()
        .close()
        .await
        .unwrap_err()
        .to_string();
    assert!(
        failure.contains("actual local receipt descendant failed"),
        "{failure}"
    );
    assert_eq!(
        recovery
            .target
            .replicator()
            .close()
            .await
            .unwrap_err()
            .to_string(),
        failure
    );
    assert!(recovery.fresh.drain().await.is_err());
    assert_eq!(recovery.fresh.cardinality(), 0);
    recovery.source.replicator().abort();
    recovery.source.replicator().close().await.unwrap();
    recovery.pair.finish().await;
}

#[tokio::test]
async fn m3_owned_local_receipt_error_and_panic_stay_sticky_without_wire_credit() {
    for panic in [false, true] {
        let recovery = Recovery::new().await;
        recovery
            .fresh
            .control(&recovery.binding, Control::Recover)
            .await
            .unwrap();
        recovery.complete().await;
        let _route = TestingTcpRoute::register_with_kind(
            recovery.fresh.local().identity.instance_id.to_string(),
            |kind, _, address| {
                Box::pin(async move {
                    Ok(if kind == Cut::EngineConfirm {
                        "http://127.0.0.1:0".to_owned()
                    } else {
                        address
                    })
                })
            },
        )
        .unwrap();
        assert!(matches!(
            recovery.fresh.confirm_effect(&recovery.binding).await,
            Err(RuntimeError::PeerUnavailable(_))
        ));
        let before = recovery.fresh.native_credit_count();
        let failure = recovery
            .fresh
            .drain_local_receipt(&recovery.binding, || {
                assert!(!panic, "injected actual local receipt callback panic");
                Err(RuntimeError::Application(
                    "injected actual local receipt callback failure".into(),
                ))
            })
            .unwrap_err()
            .to_string();
        assert_eq!(recovery.fresh.native_credit_count(), before);
        assert!(recovery.fresh.check_health().is_err());
        recovery.target.replicator().abort();
        let closed = recovery
            .target
            .replicator()
            .close()
            .await
            .unwrap_err()
            .to_string();
        assert!(
            closed.contains(&failure),
            "{closed} did not retain {failure}"
        );
        assert_eq!(
            recovery
                .target
                .replicator()
                .close()
                .await
                .unwrap_err()
                .to_string(),
            closed
        );
        assert!(recovery.fresh.drain().await.is_err());
        assert_eq!(recovery.fresh.cardinality(), 0);
        recovery.source.replicator().abort();
        recovery.source.replicator().close().await.unwrap();
        recovery.pair.finish().await;
    }
}

#[tokio::test]
async fn m3_queued_receipt_panics_before_and_during_local_drain_keep_actual_close_joinable() {
    for panic_later in [false, true] {
        let recovery = Recovery::new().await;
        recovery
            .fresh
            .control(&recovery.binding, Control::Recover)
            .await
            .unwrap();
        recovery.complete().await;
        let held = Gate::new();
        let gate = held.clone();
        let _route = TestingTcpRoute::register_with_kind(
            recovery.fresh.local().identity.instance_id.to_string(),
            move |kind, _, address| {
                let gate = gate.clone();
                Box::pin(async move {
                    if kind == Cut::BeforeCredit && gate.armed.swap(false, Ordering::SeqCst) {
                        gate.entered.add_permits(1);
                        gate.release.acquire().await.unwrap().forget();
                    }
                    Ok(if kind == Cut::EngineConfirm {
                        "http://127.0.0.1:0".to_owned()
                    } else {
                        address
                    })
                })
            },
        )
        .unwrap();
        let called = Arc::new(AtomicUsize::new(0));
        let before = recovery.fresh.native_credit_count();
        for index in 0..2 {
            let permit = recovery
                .fresh
                .acknowledgements()
                .reserve_owned()
                .await
                .unwrap();
            let called = called.clone();
            recovery
                .fresh
                .sessions
                .effect(&recovery.binding, || {
                    recovery.fresh.queue_ack(
                        recovery.binding.clone(),
                        permit,
                        Box::new(move || {
                            called.fetch_add(1, Ordering::AcqRel);
                            assert!(
                                (index == 1) != panic_later,
                                "injected queued native callback panic"
                            );
                            Ok(())
                        }),
                    )
                })
                .unwrap()
                .unwrap();
            if index == 0 {
                held.entered().await;
            }
        }
        held.release.add_permits(1);
        until(|| {
            recovery
                .fresh
                .inspect_acks()
                .as_array()
                .unwrap()
                .iter()
                .any(|entry| entry["failure"].is_string())
        })
        .await;
        assert_eq!(
            called.load(Ordering::Acquire),
            if panic_later { 2 } else { 1 }
        );
        assert_eq!(
            recovery.fresh.native_credit_count(),
            before + i64::from(panic_later)
        );
        assert!(recovery.fresh.sessions.active(&recovery.binding).is_err());
        assert!(
            recovery
                .fresh
                .sessions
                .fence(&recovery.binding)
                .check()
                .is_err()
        );
        recovery.target.replicator().abort();
        let failure = recovery
            .target
            .replicator()
            .close()
            .await
            .unwrap_err()
            .to_string();
        assert!(failure.contains("native ACK owner panicked"), "{failure}");
        assert_eq!(
            recovery
                .target
                .replicator()
                .close()
                .await
                .unwrap_err()
                .to_string(),
            failure
        );
        assert!(recovery.fresh.drain().await.is_err());
        assert_eq!(recovery.fresh.cardinality(), 0);
        recovery.source.replicator().abort();
        recovery.source.replicator().close().await.unwrap();
        recovery.pair.finish().await;
    }
}

#[tokio::test]
async fn m2_cleanup_cancels_local_confirmation_without_dropping_remote_control_join() {
    let recovery = Recovery::new().await;
    recovery
        .fresh
        .control(&recovery.binding, Control::Recover)
        .await
        .unwrap();
    recovery.complete().await;
    let confirm = Gate::new();
    let _route = recovery.route(vec![(Cut::ConfirmReceived, confirm.clone())]);
    let sender = recovery.fresh.acknowledgements();
    let permit = sender.reserve_owned().await.unwrap();
    let credited = Arc::new(AtomicBool::new(false));
    let credit = credited.clone();
    recovery
        .fresh
        .sessions
        .effect(&recovery.binding, || {
            recovery.fresh.queue_ack(
                recovery.binding.clone(),
                permit,
                Box::new(move || {
                    credit.store(true, Ordering::Release);
                    Ok(())
                }),
            )
        })
        .unwrap()
        .unwrap();
    confirm.entered().await;
    recovery.target.replicator().abort();
    tokio::time::timeout(Duration::from_secs(1), recovery.target.replicator().close())
        .await
        .expect("selected local shutdown must cancel the network wait")
        .unwrap();
    assert!(!credited.load(Ordering::Acquire));
    assert_eq!(recovery.fresh.cardinality(), 0);
    assert!(recovery.fresh.check_health().is_err());
    let source = recovery.source.replicator();
    source.abort();
    let close = tokio::spawn(async move { source.close().await });
    tokio::task::yield_now().await;
    assert!(
        !close.is_finished(),
        "remote control must remain joined by its source owner"
    );
    confirm.release.add_permits(1);
    close.await.unwrap().unwrap();
    recovery.finish().await;
}

#[tokio::test]
async fn m2_cleanup_preserves_confirmation_failure_selected_before_shutdown() {
    let recovery = Recovery::new().await;
    recovery
        .fresh
        .control(&recovery.binding, Control::Recover)
        .await
        .unwrap();
    recovery.complete().await;
    let confirm = Gate::new();
    let _route = recovery.route(vec![(Cut::ConfirmReceived, confirm.clone())]);
    let permit = recovery
        .fresh
        .acknowledgements()
        .reserve_owned()
        .await
        .unwrap();
    let credited = Arc::new(AtomicBool::new(false));
    let credit = credited.clone();
    recovery
        .fresh
        .sessions
        .effect(&recovery.binding, || {
            recovery.fresh.queue_ack(
                recovery.binding.clone(),
                permit,
                Box::new(move || {
                    credit.store(true, Ordering::Release);
                    Ok(())
                }),
            )
        })
        .unwrap()
        .unwrap();
    confirm.entered().await;
    recovery
        .pair
        .source_access
        .withdrawn
        .store(true, Ordering::Release);
    confirm.release.add_permits(1);
    until(|| {
        recovery
            .fresh
            .inspect_acks()
            .as_array()
            .unwrap()
            .iter()
            .any(|owner| owner["failure"].is_string())
    })
    .await;
    assert!(!credited.load(Ordering::Acquire));
    recovery.target.replicator().abort();
    let failure = recovery
        .target
        .replicator()
        .close()
        .await
        .unwrap_err()
        .to_string();
    assert!(failure.contains("authority"), "{failure}");
    assert_eq!(
        recovery
            .target
            .replicator()
            .close()
            .await
            .unwrap_err()
            .to_string(),
        failure
    );
    assert!(recovery.fresh.drain().await.is_err());
    assert_eq!(recovery.fresh.cardinality(), 0);
    recovery.source.replicator().abort();
    recovery.source.replicator().close().await.unwrap();
    recovery.pair.finish().await;
}

#[tokio::test]
async fn m2_recovered_observer_waits_for_the_actual_retained_transfer_result() {
    let recovery = Recovery::new().await;
    let discovery = Gate::new();
    let _route = recovery.route(vec![(Cut::RecoveryDiscovery, discovery.clone())]);
    recovery
        .fresh
        .control(&recovery.binding, Control::Recover)
        .await
        .unwrap();
    discovery.entered().await;
    {
        let owner = recovery.pair.source.test_production();
        let observer = owner.recover_source(&recovery.binding, true);
        tokio::pin!(observer);
        assert!(
            futures::poll!(observer.as_mut()).is_pending(),
            "pending transfer is retained work to await, not QueueFull to poll over the wire"
        );
        discovery.release.add_permits(1);
        recovery.complete().await;
        observer.await.unwrap();
    }
    recovery.write().await;
    recovery.finish().await;
}

#[tokio::test]
async fn m2_recovered_observers_leave_capacity_for_actual_transfer_progress() {
    let recovery = Recovery::new().await;
    let discovery = Gate::new();
    let _route = recovery.route(vec![(Cut::RecoveryDiscovery, discovery.clone())]);
    recovery
        .fresh
        .control(&recovery.binding, Control::Recover)
        .await
        .unwrap();
    discovery.entered().await;
    let capacity = recovery.pair.source.controls_available();
    let mut observers = Vec::new();
    for _ in 0..capacity {
        let target = recovery.fresh.clone();
        let binding = recovery.binding.clone();
        observers.push(tokio::spawn(async move {
            target.control(&binding, Control::Recovered).await
        }));
    }
    until(|| recovery.pair.source.controls_available() == 1).await;
    tokio::time::timeout(
        Duration::from_secs(1),
        recovery.fresh.control(&recovery.binding, Control::Recover),
    )
    .await
    .expect("waiting observers must not occupy the progress slot")
    .unwrap();
    assert_eq!(
        recovery.pair.source.test_production().inspect()["recoverySpawned"],
        1
    );
    discovery.release.add_permits(1);
    recovery.complete().await;
    for observer in observers {
        observer.await.unwrap().unwrap();
    }
    until(|| recovery.pair.source.controls_available() == capacity).await;
    recovery.write().await;
    recovery.finish().await;
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
    until(|| {
        recovery
            .pair
            .source
            .sessions
            .active(&recovery.predecessor)
            .is_err()
    })
    .await;
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
            .predecessor_completed(&recovery.predecessor)
    );
    assert!(
        recovery
            .pair
            .source
            .sessions
            .active(&recovery.predecessor)
            .is_err()
    );
    assert!(
        recovery
            .pair
            .source
            .sessions
            .fence(&recovery.predecessor)
            .check()
            .is_err()
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
