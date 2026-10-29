//! Independent contract fixture. No database/native positive qualification.
use super::*;
use kuberic_runtime::replicator::{CustomReplicatorControl, ReplicatorCapabilities};
use tokio::sync::{Mutex as AsyncMutex, Semaphore};
use tokio_util::sync::CancellationToken;

struct Contract {
    context: OnceLock<ReplicatorFactoryContext>,
    endpoint: String,
    peer: Mutex<Option<std::sync::Weak<Contract>>>,
    inbound: Mutex<Option<PublicInboundBuildAuthority>>,
    installed: Mutex<Option<ConfigurationDescriptor>>,
    idle: Mutex<Vec<ReplicaId>>,
    observation: AtomicUsize,
    observe_hold: AtomicBool,
    observe_entered: Semaphore,
    observe_release: Semaphore,
    child_cancel: CancellationToken,
    children: AsyncMutex<Vec<tokio::task::JoinHandle<()>>>,
    alive: Arc<AtomicUsize>,
    hold_build: AtomicBool,
    build_entered: Semaphore,
    build_release: Semaphore,
    fail_drain: AtomicBool,
    panic_drain: AtomicBool,
    installations: AtomicUsize,
    fail_install: AtomicBool,
    factory_hold: AtomicBool,
    factory_entered: Semaphore,
    factory_release: Semaphore,
    different_bundle: AtomicBool,
    install_hold: AtomicBool,
    install_entered: Semaphore,
    install_release: Semaphore,
    terminal_calls: AtomicUsize,
    drain_hold: AtomicBool,
    drain_entered: Semaphore,
    drain_release: Semaphore,
}

fn failure(message: &str) -> RuntimeError {
    RuntimeError::Application(message.into())
}

impl Contract {
    async fn child(&self) {
        let cancel = self.child_cancel.clone();
        let alive = self.alive.clone();
        alive.fetch_add(1, Ordering::SeqCst);
        self.children.lock().await.push(tokio::spawn(async move {
            cancel.cancelled().await;
            alive.fetch_sub(1, Ordering::SeqCst);
        }));
    }
    async fn drain(&self) -> Result<()> {
        if self.panic_drain.load(Ordering::SeqCst) {
            panic!("lost contract drain owner");
        }
        if self.fail_drain.load(Ordering::SeqCst) {
            return Err(failure("owned child still live"));
        }
        if self.drain_hold.load(Ordering::SeqCst) {
            self.drain_entered.add_permits(1);
            self.drain_release.acquire().await.unwrap().forget();
        }
        self.child_cancel.cancel();
        self.build_release.add_permits(1);
        for task in self.children.lock().await.drain(..) {
            task.await.map_err(|e| failure(&e.to_string()))?;
        }
        Ok(())
    }
    async fn accept(&self, authority: &PublicInboundBuildAuthority) -> Result<()> {
        self.context
            .get()
            .unwrap()
            .validate_build_authority(authority)
            .await?;
        if self.inbound.lock().unwrap().as_ref() != Some(authority) {
            return Err(failure("not installed"));
        }
        self.child().await;
        self.context
            .get()
            .unwrap()
            .validate_build_authority(authority)
            .await
    }
}

