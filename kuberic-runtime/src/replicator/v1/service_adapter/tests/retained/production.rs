use super::super::super::engine_session::{Binding, Control, ControlEnvelope, Endpoint};
use super::super::super::production::test_compose;
use super::super::super::production_transport::Transport;
use super::*;
use crate::protocol::public_operations::*;
use crate::protocol::types::*;
use crate::replicator::{
    CheckedCreationReconstruction, CheckedRetainedMember, PartitionAccessView,
    PartitionInformation, ReplicaInformation, ReplicaSetConfiguration,
    ReplicatorCreationReservation, ReplicatorFactoryContext, ReplicatorInterfaces,
    RuntimeHostToken, TestingTcpRoute, TestingTcpRouteKind,
};

// These are checked-host fixtures, not evidence of OS exclusion or durable journal recovery.
struct Access {
    publication: Mutex<ReplicaPublication>,
    selected: Mutex<Option<CheckedRetainedMember>>,
    initialized: Mutex<Option<CheckedRetainedMember>>,
    reconstruction: Mutex<Option<CheckedCreationReconstruction>>,
    reconstructed: Mutex<Option<CheckedCreationReconstruction>>,
    confirmations: AtomicUsize,
    fail_confirmation: AtomicBool,
    withdrawn: AtomicBool,
}