struct Factory(Arc<Contract>);
#[async_trait::async_trait]
impl ReplicatorFactory for Factory {
    async fn create_replicator(
        &self,
        context: ReplicatorFactoryContext,
        provider: Option<Arc<dyn StateProvider>>,
        _: ReplicatorSettings,
    ) -> Result<ReplicatorInterfaces> {
        assert!(provider.is_none());
        self.0
            .context
            .set(context.clone())
            .map_err(|_| failure("duplicate context"))?;
        let interfaces = ReplicatorInterfaces::primary(
            self.0.clone(),
            None,
            ReplicatorCapabilities::conservative(),
        )
        .with_custom_control(&context, self.0.clone())?;
        if self.0.factory_hold.load(Ordering::SeqCst) {
            self.0.child().await;
            self.0.factory_entered.add_permits(1);
            self.0.factory_release.acquire().await.unwrap().forget();
        }
        if self.0.different_bundle.load(Ordering::SeqCst) {
            self.0.child().await;
            return Ok(ReplicatorInterfaces::primary(
                self.0.clone(),
                None,
                ReplicatorCapabilities::conservative(),
            ));
        }
        Ok(interfaces)
    }
}
#[async_trait::async_trait]
impl StatefulServiceReplica for Contract {
    async fn open(self: Arc<Self>, context: OpenContext) -> Result<Arc<dyn Replicator>> {
        let interfaces = context
            .partition
            .with_factory(Arc::new(Factory(self.clone())))
            .create_replicator(
                None,
                Some(ReplicatorSettings {
                    replication_address: self.endpoint.clone(),
                }),
            )
            .await?;
        Ok(interfaces.replicator())
    }
    async fn change_role(&self, _: ReplicaRole) -> Result<RoleChange> {
        Ok(RoleChange {
            service_address: None,
        })
    }
    async fn close(&self) -> Result<()> {
        self.terminal_calls.fetch_add(1, Ordering::SeqCst);
        self.install_release.add_permits(1);
        Ok(())
    }
    fn abort(&self) {
        self.terminal_calls.fetch_add(1, Ordering::SeqCst);
        self.install_release.add_permits(1);
        self.build_release.add_permits(1);
    }
}
#[async_trait::async_trait]
impl Replicator for Contract {
    async fn open(&self) -> Result<String> {
        Ok(self.endpoint.clone())
    }
    async fn change_role(&self, _: Epoch, _: ReplicaRole) -> Result<()> {
        Ok(())
    }
    async fn update_epoch(&self, _: Epoch) -> Result<()> {
        Ok(())
    }
    async fn close(&self) -> Result<()> {
        Ok(())
    }
    fn abort(&self) {
        StatefulServiceReplica::abort(self);
    }
    async fn current_progress(&self) -> Result<i64> {
        Err(RuntimeError::ProgressNotReady)
    }
    async fn catch_up_capability(&self) -> Result<i64> {
        Err(RuntimeError::ProgressNotReady)
    }
}
#[async_trait::async_trait]
impl PrimaryReplicator for Contract {
    async fn on_data_loss(&self) -> Result<bool> {
        Err(failure("unsupported contract fixture recovery"))
    }
    async fn update_catch_up_replica_set_configuration(
        &self,
        current: ReplicaSetConfiguration,
        _: ReplicaSetConfiguration,
    ) -> Result<()> {
        self.update_current_replica_set_configuration(current).await
    }
    async fn update_current_replica_set_configuration(
        &self,
        current: ReplicaSetConfiguration,
    ) -> Result<()> {
        self.idle.lock().unwrap().retain(|id| {
            !current
                .configuration
                .members
                .iter()
                .any(|member| member.identity.replica_id == *id)
        });
        *self.installed.lock().unwrap() = Some(current.configuration);
        Ok(())
    }
    async fn wait_for_catch_up_quorum(&self, _: ReplicaSetQuorumMode) -> Result<()> {
        Err(failure("no native quorum in contract fixture"))
    }
    async fn build_replica(&self, replica: ReplicaInformation) -> Result<()> {
        let context = self.context.get().unwrap();
        let authority = context.outbound_build_authority(&replica.build_id).await?;
        assert_eq!(authority.attempt.target, replica.identity);
        assert_eq!(
            Some(&authority.attempt.target_storage),
            replica.storage_incarnation.as_ref()
        );
        let peer = self
            .peer
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .upgrade()
            .unwrap();
        peer.accept(&authority).await?;
        self.child().await;
        self.build_entered.add_permits(1);
        if self.hold_build.load(Ordering::SeqCst) {
            self.build_release.acquire().await.unwrap().forget();
        }
        context.validate_build_authority(&authority).await?;
        self.idle.lock().unwrap().push(replica.identity.replica_id);
        Ok(())
    }
    async fn remove_replica(&self, _: ReplicaId) -> Result<()> {
        Err(failure("unsupported contract fixture remove"))
    }
}
#[async_trait::async_trait]
impl CustomReplicatorControl for Contract {
    async fn install_inbound_build_authority(
        &self,
        authority: &PublicInboundBuildAuthority,
    ) -> Result<()> {
        self.context
            .get()
            .unwrap()
            .validate_build_authority(authority)
            .await?;
        self.installations.fetch_add(1, Ordering::SeqCst);
        *self.inbound.lock().unwrap() = Some(authority.clone());
        if self.install_hold.load(Ordering::SeqCst) {
            self.child().await;
            self.install_entered.add_permits(1);
            self.install_release.acquire().await.unwrap().forget();
        }
        if self.fail_install.load(Ordering::SeqCst) {
            self.child().await;
            return Err(failure("partially installed contract ingress"));
        }
        Ok(())
    }
    async fn revoke_inbound_build_authority(
        &self,
        authority: &PublicInboundBuildAuthority,
    ) -> Result<()> {
        assert!(
            self.context
                .get()
                .unwrap()
                .validate_build_authority(authority)
                .await
                .is_err()
        );
        if self.inbound.lock().unwrap().as_ref() == Some(authority) {
            *self.inbound.lock().unwrap() = None;
        }
        Ok(())
    }
    async fn idle_replicas(&self, configuration: &PublicConfiguration) -> Result<Vec<ReplicaId>> {
        let ids = match self.observation.load(Ordering::SeqCst) {
            1 => return Err(RuntimeError::ProgressNotReady),
            2 => vec![ReplicaId::new(2), ReplicaId::new(2)],
            3 => vec![ReplicaId::new(1)],
            4 => return Err(failure("different disjoint installed configuration")),
            5 => vec![ReplicaId::new(-1)],
            _ => {
                if self.installed.lock().unwrap().as_ref() != Some(&configuration.current) {
                    return Err(failure("configuration mismatch"));
                }
                self.idle.lock().unwrap().clone()
            }
        };
        self.observe_entered.add_permits(1);
        if self.observe_hold.swap(false, Ordering::SeqCst) {
            self.observe_release.acquire().await.unwrap().forget();
        }
        Ok(ids)
    }
    async fn shutdown_and_drain(&self) -> Result<()> {
        self.drain().await
    }
}
#[async_trait::async_trait]
impl Sample for Contract {
    fn fresh(_: &Path) -> bool {
        true
    }
    fn create(_: &Path) -> Arc<Self> {
        static NEXT: AtomicUsize = AtomicUsize::new(1);
        Arc::new(Self {
            context: OnceLock::new(),
            endpoint: format!(
                "http://contract-{}:7000",
                NEXT.fetch_add(1, Ordering::SeqCst)
            ),
            peer: Mutex::new(None),
            inbound: Mutex::new(None),
            installed: Mutex::new(None),
            idle: Mutex::new(vec![]),
            observation: AtomicUsize::new(0),
            observe_hold: AtomicBool::new(false),
            observe_entered: Semaphore::new(0),
            observe_release: Semaphore::new(0),
            child_cancel: CancellationToken::new(),
            children: AsyncMutex::new(vec![]),
            alive: Arc::new(AtomicUsize::new(0)),
            hold_build: AtomicBool::new(false),
            build_entered: Semaphore::new(0),
            build_release: Semaphore::new(0),
            fail_drain: AtomicBool::new(false),
            panic_drain: AtomicBool::new(false),
            installations: AtomicUsize::new(0),
            fail_install: AtomicBool::new(false),
            factory_hold: AtomicBool::new(false),
            factory_entered: Semaphore::new(0),
            factory_release: Semaphore::new(0),
            different_bundle: AtomicBool::new(false),
            install_hold: AtomicBool::new(false),
            install_entered: Semaphore::new(0),
            install_release: Semaphore::new(0),
            terminal_calls: AtomicUsize::new(0),
            drain_hold: AtomicBool::new(false),
            drain_entered: Semaphore::new(0),
            drain_release: Semaphore::new(0),
        })
    }
    async fn drain(&self) -> Result<()> {
        Contract::drain(self).await
    }
}

fn history(
    node: &Node<Contract>,
    command: PublicV1HistoryCommand,
) -> impl std::future::Future<Output = std::result::Result<(), tonic::Status>> + Send + 'static {
    let address = node.control_address.clone();
    let resource = node.history.resource_uid.clone();
    let identity = node.identity.clone();
    let session = node.session.clone();
    async move {
        let mut client = proto::replica_peer_client::ReplicaPeerClient::connect(address)
            .await
            .unwrap();
        client
            .execute(authorized(proto::ExecuteCommandRequest {
                protocol_version: kuberic_runtime::protocol::PROTOCOL_VERSION,
                resource_uid: resource.to_string(),
                target: Some(identity.into()),
                expected_process_session_id: session.to_string(),
                command: Some(proto::execute_command_request::Command::PublicV1History(
                    convert::public_v1_history_command_to_proto(&command).unwrap(),
                )),
            }))
            .await
            .map(|_| ())
    }
}

fn execute(
    node: &Node<Contract>,
    intent: PublicOperationIntent,
) -> tokio::task::JoinHandle<std::result::Result<(), tonic::Status>> {
    let address = node.control_address.clone();
    let identity = node.identity.clone();
    let resource = node.history.resource_uid.clone();
    let session = node.session.clone();
    tokio::spawn(async move {
        let mut client = proto::agent_control_client::AgentControlClient::connect(address)
            .await
            .unwrap();
        client
            .execute(authorized(proto::ExecuteCommandRequest {
                protocol_version: kuberic_runtime::protocol::PROTOCOL_VERSION,
                resource_uid: resource.to_string(),
                target: Some(identity.clone().into()),
                expected_process_session_id: session.to_string(),
                command: Some(proto::execute_command_request::Command::PublicV1Operation(
                    convert::public_v1_operation_command_to_proto(&PublicV1OperationCommand {
                        target: identity,
                        intent,
                    })
                    .unwrap(),
                )),
            }))
            .await
            .map(|_| ())
    })
}