#[async_trait]
impl PartitionAccessView for Access {
    async fn creation_reconstruction(
        &self,
        _: Option<ReplicatorCreationReservation>,
    ) -> crate::Result<Option<CheckedCreationReconstruction>> {
        if self.withdrawn.load(Ordering::SeqCst) {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        Ok(self.reconstruction.lock().unwrap().clone())
    }

    async fn confirm_reconstructed_native_initialization(
        &self,
        _: ReplicatorCreationReservation,
        selected: &CheckedCreationReconstruction,
    ) -> crate::Result<()> {
        self.confirmations.fetch_add(1, Ordering::SeqCst);
        if self.fail_confirmation.load(Ordering::SeqCst)
            || self.reconstruction.lock().unwrap().as_ref() != Some(selected)
        {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        *self.reconstructed.lock().unwrap() = Some(selected.clone());
        Ok(())
    }

    fn partition_information(&self) -> PartitionInformation {
        PartitionInformation {
            partition_id: PartitionId::new("retained-adapter"),
        }
    }
    async fn read_status(&self) -> crate::Result<AccessStatus> {
        Ok(AccessStatus::NotPrimary)
    }
    async fn write_status(&self) -> crate::Result<AccessStatus> {
        Ok(AccessStatus::NotPrimary)
    }
    fn native_write_status(&self) -> AccessStatus {
        AccessStatus::Granted
    }
    fn native_write_admission(&self) -> crate::Result<Arc<Mutex<()>>> {
        Ok(Arc::new(Mutex::new(())))
    }
    async fn native_credentials(&self) -> crate::Result<Arc<str>> {
        Ok(Arc::from("test-token"))
    }
    async fn local_publication(
        &self,
        _: ReplicatorCreationReservation,
    ) -> crate::Result<Option<ReplicaPublication>> {
        Ok((!self.withdrawn.load(Ordering::SeqCst))
            .then(|| self.publication.lock().unwrap().clone()))
    }
    async fn retained_member_restore(
        &self,
        _: ReplicatorCreationReservation,
    ) -> crate::Result<Option<CheckedRetainedMember>> {
        if self.withdrawn.load(Ordering::SeqCst) {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        Ok(self.selected.lock().unwrap().clone())
    }
    async fn confirm_retained_native_initialization(
        &self,
        _: ReplicatorCreationReservation,
        selected: &CheckedRetainedMember,
    ) -> crate::Result<()> {
        self.confirmations.fetch_add(1, Ordering::SeqCst);
        if self.fail_confirmation.load(Ordering::SeqCst)
            || self.selected.lock().unwrap().as_ref() != Some(selected)
        {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        *self.initialized.lock().unwrap() = Some(selected.clone());
        Ok(())
    }
    async fn report_load(&self, _: Vec<LoadMetric>) -> crate::Result<()> {
        Ok(())
    }
    async fn report_fault(&self, _: FaultType) -> crate::Result<()> {
        Ok(())
    }
}

fn identity(id: i64) -> ReplicaIdentity {
    ReplicaIdentity {
        replica_id: ReplicaId::new(id),
        instance_id: ReplicaInstanceId::new(format!("retained-{id}")),
        agent_generation: AgentGeneration::new(format!("retained-agent-{id}")),
    }
}

fn configuration(primary: i64, members: &[i64], epoch: Epoch) -> ConfigurationDescriptor {
    ConfigurationDescriptor::new(
        epoch,
        ReplicaId::new(primary),
        members
            .iter()
            .map(|id| ConfigurationMember {
                identity: identity(*id),
                role: if *id == primary {
                    ReplicaRole::Primary
                } else {
                    ReplicaRole::ActiveSecondary
                },
            })
            .collect(),
        2,
    )
}

fn publication(id: i64, address: String) -> ReplicaPublication {
    let resource = ResourceUid::new("retained-adapter");
    let initialization =
        derive_history_initialization_id(&resource, PublicProtocolGeneration::new(1));
    let mut history =
        derive_bootstrap_history(&resource, PublicProtocolGeneration::new(1), &initialization);
    history.data_loss_number = 1;
    ReplicaPublication {
        format: ORDINARY_ACTION_FORMAT,
        scope: ReplicaScope {
            history,
            storage: ReplicaStorageIncarnationContext {
                replica: identity(id),
                storage: ReplicaStorageIdentity::Persistent {
                    pod_uid: PodUid::new(format!("retained-pod-{id}")),
                    pvc_uid: PvcUid::new(format!("retained-pvc-{id}")),
                },
                storage_generation: StorageGeneration::new(format!("retained-storage-{id}")),
            },
            process_session_id: ProcessSessionId::new(format!("fresh-process-{id}")),
            creation_id: ReplicatorCreationId::new(format!("fresh-creation-{id}")),
            epoch: epoch(1),
            configuration: ReplicaConfigurationScope {
                current: configuration(1, &[1, 2, 3], epoch(1)),
                previous: None,
                replicas: Vec::new(),
            },
        },
        replication_address: address,
        generation: 1,
        stage: ReplicaPublicationStage::DiscoveryPending,
        role: ReplicaRole::None,
    }
}

fn information(
    publication: &ReplicaPublication,
    role: ReplicaRole,
    progress: Lsn,
) -> ReplicaInformation {
    ReplicaInformation {
        identity: publication.scope.storage.replica.clone(),
        process_session_id: publication.scope.process_session_id.clone(),
        storage_incarnation: Some(publication.scope.storage.clone()),
        replication_address: publication.replication_address.clone(),
        role,
        current_progress: progress,
        catch_up_capability: 0,
        must_catchup: false,
    }
}

fn peer(
    publication: &ReplicaPublication,
    role: ReplicaRole,
    progress: Lsn,
) -> ReplicaPeerDescription {
    let info = information(publication, role, progress);
    ReplicaPeerDescription {
        identity: info.identity,
        process_session_id: info.process_session_id,
        storage_incarnation: info.storage_incarnation.unwrap(),
        replication_address: info.replication_address,
        role,
        current_progress: progress,
        catch_up_capability: 0,
        must_catchup: false,
        is_up: true,
        is_ready: true,
    }
}

struct Member {
    interfaces: ReplicatorInterfaces,
    transport: Arc<Transport>,
    access: Arc<Access>,
    provider: Arc<Provider>,
    checked: Option<CheckedRetainedMember>,
}

impl Member {
    async fn new(id: i64, retained: bool, progress: Lsn) -> Self {
        Self::new_selected(id, retained, progress, false).await
    }

    async fn new_selected(id: i64, retained: bool, progress: Lsn, reconstruct: bool) -> Self {
        Self::new_scoped(id, retained, progress, reconstruct, None, None).await
    }

    async fn new_scoped(
        id: i64,
        retained: bool,
        progress: Lsn,
        reconstruct: bool,
        predecessor: Option<ReplicaPublication>,
        retained_provider: Option<Arc<Provider>>,
    ) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut publication = publication(id, format!("http://{}", listener.local_addr().unwrap()));
        if let Some(old) = &predecessor {
            publication.scope = old.scope.clone();
            publication.scope.process_session_id =
                ProcessSessionId::new(format!("reconstructed-process-{id}"));
        }
        let reservation = ReplicatorCreationReservation::new(RuntimeHostToken::new());
        publication.scope.creation_id =
            reservation.identity(RuntimeHostToken::new()).to_public_id();
        if reconstruct {
            publication.scope.configuration.current = ConfigurationDescriptor::new(
                epoch(1),
                ReplicaId::new(id),
                vec![ConfigurationMember {
                    identity: identity(id),
                    role: ReplicaRole::Primary,
                }],
                1,
            );
        }
        let checked = retained.then(|| {
            CheckedRetainedMember::new(
                publication.scope.clone(),
                RetainedMemberQualification {
                    predecessor_process_session_id: predecessor
                        .as_ref()
                        .map(|old| old.scope.process_session_id.clone())
                        .unwrap_or_else(|| ProcessSessionId::new("excluded-process")),
                    predecessor_creation_id: predecessor
                        .as_ref()
                        .map(|old| old.scope.creation_id.clone())
                        .unwrap_or_else(|| ReplicatorCreationId::new("excluded-creation")),
                    admitted_operation_id: OperationId::new("historical-admission"),
                    admitted_revision: 1,
                    configuration: publication.scope.configuration.current.clone(),
                },
            )
        });
        let access = Arc::new(Access {
            publication: Mutex::new(publication.clone()),
            selected: Mutex::new(checked.clone()),
            initialized: Mutex::new(None),
            reconstruction: Mutex::new(reconstruct.then(|| {
                CheckedCreationReconstruction::new(ReplicaGoal {
                    format: ORDINARY_ACTION_FORMAT,
                    identity: PublicV1Identity::new(1),
                    operation_id: OperationId::new("checked-native-reconstruction"),
                    revision: 2,
                    scope: publication.scope.clone(),
                    desired: ReplicaGoalKind::RecoverContainedCreation {
                        recovery: Box::new(ContainedCreationRecovery {
                            addition: None,
                            predecessor_process_session_id: ProcessSessionId::new(
                                "excluded-process",
                            ),
                            predecessor_creation_id: ReplicatorCreationId::new("excluded-creation"),
                            predecessor_operations: vec![OperationId::new("old-native-effect")],
                            receiver_settlements: Vec::new(),
                            role: ReplicaRole::Primary,
                            catch_up: Some(PublicCatchUpMode::All),
                        }),
                    },
                })
            })),
            reconstructed: Mutex::new(None),
            confirmations: AtomicUsize::new(0),
            fail_confirmation: AtomicBool::new(false),
            withdrawn: AtomicBool::new(false),
        });
        let context = ReplicatorFactoryContext::new(
            RuntimeHostToken::new(),
            identity(id),
            publication.scope.process_session_id.clone(),
            access.clone(),
            PublicV1LifecycleBinding {
                identity: PublicV1Identity::new(1),
                resource_uid: publication.scope.history.resource_uid.clone(),
                spec_generation: 1,
                state_persistence: StatePersistence::Persisted,
            },
            "127.0.0.1:0".parse().unwrap(),
        )
        .for_creation(reservation)
        .with_ordinary_contract(true);
        let registry = MessageRegistry::default();
        let transport = Transport::test_owner(
            context.clone(),
            Endpoint::publication(&publication),
            publication.scope.history.clone(),
            Vec::new(),
            registry.clone(),
        );
        transport.test_listen(listener);
        let provider = match retained_provider {
            Some(provider) => {
                assert_eq!(provider.progress.load(Ordering::SeqCst), progress);
                provider
            }
            None => {
                let provider = Arc::new(Provider::new());
                provider.progress.store(progress, Ordering::SeqCst);
                provider
            }
        };
        let interfaces = test_compose(
            context.clone(),
            provider.clone(),
            registry,
            transport.clone(),
        )
        .await
        .unwrap();
        // Exercise the real creation attachment. Only the host qualification/registry are fixtures.
        context
            .builtin_factory_created
            .set(Arc::downgrade(&interfaces.creation))
            .unwrap();
        let attachment = interfaces.prepare_attachment(reservation).unwrap();
        attachment.disarm();
        context
            .builtin_registered
            .set(publication.scope.creation_id)
            .unwrap();
        Self {
            interfaces,
            transport,
            access,
            provider,
            checked,
        }
    }
}

#[tokio::test]
async fn reconstructed_primary_native_bundle_uses_actual_checkpoint_and_keeps_publication_pending()
{
    let member = Member::new_selected(1, false, 7, true).await;
    member.interfaces.replicator().open().await.unwrap();
    member
        .interfaces
        .replicator()
        .change_role(epoch(1), ReplicaRole::Primary)
        .await
        .unwrap();
    assert_eq!(
        member
            .interfaces
            .replicator()
            .current_progress()
            .await
            .unwrap(),
        7
    );
    assert_eq!(member.provider.progress_calls.load(Ordering::SeqCst), 1);
    assert_eq!(*member.provider.epochs.lock().unwrap(), vec![(epoch(1), 7)]);
    assert_eq!(member.access.confirmations.load(Ordering::SeqCst), 1);
    assert_eq!(
        *member.access.reconstructed.lock().unwrap(),
        *member.access.reconstruction.lock().unwrap()
    );
    assert_eq!(
        member.access.publication.lock().unwrap().stage,
        ReplicaPublicationStage::DiscoveryPending
    );
    assert_eq!(
        member.access.write_status().await.unwrap(),
        AccessStatus::NotPrimary
    );
    member.finish().await;
}

#[tokio::test]
async fn reconstructed_primary_rechecks_private_context_before_effect_and_after_native_callbacks() {
    for before_effect in [true, false] {
        let member = Member::new_selected(1, false, 7, true).await;
        member.interfaces.replicator().open().await.unwrap();
        if before_effect {
            member.access.withdrawn.store(true, Ordering::SeqCst);
        } else {
            member
                .access
                .fail_confirmation
                .store(true, Ordering::SeqCst);
        }
        assert!(
            member
                .interfaces
                .replicator()
                .change_role(epoch(1), ReplicaRole::Primary)
                .await
                .is_err()
        );
        assert!(member.access.reconstructed.lock().unwrap().is_none());
        assert_eq!(
            member.provider.progress_calls.load(Ordering::SeqCst),
            usize::from(!before_effect)
        );
        assert_eq!(
            member.access.publication.lock().unwrap().stage,
            ReplicaPublicationStage::DiscoveryPending
        );
        member.finish().await;
    }
}

impl Member {
    async fn restore(&self) {
        self.interfaces.replicator().open().await.unwrap();
        self.interfaces
            .replicator()
            .change_role(epoch(1), ReplicaRole::ActiveSecondary)
            .await
            .unwrap();
    }

    async fn finish(self) {
        self.interfaces.replicator().abort();
        let _ = self.interfaces.replicator().close().await;
        self.transport.drain().await.unwrap();
    }
}

#[tokio::test]
async fn production_retained_confirmation_follows_real_initialization_and_omits_copy() {
    let member = Member::new(2, true, 7).await;
    member.provider.epoch_gate.forget_permits(100);
    member.interfaces.replicator().open().await.unwrap();
    let control = member.interfaces.replicator();
    let role = tokio::spawn(async move {
        control
            .change_role(epoch(1), ReplicaRole::ActiveSecondary)
            .await
    });
    until(|| !member.provider.epochs.lock().unwrap().is_empty()).await;
    assert_eq!(member.access.confirmations.load(Ordering::SeqCst), 0);
    assert!(member.access.initialized.lock().unwrap().is_none());
    assert!(member.transport.sessions.inbound().is_none());
    member.provider.epoch_gate.add_permits(1);
    role.await.unwrap().unwrap();
    assert_eq!(
        member.access.initialized.lock().unwrap().as_ref(),
        member.checked.as_ref()
    );
    assert_eq!(member.access.confirmations.load(Ordering::SeqCst), 1);
    assert_eq!(
        member
            .interfaces
            .replicator()
            .current_progress()
            .await
            .unwrap(),
        7
    );
    assert_eq!(
        member
            .interfaces
            .replicator()
            .catch_up_capability()
            .await
            .unwrap(),
        0
    );
    let state = member.interfaces.state_replicator().unwrap();
    assert!(
        state
            .copy_operations()
            .unwrap()
            .get_operation()
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        state
            .replication_operations()
            .unwrap()
            .get_operation()
            .now_or_never()
            .is_none()
    );
    assert_eq!(member.provider.copy_calls.load(Ordering::SeqCst), 0);
    assert_eq!(member.provider.context_calls.load(Ordering::SeqCst), 0);
    assert!(member.transport.sessions.vacant_receiver());
    member
        .interfaces
        .replicator()
        .update_epoch(epoch(1))
        .await
        .unwrap();
    assert!(
        member.transport.sessions.vacant_receiver(),
        "no unchanged Primary recovery dependency"
    );
    member.finish().await;
}

#[tokio::test]
async fn production_retained_failure_and_scope_change_never_confirm_or_publish_native_role() {
    for failure in ["provider", "scope", "confirmation"] {
        let member = Member::new(2, true, 7).await;
        member.interfaces.replicator().open().await.unwrap();
        member.provider.epoch_gate.forget_permits(100);
        let control = member.interfaces.replicator();
        let role = tokio::spawn(async move {
            control
                .change_role(epoch(1), ReplicaRole::ActiveSecondary)
                .await
        });
        until(|| !member.provider.epochs.lock().unwrap().is_empty()).await;
        match failure {
            "provider" => member.provider.fail_epoch.store(true, Ordering::SeqCst),
            "scope" => {
                let old = member.checked.as_ref().unwrap();
                let mut scope = old.scope().clone();
                scope.creation_id = ReplicatorCreationId::new("crossed-creation");
                *member.access.selected.lock().unwrap() = Some(CheckedRetainedMember::new(
                    scope,
                    old.qualification().clone(),
                ));
            }
            _ => member
                .access
                .fail_confirmation
                .store(true, Ordering::SeqCst),
        }
        member.provider.epoch_gate.add_permits(1);
        assert!(role.await.unwrap().is_err());
        assert!(member.access.initialized.lock().unwrap().is_none());
        assert!(member.transport.selected_lifecycle().is_none());
        assert!(member.transport.sessions.vacant_receiver());
        assert!(
            member
                .transport
                .test_production()
                .requires_retained_reformation()
        );
        member.finish().await;
    }
}

#[tokio::test]
async fn production_retained_confirmation_retry_does_not_reinitialize_native_history() {
    let member = Member::new(2, true, 7).await;
    member
        .access
        .fail_confirmation
        .store(true, Ordering::SeqCst);
    member.interfaces.replicator().open().await.unwrap();
    assert!(
        member
            .interfaces
            .replicator()
            .change_role(epoch(1), ReplicaRole::ActiveSecondary)
            .await
            .is_err()
    );
    assert!(member.access.initialized.lock().unwrap().is_none());
    assert!(member.transport.selected_lifecycle().is_none());
    member
        .access
        .fail_confirmation
        .store(false, Ordering::SeqCst);
    member
        .interfaces
        .replicator()
        .change_role(epoch(1), ReplicaRole::ActiveSecondary)
        .await
        .unwrap();
    assert_eq!(
        member.access.initialized.lock().unwrap().as_ref(),
        member.checked.as_ref()
    );
    assert_eq!(member.provider.progress_calls.load(Ordering::SeqCst), 1);
    assert_eq!(*member.provider.epochs.lock().unwrap(), vec![(epoch(1), 7)]);
    assert!(member.transport.sessions.vacant_receiver());
    member.finish().await;
}

#[tokio::test]
async fn production_restored_unbound_owner_drains_real_local_role_eos_without_a_nonce() {
    let member = Member::new(2, true, 7).await;
    member.restore().await;
    let mut stream = member
        .interfaces
        .state_replicator()
        .unwrap()
        .replication_operations()
        .unwrap();
    let control = member.interfaces.replicator();
    let role =
        tokio::spawn(async move { control.change_role(epoch(2), ReplicaRole::Primary).await });
    let eos = tokio::time::timeout(Duration::from_secs(5), stream.get_operation())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(eos.end_of_stream);
    assert_eq!(eos.lsn, 0);
    assert_eq!(eos.epoch, Epoch::new(0, 0));
    assert!(eos.data.is_empty());
    assert!(!role.is_finished());
    assert!(member.transport.sessions.vacant_receiver());
    eos.acknowledge().unwrap();
    assert!(eos.acknowledge().is_err());
    role.await.unwrap().unwrap();
    assert!(stream.get_operation().await.unwrap().is_none());
    assert_eq!(
        member
            .interfaces
            .replicator()
            .current_progress()
            .await
            .unwrap(),
        7
    );
    assert!(member.transport.sessions.vacant_receiver());
    member.finish().await;
}

#[tokio::test]
async fn production_unbound_role_eos_rechecks_actual_guard_and_refuses_invented_or_copy_eos() {
    use super::super::super::super::operation_queue::QueueOperation;
    use super::super::super::super::received_operation::ReceivedOperation as NativeOperation;
    use super::super::super::production::retained_role_drain_operation;
    use crate::replicator::stream::OperationKind;
    let member = Member::new(2, true, 7).await;
    member.restore().await;
    let owner = member.transport.test_production();
    let service = owner.retained_service();
    let marker = service.retained_receiver.lock().unwrap().clone();
    let secondary = owner.drainage_secondary();
    let mut stream = service.replication_stream().unwrap();
    let control = member.interfaces.replicator();
    let role =
        tokio::spawn(async move { control.change_role(epoch(2), ReplicaRole::Primary).await });
    let eos = tokio::time::timeout(Duration::from_secs(5), stream.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let wrap = |operation, kind| super::super::super::ServiceOperation {
        operation,
        secondary: secondary.clone(),
        retained_receiver: marker.clone(),
        kind,
    };
    let admitted = Arc::new(AtomicBool::new(true));
    for (operation, kind) in [
        (
            NativeOperation::end_of_stream(false),
            StreamKind::Replication,
        ),
        (
            NativeOperation::copy_end_of_stream(3, true).unwrap(),
            StreamKind::Replication,
        ),
        (NativeOperation::end_of_stream(true), StreamKind::Copy),
        (
            NativeOperation::new(
                QueueOperation::new(1, Bytes::new(), true),
                Epoch::new(0, 0),
                true,
            ),
            StreamKind::Replication,
        ),
        (
            NativeOperation::new(QueueOperation::new(0, Bytes::new(), true), epoch(2), true),
            StreamKind::Replication,
        ),
    ] {
        assert!(
            retained_role_drain_operation(
                member.transport.clone(),
                service.clone(),
                admitted.clone(),
                wrap(operation, kind),
                OperationKind::Replication
            )
            .is_none()
        );
    }
    let invented = retained_role_drain_operation(
        member.transport.clone(),
        service.clone(),
        admitted.clone(),
        wrap(
            NativeOperation::end_of_stream(true),
            StreamKind::Replication,
        ),
        OperationKind::Replication,
    )
    .unwrap();
    assert!(
        invented.acknowledge().is_err(),
        "the actual native queue must reject a foreign EOS identity"
    );
    let actual = retained_role_drain_operation(
        member.transport.clone(),
        service.clone(),
        admitted,
        eos,
        OperationKind::Replication,
    )
    .unwrap();
    member
        .transport
        .selected_role(epoch(2), ReplicaRole::ActiveSecondary);
    assert!(
        actual.acknowledge().is_err(),
        "epoch-only/lifecycle selection cannot acknowledge the role EOS"
    );
    member
        .transport
        .begin_role_change(epoch(2), ReplicaRole::Primary);
    actual.acknowledge().unwrap();
    role.await.unwrap().unwrap();
    assert!(actual.acknowledge().is_err());
    member.finish().await;
}

async fn elected(source: &Member, target: &Member) -> Binding {
    target.restore().await;
    {
        let mut publication = target.access.publication.lock().unwrap();
        publication.stage = ReplicaPublicationStage::AchievedScope;
        publication.role = ReplicaRole::ActiveSecondary;
    }
    // The real controller qualifies an achieved restoration with native queries.
    assert_eq!(
        target
            .interfaces
            .replicator()
            .current_progress()
            .await
            .unwrap(),
        7
    );
    target
        .interfaces
        .replicator()
        .update_epoch(epoch(2))
        .await
        .unwrap();
    source.interfaces.replicator().open().await.unwrap();
    source
        .interfaces
        .replicator()
        .change_role(epoch(2), ReplicaRole::Primary)
        .await
        .unwrap();
    publish_election(source, target)
}

fn publish_election(source: &Member, target: &Member) -> Binding {
    let mut source_publication = source.access.publication.lock().unwrap().clone();
    let mut target_publication = target.access.publication.lock().unwrap().clone();
    let current = configuration(3, &[3, 2], epoch(2));
    for publication in [&mut source_publication, &mut target_publication] {
        publication.scope.epoch = epoch(2);
        publication.scope.configuration.current = current.clone();
        publication.stage = ReplicaPublicationStage::AchievedScope;
        publication.generation += 1;
    }
    source_publication.role = ReplicaRole::Primary;
    target_publication.role = ReplicaRole::ActiveSecondary;
    source_publication.scope.configuration.replicas =
        vec![peer(&target_publication, ReplicaRole::ActiveSecondary, 7)];
    target_publication.scope.configuration.replicas =
        vec![peer(&source_publication, ReplicaRole::Primary, 7)];
    *source.access.publication.lock().unwrap() = source_publication.clone();
    *target.access.publication.lock().unwrap() = target_publication.clone();
    source
        .transport
        .test_publish(vec![target_publication.clone()]);
    target
        .transport
        .test_publish(vec![source_publication.clone()]);
    let mut binding = Binding::new(source_publication, target_publication).unwrap();
    binding.reformation = true;
    binding.validate().unwrap();
    binding
}

#[tokio::test]
async fn production_reformation_does_not_borrow_caller_ready_before_actual_native_and_fresh_host_receipts()
 {
    for missing in ["native", "epoch", "achieved-restoration"] {
        let source = Member::new(3, false, 7).await;
        let target = Member::new(2, true, 7).await;
        source.interfaces.replicator().open().await.unwrap();
        source
            .interfaces
            .replicator()
            .change_role(epoch(2), ReplicaRole::Primary)
            .await
            .unwrap();
        if missing != "native" {
            target.restore().await;
        }
        if missing == "achieved-restoration" {
            target
                .interfaces
                .replicator()
                .update_epoch(epoch(2))
                .await
                .unwrap();
        }
        let binding = publish_election(&source, &target);
        assert!(
            source
                .transport
                .control(&binding, Control::CheckReformation)
                .await
                .is_err(),
            "{missing}"
        );
        assert!(
            source
                .transport
                .control(&binding, Control::Reform)
                .await
                .is_err(),
            "{missing}"
        );
        assert!(target.transport.sessions.vacant_receiver());
        source.finish().await;
        target.finish().await;
    }
}

#[tokio::test]
async fn production_retained_reformation_authenticates_fresh_native_binding_and_waits_actual_ack() {
    let source = Member::new(3, false, 7).await;
    let target = Member::new(2, true, 7).await;
    let binding = elected(&source, &target).await;
    let primary = source.interfaces.primary_replicator().unwrap();
    let current = ReplicaSetConfiguration {
        configuration: binding.source.scope.configuration.current.clone(),
        replicas: vec![information(
            &binding.target,
            ReplicaRole::ActiveSecondary,
            7,
        )],
    };
    let entered = Arc::new(Semaphore::new(0));
    let release = Arc::new(Semaphore::new(0));
    let armed = Arc::new(AtomicUsize::new(2));
    let cut = TestingTcpRoute::register_with_kind(identity(3).instance_id.to_string(), {
        let entered = entered.clone();
        let release = release.clone();
        let armed = armed.clone();
        move |kind, _, address| {
            let entered = entered.clone();
            let release = release.clone();
            let armed = armed.clone();
            Box::pin(async move {
                if kind == TestingTcpRouteKind::BeforeDispatch
                    && armed
                        .try_update(Ordering::SeqCst, Ordering::SeqCst, |left| {
                            left.checked_sub(1)
                        })
                        .is_ok()
                {
                    entered.add_permits(1);
                    release.acquire().await.unwrap().forget();
                }
                Ok(address)
            })
        }
    });
    let selected = current.clone();
    let configuration = tokio::spawn(async move {
        primary
            .update_current_replica_set_configuration(selected)
            .await
    });
    tokio::time::timeout(Duration::from_secs(5), entered.acquire())
        .await
        .unwrap()
        .unwrap()
        .forget();
    assert!(
        !configuration.is_finished(),
        "sampled prefix cannot complete the source configuration"
    );
    let selected = source.transport.sessions.binding_for(2).unwrap();
    assert!(selected.reformation && selected.continuation.is_none() && !selected.recovery);
    assert_eq!(
        target.transport.sessions.inbound().as_ref(),
        Some(&selected)
    );
    assert!(!source.transport.sessions.completed(&selected));
    assert!(!target.transport.sessions.completed(&selected));
    let receiver = target.transport.test_production().drainage_secondary();
    let (owner, generation) = receiver.transport_identity().unwrap();
    assert_eq!(generation, 1);
    assert!(!owner.is_nil());
    assert_eq!(
        source
            .transport
            .reformation_checkpoint(&selected)
            .unwrap()
            .load(Ordering::SeqCst),
        -1
    );
    let native_route =
        super::super::super::production_transport::route(&selected, source.transport.local())
            .unwrap();
    let primary = source.transport.test_production().removal_primary();
    let identity = super::super::super::super::session::SessionIdentity {
        primary_endpoint: native_route.sender_address.clone(),
        remote_endpoint: native_route.remote_address.clone(),
        remote_incarnation: source.transport.native_incarnation(&selected).unwrap(),
    };
    assert!(
        primary
            .continuation_session(&native_route, &identity, epoch(2))
            .is_err(),
        "no sender root can borrow the sampled/provider prefix before a fresh native ACK"
    );
    release.add_permits(1);
    tokio::time::timeout(Duration::from_secs(5), entered.acquire())
        .await
        .unwrap()
        .unwrap()
        .forget();
    assert!(
        !configuration.is_finished(),
        "native sender must process its own fresh ACK after installation"
    );
    let native = primary
        .continuation_session(&native_route, &identity, epoch(2))
        .unwrap();
    assert_eq!(
        source
            .transport
            .reformation_checkpoint(&selected)
            .unwrap()
            .load(Ordering::SeqCst),
        7
    );
    assert!(
        source
            .transport
            .authorize_completion(&selected)
            .await
            .is_err()
    );
    release.add_permits(1);
    configuration.await.unwrap().unwrap();
    drop(cut);
    assert!(native.progress(0).applied >= 7);
    assert!(source.transport.sessions.completed(&selected));
    assert!(target.transport.sessions.completed(&selected));
    let mut stream = target
        .interfaces
        .state_replicator()
        .unwrap()
        .replication_operations()
        .unwrap();
    let mut write = source
        .interfaces
        .state_replicator()
        .unwrap()
        .begin_replicate(Bytes::from_static(b"after-retained-bind"))
        .unwrap();
    let operation = tokio::time::timeout(Duration::from_secs(5), stream.get_operation())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(operation.data, Bytes::from_static(b"after-retained-bind"));
    assert_eq!(operation.lsn, 8);
    assert!(write.completion.as_mut().now_or_never().is_none());
    target
        .provider
        .progress
        .store(operation.lsn, Ordering::SeqCst);
    operation.acknowledge().unwrap();
    tokio::time::timeout(Duration::from_secs(5), write.completion)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(target.provider.progress.load(Ordering::SeqCst), 8);
    assert_eq!(target.provider.copy_calls.load(Ordering::SeqCst), 0);
    assert_eq!(target.provider.context_calls.load(Ordering::SeqCst), 0);
    let mut crossed = selected.clone();
    crossed.nonce = uuid::Uuid::new_v4().to_string();
    assert!(
        source
            .transport
            .control(&crossed, Control::Reform)
            .await
            .is_err()
    );
    // Unknown-nonce retirement retains its own remove-before-Build tombstone.
    // It must not retire the different live receiver or borrow its ACK owner.
    source
        .transport
        .control(&crossed, Control::Retire)
        .await
        .unwrap();
    assert!(source.transport.sessions.completed(&selected));
    assert!(target.transport.sessions.completed(&selected));
    assert_eq!(receiver.transport_identity().unwrap(), (owner, generation));
    let mut later = source
        .interfaces
        .state_replicator()
        .unwrap()
        .begin_replicate(Bytes::from_static(b"after-crossed-retire"))
        .unwrap();
    let operation = tokio::time::timeout(Duration::from_secs(5), stream.get_operation())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(operation.lsn, 9);
    assert_eq!(operation.data, Bytes::from_static(b"after-crossed-retire"));
    assert!(later.completion.as_mut().now_or_never().is_none());
    target
        .provider
        .progress
        .store(operation.lsn, Ordering::SeqCst);
    operation.acknowledge().unwrap();
    tokio::time::timeout(Duration::from_secs(5), later.completion)
        .await
        .unwrap()
        .unwrap();
    source.finish().await;
    target.finish().await;
}

#[tokio::test]
async fn production_live_source_retained_target_preserves_pending_pair_roots_and_exact_retry() {
    let source = Member::new(1, false, 0).await;
    let old_target = Member::new(2, false, 0).await;
    let mut source_publication = source.access.publication.lock().unwrap().clone();
    let mut old_publication = old_target.access.publication.lock().unwrap().clone();
    let bootstrap = ConfigurationDescriptor::new(
        epoch(1),
        ReplicaId::new(1),
        vec![ConfigurationMember {
            identity: identity(1),
            role: ReplicaRole::Primary,
        }],
        1,
    );
    for publication in [&mut source_publication, &mut old_publication] {
        publication.scope.configuration.current = bootstrap.clone();
        publication.stage = ReplicaPublicationStage::AchievedScope;
    }
    source_publication.role = ReplicaRole::Primary;
    old_publication.role = ReplicaRole::IdleSecondary;
    old_publication.scope.configuration.replicas =
        source_publication.scope.configuration.replicas.clone();
    *source.access.publication.lock().unwrap() = source_publication.clone();
    *old_target.access.publication.lock().unwrap() = old_publication.clone();
    source.transport.test_publish(vec![old_publication.clone()]);
    old_target
        .transport
        .test_publish(vec![source_publication.clone()]);
    source.interfaces.replicator().open().await.unwrap();
    source
        .interfaces
        .replicator()
        .change_role(epoch(1), ReplicaRole::Primary)
        .await
        .unwrap();
    old_target.interfaces.replicator().open().await.unwrap();
    old_target
        .interfaces
        .replicator()
        .change_role(epoch(1), ReplicaRole::IdleSecondary)
        .await
        .unwrap();
    source
        .interfaces
        .primary_replicator()
        .unwrap()
        .update_current_replica_set_configuration(ReplicaSetConfiguration {
            configuration: bootstrap.clone(),
            replicas: Vec::new(),
        })
        .await
        .unwrap();
    let primary = source.interfaces.primary_replicator().unwrap();
    let mut idle = information(&old_publication, ReplicaRole::IdleSecondary, INVALID_LSN);
    idle.catch_up_capability = INVALID_LSN;
    let mut build = tokio::spawn(async move { primary.build_replica(idle).await });
    let mut copy = old_target
        .interfaces
        .state_replicator()
        .unwrap()
        .copy_operations()
        .unwrap();
    loop {
        let operation = tokio::select! {
            operation = tokio::time::timeout(Duration::from_secs(5), copy.get_operation()) => {
                operation.expect("initial actual native copy did not produce its next operation")
                    .unwrap().unwrap()
            }
            result = &mut build => panic!("initial actual Build ended before its copy receipt: {result:?}"),
        };
        let last = operation.end_of_stream;
        if !last {
            assert_eq!(operation.data, Bytes::from_static(b"snapshot"));
        }
        operation.acknowledge().unwrap();
        if last {
            break;
        }
    }
    build.await.unwrap().unwrap();
    let old_binding = source.transport.sessions.binding_for(2).unwrap();
    let original = configuration(1, &[1, 2], epoch(1));
    source_publication.scope.configuration.current = original.clone();
    source_publication.scope.configuration.previous = Some(bootstrap.clone());
    source_publication.stage = ReplicaPublicationStage::DiscoveryPending;
    source_publication.role = ReplicaRole::None;
    source_publication.scope.configuration.replicas =
        vec![peer(&old_publication, ReplicaRole::ActiveSecondary, 0)];
    *source.access.publication.lock().unwrap() = source_publication.clone();
    *old_target.access.publication.lock().unwrap() = old_publication.clone();
    source.transport.test_publish(vec![old_publication.clone()]);
    old_target
        .transport
        .test_publish(vec![source_publication.clone()]);
    source
        .interfaces
        .primary_replicator()
        .unwrap()
        .update_catch_up_replica_set_configuration(
            ReplicaSetConfiguration {
                configuration: original.clone(),
                replicas: vec![information(
                    &old_publication,
                    ReplicaRole::ActiveSecondary,
                    0,
                )],
            },
            ReplicaSetConfiguration {
                configuration: bootstrap.clone(),
                replicas: Vec::new(),
            },
        )
        .await
        .unwrap();
    old_target
        .interfaces
        .replicator()
        .change_role(epoch(1), ReplicaRole::ActiveSecondary)
        .await
        .unwrap();
    old_publication.scope.configuration.current = original.clone();
    old_publication.scope.configuration.previous = Some(bootstrap);
    old_publication.role = ReplicaRole::ActiveSecondary;
    old_publication.scope.configuration.replicas =
        vec![peer(&source_publication, ReplicaRole::Primary, 0)];
    *old_target.access.publication.lock().unwrap() = old_publication.clone();
    source.transport.test_publish(vec![old_publication.clone()]);
    old_target
        .transport
        .test_publish(vec![source_publication.clone()]);
    source
        .interfaces
        .primary_replicator()
        .unwrap()
        .wait_for_catch_up_quorum(crate::replicator::ReplicaSetQuorumMode::All)
        .await
        .unwrap();
    source_publication.scope.configuration.previous = None;
    *source.access.publication.lock().unwrap() = source_publication.clone();
    *old_target.access.publication.lock().unwrap() = old_publication.clone();
    source.transport.test_publish(vec![old_publication.clone()]);
    old_target
        .transport
        .test_publish(vec![source_publication.clone()]);
    source
        .interfaces
        .primary_replicator()
        .unwrap()
        .update_current_replica_set_configuration(ReplicaSetConfiguration {
            configuration: original.clone(),
            replicas: vec![information(
                &old_publication,
                ReplicaRole::ActiveSecondary,
                0,
            )],
        })
        .await
        .unwrap();
    source_publication.stage = ReplicaPublicationStage::AchievedScope;
    source_publication.role = ReplicaRole::Primary;
    old_publication.scope.configuration.previous = None;
    *source.access.publication.lock().unwrap() = source_publication.clone();
    *old_target.access.publication.lock().unwrap() = old_publication.clone();
    source.transport.test_publish(vec![old_publication.clone()]);
    old_target
        .transport
        .test_publish(vec![source_publication.clone()]);
    let source_endpoint = source.transport.local().clone();
    let old_endpoint = old_target.transport.local().clone();
    let native = source.transport.test_production().removal_primary();
    let old_route =
        super::super::super::production_transport::route(&old_binding, &source_endpoint).unwrap();
    let old_identity = super::super::super::super::session::SessionIdentity {
        primary_endpoint: old_route.sender_address.clone(),
        remote_endpoint: old_route.remote_address.clone(),
        remote_incarnation: source.transport.native_incarnation(&old_binding).unwrap(),
    };
    let original_sender = native
        .continuation_session(&old_route, &old_identity, epoch(1))
        .unwrap();
    let mut old_stream = old_target
        .interfaces
        .state_replicator()
        .unwrap()
        .replication_operations()
        .unwrap();
    let first = source
        .interfaces
        .state_replicator()
        .unwrap()
        .begin_replicate(Bytes::from_static(b"retained-prefix"))
        .unwrap();
    let operation = tokio::time::timeout(Duration::from_secs(5), old_stream.get_operation())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(operation.lsn, 1);
    old_target.provider.progress.store(1, Ordering::SeqCst);
    operation.acknowledge().unwrap();
    first.completion.await.unwrap();
    source.provider.progress.store(1, Ordering::SeqCst);
    let mut pending = source
        .interfaces
        .state_replicator()
        .unwrap()
        .begin_replicate(Bytes::from_static(b"pending-before-reformation"))
        .unwrap();
    let old_operation = tokio::time::timeout(Duration::from_secs(5), old_stream.get_operation())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(old_operation.lsn, 2);
    assert!(pending.completion.as_mut().now_or_never().is_none());
    let source_progress_queries = source.provider.progress_calls.load(Ordering::SeqCst);
    let retained_provider = old_target.provider.clone();
    let historical_copy_calls = retained_provider.copy_calls.load(Ordering::SeqCst);
    let historical_context_calls = retained_provider.context_calls.load(Ordering::SeqCst);
    // The target's actual old owners are joined; the source stays live.
    drop(copy);
    drop(old_stream);
    old_target.finish().await;
    let target = Member::new_scoped(
        2,
        true,
        1,
        false,
        Some(old_publication.clone()),
        Some(retained_provider.clone()),
    )
    .await;
    assert!(Arc::ptr_eq(&target.provider, &retained_provider));
    assert_eq!(target.transport.local().storage, old_endpoint.storage);
    assert_ne!(
        target.transport.local().process_session_id,
        old_endpoint.process_session_id
    );
    assert_ne!(
        target.transport.local().creation_id,
        old_endpoint.creation_id
    );
    target.restore().await;
    let mut target_publication = target.access.publication.lock().unwrap().clone();
    target_publication.stage = ReplicaPublicationStage::AchievedScope;
    target_publication.role = ReplicaRole::ActiveSecondary;
    *target.access.publication.lock().unwrap() = target_publication.clone();
    assert_eq!(
        target
            .interfaces
            .replicator()
            .current_progress()
            .await
            .unwrap(),
        1
    );
    source
        .interfaces
        .replicator()
        .update_epoch(epoch(2))
        .await
        .unwrap();
    target
        .interfaces
        .replicator()
        .update_epoch(epoch(2))
        .await
        .unwrap();
    let next = configuration(1, &[1, 2], epoch(2));
    for publication in [&mut source_publication, &mut target_publication] {
        publication.scope.epoch = epoch(2);
        publication.scope.configuration.current = next.clone();
        publication.scope.configuration.previous = Some(original.clone());
        publication.generation += 1;
    }
    source_publication.scope.configuration.replicas =
        vec![peer(&target_publication, ReplicaRole::ActiveSecondary, 1)];
    target_publication.scope.configuration.replicas =
        vec![peer(&source_publication, ReplicaRole::Primary, 1)];
    *source.access.publication.lock().unwrap() = source_publication.clone();
    *target.access.publication.lock().unwrap() = target_publication.clone();
    source
        .transport
        .test_publish(vec![target_publication.clone()]);
    target
        .transport
        .test_publish(vec![source_publication.clone()]);
    let mut attempted =
        Binding::new(source_publication.clone(), target_publication.clone()).unwrap();
    attempted.reformation = true;
    attempted.live_target = Some(
        source
            .transport
            .live_target_relation(&old_binding, &source_publication, &target_publication)
            .unwrap(),
    );
    assert!(
        source
            .transport
            .control(&attempted, Control::CheckReformation)
            .await
            .is_err(),
        "initialized checkpoint/publication cannot substitute for a consuming application"
    );
    let mut stream = target
        .interfaces
        .state_replicator()
        .unwrap()
        .replication_operations()
        .unwrap();
    assert!(stream.get_operation().now_or_never().is_none());
    let current = ReplicaSetConfiguration {
        configuration: next,
        replicas: vec![information(
            &target_publication,
            ReplicaRole::ActiveSecondary,
            1,
        )],
    };
    let previous = ReplicaSetConfiguration {
        configuration: original.clone(),
        // The public scope describes the fresh physical endpoint. Native PC
        // resolution must still select the exact frozen old sender root.
        replicas: current.replicas.clone(),
    };
    let entered = Arc::new(Semaphore::new(0));
    let release = Arc::new(Semaphore::new(0));
    let probes = Arc::new(AtomicUsize::new(0));
    let cut = TestingTcpRoute::register_with_kind(identity(1).instance_id.to_string(), {
        let entered = entered.clone();
        let release = release.clone();
        let probes = probes.clone();
        move |kind, _, address| {
            let entered = entered.clone();
            let release = release.clone();
            let probes = probes.clone();
            Box::pin(async move {
                if kind == TestingTcpRouteKind::EngineReserve
                    && probes.fetch_add(1, Ordering::SeqCst) == 1
                {
                    entered.add_permits(1);
                    release.acquire().await.unwrap().forget();
                }
                Ok(address)
            })
        }
    });
    let primary = source.interfaces.primary_replicator().unwrap();
    let selected = current.clone();
    let frozen = previous.clone();
    let observer = tokio::spawn(async move {
        primary
            .update_catch_up_replica_set_configuration(selected, frozen)
            .await
    });
    tokio::time::timeout(Duration::from_secs(5), entered.acquire())
        .await
        .unwrap()
        .unwrap()
        .forget();
    let fresh_binding = source.transport.sessions.binding_for(2).unwrap();
    assert!(fresh_binding.live_target.is_some());
    assert_ne!(fresh_binding.nonce, old_binding.nonce);
    assert!(source.transport.sessions.active(&old_binding).is_err());
    assert!(!original_sender.transport_active());
    assert!(!original_sender.is_processing());
    assert_eq!(source.transport.pending_sends(&old_binding), None);
    assert!(target.transport.sessions.vacant_receiver());
    assert_eq!(source.transport.local(), &source_endpoint);
    assert!(pending.completion.as_mut().now_or_never().is_none());
    observer.abort();
    assert!(observer.await.unwrap_err().is_cancelled());
    let primary = source.interfaces.primary_replicator().unwrap();
    let selected = current.clone();
    let retry = tokio::spawn(async move {
        primary
            .update_catch_up_replica_set_configuration(selected, previous)
            .await
    });
    assert!(!retry.is_finished());
    assert!(
        source
            .transport
            .sessions
            .effect(&old_binding, || panic!("old nonce regained authority"))
            .is_err()
    );
    assert!(old_operation.acknowledge().is_err());
    release.add_permits(1);
    let replay = tokio::time::timeout(Duration::from_secs(5), stream.get_operation())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(replay.lsn, 2);
    assert_eq!(
        replay.data,
        Bytes::from_static(b"pending-before-reformation")
    );
    assert!(pending.completion.as_mut().now_or_never().is_none());
    target.provider.progress.store(2, Ordering::SeqCst);
    replay.acknowledge().unwrap();
    tokio::time::timeout(Duration::from_secs(5), pending.completion)
        .await
        .unwrap()
        .unwrap();
    source.provider.progress.store(2, Ordering::SeqCst);
    tokio::time::timeout(Duration::from_secs(5), retry)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    drop(cut);
    assert_eq!(
        source.provider.progress_calls.load(Ordering::SeqCst),
        source_progress_queries
    );
    assert_eq!(original_sender.progress(0).applied, 1);
    let roots = native.inspect_configuration();
    assert_eq!(roots["current"]["writeQuorum"], 2);
    assert_eq!(roots["previous"]["writeQuorum"], 2);
    assert_eq!(
        roots["previous"]["members"][0]["incarnation"],
        old_identity.remote_incarnation
    );
    assert_ne!(
        roots["current"]["members"][0]["incarnation"],
        roots["previous"]["members"][0]["incarnation"]
    );
    let delayed_old_ack = old_route
        .reversed()
        .envelope(wire::envelope::Body::ReplicationAck(wire::ReplicationAck {
            replication_received: 2,
            replication_applied: 2,
            copy_received: -1,
            copy_applied: -1,
            ..Default::default()
        }));
    assert!(native.receive_ack(&delayed_old_ack).is_err());
    let mut crossed_nonce = fresh_binding.clone();
    crossed_nonce.nonce = uuid::Uuid::new_v4().to_string();
    assert!(
        target
            .transport
            .control(&crossed_nonce, Control::Confirm)
            .await
            .is_err()
    );
    assert_eq!(original_sender.progress(0).applied, 1);
    assert!(source.transport.sessions.completed(&fresh_binding));
    assert!(target.transport.sessions.completed(&fresh_binding));
    source
        .interfaces
        .primary_replicator()
        .unwrap()
        .wait_for_catch_up_quorum(crate::replicator::ReplicaSetQuorumMode::All)
        .await
        .unwrap();
    for publication in [&mut source_publication, &mut target_publication] {
        publication.scope.configuration.previous = None;
        publication.generation += 1;
    }
    *source.access.publication.lock().unwrap() = source_publication.clone();
    *target.access.publication.lock().unwrap() = target_publication.clone();
    source.transport.test_publish(vec![target_publication]);
    target.transport.test_publish(vec![source_publication]);
    source
        .interfaces
        .primary_replicator()
        .unwrap()
        .update_current_replica_set_configuration(current)
        .await
        .unwrap();
    let write = source
        .interfaces
        .state_replicator()
        .unwrap()
        .begin_replicate(Bytes::from_static(b"new-write-after-reformation"))
        .unwrap();
    let operation = tokio::time::timeout(Duration::from_secs(5), stream.get_operation())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(operation.lsn, 3);
    assert_eq!(
        operation.data,
        Bytes::from_static(b"new-write-after-reformation")
    );
    target.provider.progress.store(3, Ordering::SeqCst);
    operation.acknowledge().unwrap();
    tokio::time::timeout(Duration::from_secs(5), write.completion)
        .await
        .unwrap()
        .unwrap();
    source.provider.progress.store(3, Ordering::SeqCst);
    assert_eq!(
        source.provider.progress.load(Ordering::SeqCst),
        target.provider.progress.load(Ordering::SeqCst)
    );
    assert!(native.inspect_configuration()["previous"].is_null());
    assert_eq!(
        target.provider.copy_calls.load(Ordering::SeqCst),
        historical_copy_calls
    );
    assert_eq!(
        target.provider.context_calls.load(Ordering::SeqCst),
        historical_context_calls
    );
    drop(stream);
    source.finish().await;
    target.finish().await;
}

#[tokio::test]
async fn production_retained_reformation_rejects_scope_epoch_and_copy_repurpose() {
    let source = Member::new(3, false, 7).await;
    let target = Member::new(2, true, 7).await;
    let binding = elected(&source, &target).await;
    assert!(
        source
            .transport
            .control(&binding, Control::CheckReformation)
            .await
            .is_ok()
    );
    assert!(target.transport.sessions.vacant_receiver());
    assert!(matches!(
        target
            .transport
            .test_production()
            .drainage_secondary()
            .transport_identity(),
        Err(SecondaryError::RetainedTransport)
    ));
    let route =
        super::super::super::production_transport::route(&binding, source.transport.local())
            .unwrap();
    let frame = route.envelope(wire::envelope::Body::EngineSessionControl(
        wire::EngineSessionControl {
            canonical_json: serde_json::to_vec(&ControlEnvelope {
                binding: binding.clone(),
                command: Control::Reform,
            })
            .unwrap()
            .into(),
        },
    ));
    let channel =
        tonic::transport::Endpoint::from_shared(binding.target.replication_address.clone())
            .unwrap()
            .connect()
            .await
            .unwrap();
    let mut anonymous = wire::carrier_client::CarrierClient::new(channel);
    assert!(
        anonymous
            .exchange(tonic::Request::new(futures::stream::iter([frame])))
            .await
            .is_err()
    );
    assert!(target.transport.sessions.vacant_receiver());
    assert!(
        source
            .transport
            .control(&binding, Control::Reform)
            .await
            .is_err(),
        "a target probe and Ready do not replace the authenticated source's actual proposal"
    );
    assert!(target.transport.sessions.vacant_receiver());
    let mut normal_copy = binding.clone();
    normal_copy.reformation = false;
    source
        .transport
        .sessions
        .proposal(normal_copy.clone())
        .unwrap();
    assert!(
        source
            .transport
            .control(&normal_copy, Control::Reserve)
            .await
            .is_err()
    );
    assert!(
        target.transport.sessions.vacant_receiver(),
        "copy selection cannot consume the restored receiver"
    );
    for changed in [
        "history", "storage", "process", "creation", "epoch", "progress", "quorum",
    ] {
        let mut crossed = binding.clone();
        match changed {
            "history" => crossed.target.scope.history.data_loss_number += 1,
            "storage" => {
                crossed.target.scope.storage.storage_generation =
                    StorageGeneration::new("wrong-storage")
            }
            "process" => {
                crossed.target.scope.process_session_id = ProcessSessionId::new("old-process")
            }
            "creation" => {
                crossed.target.scope.creation_id = ReplicatorCreationId::new("old-creation")
            }
            "epoch" => crossed.target.scope.epoch = epoch(1),
            "progress" => crossed.source.scope.configuration.replicas[0].current_progress = 8,
            _ => crossed.target.scope.configuration.current.write_quorum = 1,
        }
        assert!(
            source
                .transport
                .control(&crossed, Control::CheckReformation)
                .await
                .is_err(),
            "{changed}"
        );
        assert!(target.transport.sessions.vacant_receiver());
    }
    target.access.withdrawn.store(true, Ordering::SeqCst);
    assert!(
        source
            .transport
            .control(&binding, Control::Reform)
            .await
            .is_err()
    );
    target.access.withdrawn.store(false, Ordering::SeqCst);
    source.finish().await;
    target.finish().await;
}