async fn setup(
    root: &Path,
) -> (
    Node<Contract>,
    Node<Contract>,
    PublicInboundBuildAuthority,
    PublicOperationIntent,
) {
    let mut source = Node::<Contract>::start_sized(&root.join("s"), 1, 1, 2).await;
    let mut target = Node::<Contract>::start_sized(&root.join("t"), 2, 1, 2).await;
    source.open(ReplicaRole::Primary, Epoch::new(0, 1)).await;
    let current = source.single(Epoch::new(0, 1)).await;
    target
        .open(ReplicaRole::IdleSecondary, Epoch::new(0, 1))
        .await;
    *source.service.peer.lock().unwrap() = Some(Arc::downgrade(&target.service));
    let attempt = OperationId::new("contract-build");
    let authority = PublicInboundBuildAuthority {
        operation_id: attempt.clone(),
        operation_revision: source.revision,
        attempt: PublicBuildAttemptReference {
            resource_uid: source.history.resource_uid.clone(),
            epoch: Epoch::new(0, 1),
            build_id: attempt.clone(),
            source: source.identity.clone(),
            source_storage: source.storage.clone(),
            source_session: source.session.clone(),
            target: target.identity.clone(),
            target_storage: target.storage.clone(),
            target_session: target.session.clone(),
        },
        history: source.history.clone(),
        current_configuration_id: current.configuration_id.clone(),
        source_address: source.service.endpoint.clone(),
        target_address: target.service.endpoint.clone(),
    };
    let program = PublicOperationProgram::Build(PublicBuildInput {
        attempt: attempt.clone(),
        source_address: authority.source_address.clone(),
        epoch: authority.attempt.epoch,
        current_configuration_id: Some(authority.current_configuration_id.clone()),
        replica: target.identity.clone(),
        target_storage: target.storage.clone(),
        process_session_id: target.session.clone(),
        replication_address: authority.target_address.clone(),
        history: Some(source.history.clone()),
    });
    let intent = PublicOperationIntent {
        identity: source.preview.clone(),
        operation_id: attempt,
        revision: source.revision,
        process_session_id: source.session.clone(),
        class: PublicOperationClass::Build {
            target: target.identity.replica_id,
        },
        input_digest: program.digest(),
        replay_disposition: program.replay_disposition(),
        lifecycle: None,
        program: Some(program),
    };
    (source, target, authority, intent)
}

fn validation_cut(
    position: kuberic_runtime::testing::PublicCutPosition,
) -> Arc<kuberic_runtime::testing::PublicOperationCut> {
    Arc::new(kuberic_runtime::testing::PublicOperationCut {
        position,
        index: 0,
        entered: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
    })
}

async fn renewal_target(
    root: &Path,
) -> (Node<Contract>, Node<Contract>, PublicInboundBuildAuthority) {
    let old_epoch = Epoch::new(0, 1);
    let epoch = Epoch::new(0, 2);
    let mut source = Node::<Contract>::start_sized(&root.join("s"), 1, 1, 2).await;
    source.open(ReplicaRole::Primary, old_epoch).await;
    source.single(old_epoch).await;
    let mut predecessor = Node::<Contract>::start_sized(&root.join("t"), 2, 1, 2).await;
    predecessor
        .open(ReplicaRole::ActiveSecondary, old_epoch)
        .await;
    let old = persist_query_observation(root, "renewal-predecessor", &predecessor.report().await);
    let revision = predecessor.revision;
    assert!(predecessor.stop().await.is_none());
    let mut target = Node::<Contract>::start_sized(&root.join("t"), 2, revision, 2).await;
    target
        .command(PublicOperationProgram::Open {
            replica: target.identity.clone(),
            existing: true,
        })
        .await;
    let fresh = persist_query_observation(root, "renewal-replacement", &target.report().await);
    let endpoint = |report: &PublicV1AgentReport| ProcessSessionEndpoint {
        identity: report.replica.clone(),
        storage_incarnation: report.storage_incarnation.clone(),
        process_session_id: report.process_session_id.clone(),
        replicator_creation_id: report.replicator_creation_id.clone(),
        role: report.role,
    };
    let survivor = endpoint(&persist_query_observation(
        root,
        "renewal-survivor",
        &source.report().await,
    ));
    let required = PublicV1SessionRenewalStatus::required(ProcessSessionRenewalRequired {
        operation_id: OperationId::new("contract-terminal-renewal"),
        reason: ProcessSessionRenewalReason::ActiveSecondaryReplacement,
        resource_uid: old.resource_uid.clone(),
        history: old.history.clone(),
        previous_epoch: old_epoch,
        renewal_epoch: epoch,
        predecessor: endpoint(&old),
        replacement: Some(endpoint(&fresh)),
        surviving_sessions: vec![survivor.clone()],
        fresh_configuration: ConfigurationDescriptor::new(
            epoch,
            source.identity.replica_id,
            vec![
                ConfigurationMember {
                    identity: source.identity.clone(),
                    role: ReplicaRole::Primary,
                },
                ConfigurationMember {
                    identity: target.identity.clone(),
                    role: ReplicaRole::ActiveSecondary,
                },
            ],
            2,
        ),
        predecessor_contained: true,
        data_loss: None,
    })
    .unwrap();
    required
        .required
        .validate_replacement_report(&fresh)
        .unwrap();
    history(
        &target,
        PublicV1HistoryCommand::SessionRenewal {
            command: Box::new(PublicV1SessionRenewalCommand::CloseAccessAndIngress {
                accepted: required.clone(),
            }),
        },
    )
    .await
    .unwrap();
    let mut staged = required.close_access().unwrap();
    for next in [
        staged.freeze_surviving_set().unwrap(),
        staged
            .freeze_surviving_set()
            .unwrap()
            .begin_epoch_barriers()
            .unwrap(),
    ] {
        target
            .running
            .handle()
            .persist_public_v1_session_renewal(Some(&staged), &next)
            .await
            .unwrap();
        staged = next;
    }
    history(
        &source,
        PublicV1HistoryCommand::SessionRenewal {
            command: Box::new(PublicV1SessionRenewalCommand::UpdateEpochBarrier {
                accepted: staged.clone(),
                survivor: survivor.clone(),
                epoch,
            }),
        },
    )
    .await
    .unwrap();
    let updated = staged.record_epoch_updated(&survivor, epoch).unwrap();
    target
        .running
        .handle()
        .persist_public_v1_session_renewal(Some(&staged), &updated)
        .await
        .unwrap();
    let role_pending = updated.finish_epoch_barriers().unwrap();
    target
        .running
        .handle()
        .persist_public_v1_session_renewal(Some(&updated), &role_pending)
        .await
        .unwrap();
    history(
        &target,
        PublicV1HistoryCommand::SessionRenewal {
            command: Box::new(PublicV1SessionRenewalCommand::ReplayReplacementRole {
                accepted: role_pending.clone(),
                process_session_id: target.session.clone(),
                epoch,
                role: ReplicaRole::ActiveSecondary,
            }),
        },
    )
    .await
    .unwrap();
    let report = target.report().await;
    assert_eq!(report.role, ReplicaRole::ActiveSecondary);
    let pending = role_pending
        .replay_replacement_role(&target.session, epoch, ReplicaRole::ActiveSecondary)
        .unwrap();
    target
        .running
        .handle()
        .persist_public_v1_session_renewal(Some(&pending), &pending)
        .await
        .unwrap();
    source
        .command(PublicOperationProgram::Role {
            epoch,
            role: ReplicaRole::Primary,
        })
        .await;
    source
        .command(PublicOperationProgram::Epoch { epoch })
        .await;
    let current = source.single(epoch).await;
    let build = OperationId::new("contract-renewal-inbound");
    let authority = PublicInboundBuildAuthority {
        operation_id: build.clone(),
        operation_revision: source.revision,
        attempt: PublicBuildAttemptReference {
            resource_uid: source.history.resource_uid.clone(),
            epoch,
            build_id: build,
            source: source.identity.clone(),
            source_storage: source.storage.clone(),
            source_session: source.session.clone(),
            target: target.identity.clone(),
            target_storage: target.storage.clone(),
            target_session: target.session.clone(),
        },
        history: source.history.clone(),
        current_configuration_id: current.configuration_id,
        source_address: source.service.endpoint.clone(),
        target_address: target.service.endpoint.clone(),
    };
    (source, target, authority)
}

#[tokio::test]
async fn custom_host_renewal_validation_cannot_cross_terminal_admission() {
    tokio::time::timeout(Duration::from_secs(90), async {
        for abort in [false, true] {
            for preinstall in [false, true] {
                let root = tempfile::tempdir_in(".").unwrap();
                let (source, target, authority) = renewal_target(root.path()).await;
                history(
                    &target,
                    PublicV1HistoryCommand::InstallInboundBuildAuthority {
                        authority: Box::new(authority.clone()),
                    },
                )
                .await
                .unwrap();
                *source.service.peer.lock().unwrap() = Some(Arc::downgrade(&target.service));
                source.service.hold_build.store(true, Ordering::SeqCst);
                let program = PublicOperationProgram::Build(PublicBuildInput {
                    attempt: authority.attempt.build_id.clone(),
                    source_address: authority.source_address.clone(),
                    epoch: authority.attempt.epoch,
                    current_configuration_id: Some(authority.current_configuration_id.clone()),
                    replica: target.identity.clone(),
                    target_storage: target.storage.clone(),
                    process_session_id: target.session.clone(),
                    replication_address: authority.target_address.clone(),
                    history: Some(authority.history.clone()),
                });
                let source_build = execute(
                    &source,
                    PublicOperationIntent {
                        identity: source.preview.clone(),
                        operation_id: authority.operation_id.clone(),
                        revision: authority.operation_revision,
                        process_session_id: source.session.clone(),
                        class: PublicOperationClass::Build {
                            target: target.identity.replica_id,
                        },
                        input_digest: program.digest(),
                        replay_disposition: program.replay_disposition(),
                        lifecycle: None,
                        program: Some(program),
                    },
                );
                source.service.build_entered.acquire().await.unwrap().forget();
                assert!(!source_build.is_finished());
                assert_eq!(target.service.installations.load(Ordering::SeqCst), 1);
                assert_eq!(target.service.alive.load(Ordering::SeqCst), 1);
                let before = target.report().await;
                assert_ne!(before.read_status, AccessStatus::Granted);
                assert_ne!(before.write_status, AccessStatus::Granted);
                assert!(before.configuration.is_none());
                let read = validation_cut(
                    kuberic_runtime::testing::PublicCutPosition::BuildAuthorityRead,
                );
                target
                    .running
                    .handle()
                    .set_public_operation_cut(Some(read.clone()));
                let validation = if preinstall {
                    // Exact install replay enters the coordinator's preinstall
                    // scope check while its actual grant is still installed.
                    tokio::spawn(history(
                        &target,
                        PublicV1HistoryCommand::InstallInboundBuildAuthority {
                            authority: Box::new(authority.clone()),
                        },
                    ))
                } else {
                    let service = target.service.clone();
                    let authority = authority.clone();
                    tokio::spawn(async move {
                        service.accept(&authority).await.map_err(|error| {
                            tonic::Status::failed_precondition(error.to_string())
                        })
                    })
                };
                read.entered.notified().await;
                let terminal_cut = validation_cut(
                    kuberic_runtime::testing::PublicCutPosition::BeforeAccessReservation,
                );
                target
                    .running
                    .handle()
                    .set_public_operation_cut(Some(terminal_cut.clone()));
                let program = if abort {
                    PublicOperationProgram::Abort
                } else {
                    PublicOperationProgram::Close
                };
                let operation_id = OperationId::new("contract-terminal-during-renewal");
                let terminal = execute(
                    &target,
                    PublicOperationIntent {
                        identity: target.preview.clone(),
                        operation_id: operation_id.clone(),
                        revision: before
                            .completed_operations
                            .iter()
                            .map(|operation| operation.revision)
                            .max()
                            .unwrap()
                            + 1,
                        process_session_id: target.session.clone(),
                        class: if abort {
                            PublicOperationClass::Abort
                        } else {
                            PublicOperationClass::Close
                        },
                        input_digest: program.digest(),
                        replay_disposition: program.replay_disposition(),
                        lifecycle: None,
                        program: Some(program),
                    },
                );
                terminal_cut.entered.notified().await;
                assert_eq!(target.service.terminal_calls.load(Ordering::SeqCst), 0);
                assert_eq!(target.service.inbound.lock().unwrap().as_ref(), Some(&authority));
                read.release.notify_one();
                assert!(
                    validation.await.unwrap().is_err(),
                    "stale renewal snapshot crossed terminal admission: abort={abort}, preinstall={preinstall}"
                );
                assert_eq!(target.service.installations.load(Ordering::SeqCst), 1);
                assert_eq!(target.service.alive.load(Ordering::SeqCst), 1);
                assert!(target.service.idle.lock().unwrap().is_empty());
                assert!(source.service.idle.lock().unwrap().is_empty());
                let admitted = target.report().await;
                assert!(admitted.configuration.is_none());
                assert_ne!(admitted.read_status, AccessStatus::Granted);
                assert_ne!(admitted.write_status, AccessStatus::Granted);
                assert!(!terminal.is_finished());
                target.service.drain_hold.store(true, Ordering::SeqCst);
                target.running.handle().set_public_operation_cut(None);
                terminal_cut.release.notify_one();
                target.service.drain_entered.acquire().await.unwrap().forget();
                assert!(!terminal.is_finished(), "terminal must join the retained child");
                assert_eq!(target.service.alive.load(Ordering::SeqCst), 1);
                assert!(
                    !target
                        .running
                        .handle()
                        .public_v1_operation_is_contained(&operation_id)
                        .await
                        .unwrap()
                );
                target.service.drain_release.add_permits(1);
                terminal.await.unwrap().unwrap();
                assert_eq!(target.service.alive.load(Ordering::SeqCst), 0);
                assert!(
                    target
                        .running
                        .handle()
                        .public_v1_operation_is_contained(&operation_id)
                        .await
                        .unwrap()
                );
                assert!(target.service.accept(&authority).await.is_err());
                target.service.drain_hold.store(false, Ordering::SeqCst);
                assert!(target.stop().await.is_none());
                assert!(source.stop().await.is_none());
                assert!(source_build.await.unwrap().is_err());
            }
        }
    })
    .await
    .expect("bounded synthetic companion terminal/renewal admission regression");
}

#[tokio::test]
async fn custom_host_validation_cannot_cross_source_target_cancel_or_revoke_cut() {
    for case in 0..4 {
        let root = tempfile::tempdir().unwrap();
        let (source, target, authority, intent) = setup(root.path()).await;
        source.service.hold_build.store(true, Ordering::SeqCst);
        history(
            &target,
            PublicV1HistoryCommand::InstallInboundBuildAuthority {
                authority: Box::new(authority.clone()),
            },
        )
        .await
        .unwrap();
        let build = execute(&source, intent);
        source
            .service
            .build_entered
            .acquire()
            .await
            .unwrap()
            .forget();
        let node = if case == 1 || case == 3 {
            &target
        } else {
            &source
        };
        let cut = validation_cut(kuberic_runtime::testing::PublicCutPosition::BuildAuthorityRead);
        node.running
            .handle()
            .set_public_operation_cut(Some(cut.clone()));
        let context = node.service.context.get().unwrap().clone();
        let a = authority.clone();
        let validation = tokio::spawn(async move { context.validate_build_authority(&a).await });
        cut.entered.notified().await;
        node.running.handle().set_public_operation_cut(None);
        let mut superseding = None;
        if case == 2 {
            let program = PublicOperationProgram::Epoch {
                epoch: Epoch::new(0, 2),
            };
            superseding = Some(execute(
                &source,
                PublicOperationIntent {
                    identity: source.preview.clone(),
                    operation_id: OperationId::new("cancel-during-validation"),
                    revision: source.revision + 1,
                    process_session_id: source.session.clone(),
                    class: PublicOperationClass::Authority,
                    input_digest: program.digest(),
                    replay_disposition: program.replay_disposition(),
                    lifecycle: None,
                    program: Some(program),
                },
            ));
        } else if case == 3 {
            history(
                &target,
                PublicV1HistoryCommand::RevokeInboundBuildAuthority {
                    authority: Box::new(authority.clone()),
                },
            )
            .await
            .unwrap();
        } else {
            node.service
                .context
                .get()
                .unwrap()
                .report_fault(FaultType::Transient)
                .await
                .unwrap();
        }
        tokio::time::timeout(Duration::from_secs(5), async {
            while node
                .service
                .context
                .get()
                .unwrap()
                .validate_build_authority(&authority)
                .await
                .is_ok()
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        cut.release.notify_one();
        assert!(
            validation.await.unwrap().is_err(),
            "snapshot crossed invalidation case {case}"
        );
        assert!(source.stop().await.is_none());
        assert!(target.stop().await.is_none());
        let _ = tokio::time::timeout(Duration::from_secs(5), build)
            .await
            .unwrap()
            .unwrap();
        if let Some(superseding) = superseding {
            let _ = tokio::time::timeout(Duration::from_secs(5), superseding)
                .await
                .unwrap()
                .unwrap();
        }
    }
}

#[tokio::test]
async fn custom_host_late_authority_commit_cannot_publish_after_shutdown() {
    let root = tempfile::tempdir().unwrap();
    let (source, mut target, authority, _) = setup(root.path()).await;
    let cut = validation_cut(kuberic_runtime::testing::PublicCutPosition::BeforeInboundBuildCommit);
    target
        .running
        .handle()
        .set_public_operation_cut(Some(cut.clone()));
    let install = tokio::spawn(history(
        &target,
        PublicV1HistoryCommand::InstallInboundBuildAuthority {
            authority: Box::new(authority),
        },
    ));
    cut.entered.notified().await;
    target.running.handle().set_public_operation_cut(None);
    target.running.shutdown();
    tokio::time::timeout(Duration::from_secs(5), async {
        while target
            .service
            .context
            .get()
            .unwrap()
            .report_load(vec![])
            .await
            .is_ok()
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    cut.release.notify_one();
    assert!(install.await.unwrap().is_err());
    target.running.wait().await.unwrap();
    assert_eq!(target.service.installations.load(Ordering::SeqCst), 0);
    let path = kuberic_runtime::testing::sqlite_store::SqliteStore::metadata_database_path(
        &root.path().join("t"),
    );
    let connection =
        rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    let state: String = connection
        .query_row(
            "SELECT state_json FROM agent_state WHERE singleton=1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let state: serde_json::Value = serde_json::from_str(&state).unwrap();
    assert!(state["publicV1OperationState"]["currentInboundBuildAuthority"].is_null());
    assert_eq!(state["publicV1OperationState"]["inboundBuildGeneration"], 0);
    source.close().await;
}

#[tokio::test]
async fn custom_host_abort_or_close_unblocks_installer_before_serialized_drain() {
    for close in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let (source, mut target, authority, _) = setup(root.path()).await;
        target.service.install_hold.store(true, Ordering::SeqCst);
        let install = tokio::spawn(history(
            &target,
            PublicV1HistoryCommand::InstallInboundBuildAuthority {
                authority: Box::new(authority),
            },
        ));
        target
            .service
            .install_entered
            .acquire()
            .await
            .unwrap()
            .forget();
        assert!(target.service.alive.load(Ordering::SeqCst) > 0);
        if close {
            target.command(PublicOperationProgram::Close).await;
        }
        target.running.shutdown();
        tokio::time::timeout(Duration::from_secs(10), target.running.wait())
            .await
            .unwrap()
            .unwrap();
        assert!(
            install.await.unwrap().is_err(),
            "an installer cannot acknowledge across terminal fencing"
        );
        assert_eq!(target.service.alive.load(Ordering::SeqCst), 0);
        source.close().await;
    }
}

async fn late_factory_shutdown(mode: u8) {
    let root = tempfile::tempdir().unwrap();
    let mut source = Node::<Contract>::start_sized(root.path(), 1, 1, 1).await;
    source.service.factory_hold.store(true, Ordering::SeqCst);
    let program = PublicOperationProgram::Open {
        replica: source.identity.clone(),
        existing: false,
    };
    let intent = PublicOperationIntent {
        identity: source.preview.clone(),
        operation_id: OperationId::new("late-custom-open"),
        revision: 2,
        process_session_id: source.session.clone(),
        class: PublicOperationClass::Authority,
        input_digest: program.digest(),
        replay_disposition: program.replay_disposition(),
        lifecycle: None,
        program: Some(program),
    };
    let address = source.control_address.clone();
    let identity = source.identity.clone();
    let resource = source.history.resource_uid.clone();
    let session = source.session.clone();
    let request = tokio::spawn(async move {
        let mut client = proto::agent_control_client::AgentControlClient::connect(address)
            .await
            .unwrap();
        client
            .execute(authorized(proto::ExecuteCommandRequest {
                protocol_version: kuberic_runtime::protocol::PROTOCOL_VERSION,
                resource_uid: resource.to_string(),
                target: Some(identity.clone().into()),
                expected_process_session_id: session.to_string(),
                command: Some(proto::execute_command_request::Command::PublicV1Operation(
                    convert::public_v1_operation_command_to_proto(&PublicV1OperationCommand {
                        target: identity,
                        intent,
                    })
                    .unwrap(),
                )),
            }))
            .await
    });
    source
        .service
        .factory_entered
        .acquire()
        .await
        .unwrap()
        .forget();
    assert!(source.service.alive.load(Ordering::SeqCst) > 0);
    source.service.fail_drain.store(mode == 1, Ordering::SeqCst);
    source
        .service
        .panic_drain
        .store(mode == 2, Ordering::SeqCst);
    source.running.shutdown();
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if source
                .service
                .context
                .get()
                .unwrap()
                .report_load(vec![])
                .await
                .is_err()
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("shutdown fences host before waiting for late factory");
    source.service.factory_release.add_permits(1);
    assert!(
        tokio::time::timeout(Duration::from_secs(5), request)
            .await
            .unwrap()
            .unwrap()
            .is_err()
    );
    let result = tokio::time::timeout(Duration::from_secs(10), source.running.wait())
        .await
        .unwrap();
    if mode == 0 {
        result.unwrap();
        assert_eq!(source.service.alive.load(Ordering::SeqCst), 0);
    } else {
        assert!(
            result.is_err(),
            "unregistered creation cleanup failure must reach process completion"
        );
        assert!(source.service.alive.load(Ordering::SeqCst) > 0);
    }
    source.service.fail_drain.store(false, Ordering::SeqCst);
    source.service.panic_drain.store(false, Ordering::SeqCst);
    source.service.drain().await.unwrap();
    assert_eq!(source.service.alive.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn custom_host_shutdown_fences_late_factory_registration_and_joins_bundle() {
    late_factory_shutdown(0).await;
}

#[tokio::test]
async fn custom_host_late_factory_failed_and_lost_cleanup_are_process_errors() {
    late_factory_shutdown(1).await;
    late_factory_shutdown(2).await;
}

#[tokio::test]
async fn custom_host_returned_bundle_cannot_discard_different_retained_creation() {
    for fail in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let mut source = Node::<Contract>::start_sized(root.path(), 1, 1, 1).await;
        source
            .service
            .different_bundle
            .store(true, Ordering::SeqCst);
        source.service.fail_drain.store(fail, Ordering::SeqCst);
        let program = PublicOperationProgram::Open {
            replica: source.identity.clone(),
            existing: false,
        };
        let result = source
            .submit_result(PublicV1OperationCommand {
                target: source.identity.clone(),
                intent: PublicOperationIntent {
                    identity: source.preview.clone(),
                    operation_id: OperationId::new("wrong-bundle-open"),
                    revision: 2,
                    process_session_id: source.session.clone(),
                    class: PublicOperationClass::Authority,
                    input_digest: program.digest(),
                    replay_disposition: program.replay_disposition(),
                    lifecycle: None,
                    program: Some(program),
                },
            })
            .await;
        assert!(
            result
                .unwrap_err()
                .message()
                .contains("does not match retained creation control")
        );
        assert!(source.service.alive.load(Ordering::SeqCst) > 0);
        source.running.shutdown();
        let result = tokio::time::timeout(Duration::from_secs(10), source.running.wait())
            .await
            .unwrap();
        if fail {
            assert!(result.is_err());
            assert!(source.service.alive.load(Ordering::SeqCst) > 0);
        } else {
            result.unwrap();
            assert_eq!(source.service.alive.load(Ordering::SeqCst), 0);
        }
        source.service.fail_drain.store(false, Ordering::SeqCst);
        source.service.drain().await.unwrap();
        assert_eq!(source.service.alive.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn custom_host_unavailable_idle_diagnostic_preserves_installed_authority_and_exact_build() {
    use kuberic_runtime::protocol::public_operations::PublicV1RemoteBuildState;
    let root = tempfile::tempdir().unwrap();
    let (mut source, target, authority, intent) = setup(root.path()).await;
    let installed = source.report().await.configuration.unwrap();
    source.service.observation.store(1, Ordering::SeqCst);
    let before = source.report().await;
    assert_eq!(before.configuration.as_ref(), Some(&installed));
    assert!(
        before.engine_idle.is_none(),
        "unavailable is not observed empty"
    );
    assert!(before.remote_builds.is_empty());
    history(
        &target,
        PublicV1HistoryCommand::InstallInboundBuildAuthority {
            authority: Box::new(authority.clone()),
        },
    )
    .await
    .unwrap();
    source
        .submit(PublicV1OperationCommand {
            target: source.identity.clone(),
            intent: intent.clone(),
        })
        .await;
    let after = source.report().await;
    assert_eq!(after.configuration.as_ref(), Some(&installed));
    assert!(
        after.engine_idle.is_none(),
        "Build success cannot synthesize a sample"
    );
    let remote = after
        .remote_builds
        .iter()
        .find(|remote| remote.operation_id == intent.operation_id)
        .unwrap();
    assert_eq!(remote.revision, intent.revision);
    assert_eq!(
        Some(PublicOperationProgram::Build(remote.build.clone())),
        intent.program
    );
    assert_eq!(remote.state, PublicV1RemoteBuildState::Ready);
    assert!(
        !installed
            .voting
            .current
            .members
            .iter()
            .any(|member| { member.identity == target.identity }),
        "exact Build success is not voting admission"
    );
    let target_report = target.report().await;
    assert_eq!(target_report.role, ReplicaRole::IdleSecondary);
    assert_ne!(target_report.write_status, AccessStatus::Granted);
    history(
        &target,
        PublicV1HistoryCommand::RevokeInboundBuildAuthority {
            authority: Box::new(authority),
        },
    )
    .await
    .unwrap();
    source.service.observation.store(0, Ordering::SeqCst);
    assert_eq!(
        source.report().await.engine_idle.unwrap().idle_replicas,
        vec![target.identity.replica_id]
    );
    source.close().await;
    target.close().await;
}

#[tokio::test]
async fn custom_host_exact_binding_reporting_revoke_and_receiver_barrier() {
    let root = tempfile::tempdir().unwrap();
    let (mut source, target, authority, intent) = setup(root.path()).await;
    assert!(
        source
            .service
            .context
            .get()
            .unwrap()
            .outbound_build_authority(&authority.operation_id)
            .await
            .is_err()
    );
    assert!(
        target
            .service
            .context
            .get()
            .unwrap()
            .validate_build_authority(&authority)
            .await
            .is_err()
    );
    assert_eq!(
        source.report().await.engine_idle.unwrap().idle_replicas,
        vec![]
    );
    for mode in 1..=5 {
        source.service.observation.store(mode, Ordering::SeqCst);
        let report = source.report().await;
        assert!(
            report.configuration.is_some(),
            "host install lost in mode {mode}"
        );
        assert!(report.engine_idle.is_none(), "mode {mode}");
    }
    source.service.observation.store(0, Ordering::SeqCst);
    history(
        &target,
        PublicV1HistoryCommand::InstallInboundBuildAuthority {
            authority: Box::new(authority.clone()),
        },
    )
    .await
    .unwrap();
    history(
        &target,
        PublicV1HistoryCommand::InstallInboundBuildAuthority {
            authority: Box::new(authority.clone()),
        },
    )
    .await
    .unwrap();
    assert_eq!(target.service.installations.load(Ordering::SeqCst), 1);
    let encoded = serde_json::to_value(&authority).unwrap();
    fn mutate_leaves(value: &serde_json::Value, path: Vec<String>, out: &mut Vec<Vec<String>>) {
        if let Some(object) = value.as_object() {
            for (k, v) in object {
                let mut p = path.clone();
                p.push(k.clone());
                mutate_leaves(v, p, out);
            }
        } else {
            out.push(path);
        }
    }
    let mut paths = vec![];
    mutate_leaves(&encoded, vec![], &mut paths);
    for path in paths {
        let mut changed = encoded.clone();
        let mut leaf = &mut changed;
        for key in &path {
            leaf = &mut leaf[key];
        }
        *leaf = match leaf {
            serde_json::Value::String(s) => serde_json::Value::String(format!("{s}-changed")),
            serde_json::Value::Number(n) => serde_json::json!(n.as_i64().unwrap() + 1),
            _ => continue,
        };
        if let Ok(changed) = serde_json::from_value::<PublicInboundBuildAuthority>(changed) {
            assert!(
                target
                    .service
                    .context
                    .get()
                    .unwrap()
                    .validate_build_authority(&changed)
                    .await
                    .is_err(),
                "{path:?}"
            );
        }
    }
    let mut wrong_digest = intent.clone();
    wrong_digest.input_digest = PublicOperationProgram::Close.digest();
    assert!(
        convert::public_v1_operation_command_to_proto(&PublicV1OperationCommand {
            target: source.identity.clone(),
            intent: wrong_digest,
        })
        .is_err()
    );
    source
        .submit(PublicV1OperationCommand {
            target: source.identity.clone(),
            intent: intent.clone(),
        })
        .await;
    let mut changed = intent.clone();
    if let Some(PublicOperationProgram::Build(input)) = changed.program.as_mut() {
        input.replication_address.push_str("-different");
    }
    changed.input_digest = changed.program.as_ref().unwrap().digest();
    assert!(
        source
            .submit_result(PublicV1OperationCommand {
                target: source.identity.clone(),
                intent: changed,
            })
            .await
            .is_err()
    );
    assert_eq!(
        source.report().await.engine_idle.unwrap().idle_replicas,
        vec![target.identity.replica_id]
    );
    assert!(
        source
            .service
            .context
            .get()
            .unwrap()
            .validate_build_authority(&authority)
            .await
            .is_err(),
        "completed source operation is historical, not a live grant"
    );
    let mut stale = authority.clone();
    stale.operation_revision += 1;
    history(
        &target,
        PublicV1HistoryCommand::RevokeInboundBuildAuthority {
            authority: Box::new(stale.clone()),
        },
    )
    .await
    .unwrap();
    target
        .service
        .context
        .get()
        .unwrap()
        .validate_build_authority(&authority)
        .await
        .unwrap();
    history(
        &target,
        PublicV1HistoryCommand::RevokeInboundBuildAuthority {
            authority: Box::new(authority.clone()),
        },
    )
    .await
    .unwrap();
    assert!(target.service.alive.load(Ordering::SeqCst) > 0);
    assert!(
        target
            .service
            .context
            .get()
            .unwrap()
            .validate_build_authority(&authority)
            .await
            .is_err()
    );
    stale.attempt.build_id = OperationId::new("successor");
    stale.operation_id = OperationId::new("successor");
    assert!(
        history(
            &target,
            PublicV1HistoryCommand::InstallInboundBuildAuthority {
                authority: Box::new(stale)
            }
        )
        .await
        .is_err()
    );
    assert_eq!(target.service.installations.load(Ordering::SeqCst), 1);
    assert!(
        history(
            &target,
            PublicV1HistoryCommand::InstallInboundBuildAuthority {
                authority: Box::new(authority)
            }
        )
        .await
        .is_err()
    );
    // The fixture has genuinely retained a built-idle peer. Installing that
    // peer as a voter removes it from its actual fixture membership owner.
    let full = ConfigurationDescriptor::new(
        Epoch::new(0, 1),
        source.identity.replica_id,
        vec![
            ConfigurationMember {
                identity: source.identity.clone(),
                role: ReplicaRole::Primary,
            },
            ConfigurationMember {
                identity: target.identity.clone(),
                role: ReplicaRole::ActiveSecondary,
            },
        ],
        2,
    );
    source
        .command(PublicOperationProgram::Configuration(PublicConfiguration {
            current: full,
            previous: None,
            replicas: vec![PublicReplicaDescriptor {
                is_up: true,
                is_ready: true,
                build_id: OperationId::default(),
                identity: target.identity.clone(),
                replication_address: target.service.endpoint.clone(),
                process_session_id: target.session.clone(),
                storage_incarnation: Some(target.storage.clone()),
                role: ReplicaRole::ActiveSecondary,
                current_progress: -1,
                catch_up_capability: -1,
                must_catchup: false,
            }],
        }))
        .await;
    assert!(
        source
            .report()
            .await
            .engine_idle
            .unwrap()
            .idle_replicas
            .is_empty()
    );
    source.close().await;
    target.close().await;
}

#[tokio::test]
async fn custom_host_late_observation_after_completed_terminal_is_discarded() {
    let root = tempfile::tempdir().unwrap();
    let (mut source, target, _, _) = setup(root.path()).await;
    while let Ok(permit) = source.service.observe_entered.try_acquire() {
        permit.forget();
    }
    source.service.observe_hold.store(true, Ordering::SeqCst);
    let address = source.control_address.clone();
    let resource = source.history.resource_uid.clone();
    let identity = source.identity.clone();
    let report = tokio::spawn(async move { report_at(&address, &resource, &identity).await });
    source
        .service
        .observe_entered
        .acquire()
        .await
        .unwrap()
        .forget();
    source.command(PublicOperationProgram::Close).await;
    source.service.observe_release.add_permits(1);
    let report = report.await.unwrap();
    assert!(report.engine_idle.is_none());
    assert!(report.configuration.is_some());
    source.stop().await;
    target.close().await;
}

#[tokio::test]
async fn custom_host_nonoverlapping_torn_configuration_sample_is_discarded() {
    let root = tempfile::tempdir().unwrap();
    let (mut source, target, _, _) = setup(root.path()).await;
    while let Ok(permit) = source.service.observe_entered.try_acquire() {
        permit.forget();
    }
    source.service.observe_hold.store(true, Ordering::SeqCst);
    let address = source.control_address.clone();
    let resource = source.history.resource_uid.clone();
    let identity = source.identity.clone();
    let report = tokio::spawn(async move { report_at(&address, &resource, &identity).await });
    source
        .service
        .observe_entered
        .acquire()
        .await
        .unwrap()
        .forget();
    source
        .command(PublicOperationProgram::Epoch {
            epoch: Epoch::new(0, 2),
        })
        .await;
    source.single(Epoch::new(0, 2)).await;
    source.service.observe_release.add_permits(1);
    let report = report.await.unwrap();
    assert!(report.engine_idle.is_none());
    assert_eq!(
        report.configuration.unwrap().voting.current.epoch,
        Epoch::new(0, 2)
    );
    source.service.observe_hold.store(false, Ordering::SeqCst);
    assert_eq!(
        source
            .report()
            .await
            .configuration
            .unwrap()
            .voting
            .current
            .epoch,
        Epoch::new(0, 2)
    );
    source.close().await;
    target.close().await;
}

#[tokio::test]
async fn custom_host_terminal_failed_or_lost_drain_does_not_release_owned_child() {
    for lost in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let (mut source, target, authority, intent) = setup(root.path()).await;
        source.service.hold_build.store(true, Ordering::SeqCst);
        history(
            &target,
            PublicV1HistoryCommand::InstallInboundBuildAuthority {
                authority: Box::new(authority),
            },
        )
        .await
        .unwrap();
        let execute = |intent: PublicOperationIntent| {
            let address = source.control_address.clone();
            let identity = source.identity.clone();
            let resource = source.history.resource_uid.clone();
            let session = source.session.clone();
            tokio::spawn(async move {
                let mut client = proto::agent_control_client::AgentControlClient::connect(address)
                    .await
                    .unwrap();
                client
                    .execute(authorized(proto::ExecuteCommandRequest {
                        protocol_version: kuberic_runtime::protocol::PROTOCOL_VERSION,
                        resource_uid: resource.to_string(),
                        target: Some(identity.clone().into()),
                        expected_process_session_id: session.to_string(),
                        command: Some(proto::execute_command_request::Command::PublicV1Operation(
                            convert::public_v1_operation_command_to_proto(
                                &PublicV1OperationCommand {
                                    target: identity,
                                    intent,
                                },
                            )
                            .unwrap(),
                        )),
                    }))
                    .await
            })
        };
        let build = execute(intent);
        source
            .service
            .build_entered
            .acquire()
            .await
            .unwrap()
            .forget();
        source.service.fail_drain.store(!lost, Ordering::SeqCst);
        source.service.panic_drain.store(lost, Ordering::SeqCst);
        let program = PublicOperationProgram::Close;
        let terminal_id = OperationId::new("failed-custom-terminal");
        let close = execute(PublicOperationIntent {
            identity: source.preview.clone(),
            operation_id: terminal_id.clone(),
            revision: source.revision + 1,
            process_session_id: source.session.clone(),
            class: PublicOperationClass::Close,
            input_digest: program.digest(),
            replay_disposition: program.replay_disposition(),
            lifecycle: None,
            program: Some(program),
        });
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if source
                    .service
                    .context
                    .get()
                    .unwrap()
                    .write_status()
                    .await
                    .unwrap()
                    != AccessStatus::Granted
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(source.service.alive.load(Ordering::SeqCst) > 0);
        let report = source.report().await;
        assert!(
            !report
                .completed_operations
                .iter()
                .any(|completion| completion.operation_id == terminal_id
                    && matches!(completion.result, PublicV1OperationResult::Succeeded))
        );
        let close = if close.is_finished() {
            assert!(
                close.await.unwrap().is_err(),
                "failed drainage cannot complete terminal success"
            );
            None
        } else {
            Some(close)
        };
        let service = source.service.clone();
        source.service.fail_drain.store(false, Ordering::SeqCst);
        source.service.panic_drain.store(false, Ordering::SeqCst);
        source.running.shutdown();
        assert!(
            source.running.wait().await.is_err(),
            "actual drain failure must survive a later process observer"
        );
        source.service.drain().await.unwrap();
        assert_eq!(service.alive.load(Ordering::SeqCst), 0);
        if let Some(close) = close {
            let _ = tokio::time::timeout(Duration::from_secs(5), close)
                .await
                .unwrap()
                .unwrap();
        }
        let _ = tokio::time::timeout(Duration::from_secs(5), build)
            .await
            .unwrap()
            .unwrap();
        target.close().await;
    }
}

#[tokio::test]
async fn custom_host_partial_install_failed_or_lost_drain_retains_real_child() {
    for lost in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let (source, mut target, authority, _) = setup(root.path()).await;
        target.service.fail_install.store(true, Ordering::SeqCst);
        target.service.fail_drain.store(!lost, Ordering::SeqCst);
        target.service.panic_drain.store(lost, Ordering::SeqCst);
        assert!(
            history(
                &target,
                PublicV1HistoryCommand::InstallInboundBuildAuthority {
                    authority: Box::new(authority.clone())
                }
            )
            .await
            .is_err()
        );
        assert!(target.service.alive.load(Ordering::SeqCst) > 0);
        assert!(
            target
                .service
                .context
                .get()
                .unwrap()
                .validate_build_authority(&authority)
                .await
                .is_err()
        );
        let mut successor = authority;
        successor.operation_revision += 1;
        successor.operation_id = OperationId::new("unsafe-successor");
        successor.attempt.build_id = OperationId::new("unsafe-successor");
        assert!(
            history(
                &target,
                PublicV1HistoryCommand::InstallInboundBuildAuthority {
                    authority: Box::new(successor)
                }
            )
            .await
            .is_err()
        );
        assert_eq!(target.service.installations.load(Ordering::SeqCst), 1);
        let service = target.service.clone();
        target.service.fail_drain.store(false, Ordering::SeqCst);
        target.service.panic_drain.store(false, Ordering::SeqCst);
        target.running.shutdown();
        let shutdown = target.running.wait().await;
        assert!(
            shutdown.is_err(),
            "failed or lost actual drainage remains sticky at process completion"
        );
        target.service.drain().await.unwrap();
        assert_eq!(service.alive.load(Ordering::SeqCst), 0);
        source.close().await;
    }
}

#[tokio::test]
async fn custom_host_close_and_abort_preempt_active_owned_callback() {
    for terminal in [PublicOperationProgram::Close, PublicOperationProgram::Abort] {
        let root = tempfile::tempdir().unwrap();
        let (mut source, target, authority, intent) = setup(root.path()).await;
        source.service.hold_build.store(true, Ordering::SeqCst);
        history(
            &target,
            PublicV1HistoryCommand::InstallInboundBuildAuthority {
                authority: Box::new(authority),
            },
        )
        .await
        .unwrap();
        let service = source.service.clone();
        let address = source.control_address.clone();
        let identity = source.identity.clone();
        let resource = source.history.resource_uid.clone();
        let session = source.session.clone();
        let observer = tokio::spawn(async move {
            let mut client = proto::agent_control_client::AgentControlClient::connect(address)
                .await
                .unwrap();
            client
                .execute(authorized(proto::ExecuteCommandRequest {
                    protocol_version: kuberic_runtime::protocol::PROTOCOL_VERSION,
                    resource_uid: resource.to_string(),
                    target: Some(identity.clone().into()),
                    expected_process_session_id: session.to_string(),
                    command: Some(proto::execute_command_request::Command::PublicV1Operation(
                        convert::public_v1_operation_command_to_proto(&PublicV1OperationCommand {
                            target: identity,
                            intent,
                        })
                        .unwrap(),
                    )),
                }))
                .await
        });
        service.build_entered.acquire().await.unwrap().forget();
        assert!(service.alive.load(Ordering::SeqCst) > 0);
        source.command(terminal).await;
        assert_eq!(service.alive.load(Ordering::SeqCst), 0);
        assert_ne!(source.report().await.write_status, AccessStatus::Granted);
        let _ = observer.await.unwrap();
        source.stop().await;
        target.close().await;
    }
}
