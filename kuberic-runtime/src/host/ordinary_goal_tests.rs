use super::*;
use crate::application::{OpenContext, OperationData, OperationDataStream, RoleChange};
use crate::host::state::{SCHEMA_VERSION, StorageIdentity};
use crate::protocol::public_operations::*;
use crate::protocol::types::*;
use crate::replicator::stream::OperationStream;
use crate::replicator::*;
use crate::{StateProvider, StatefulServiceReplica};
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicI64, AtomicUsize};
use std::time::Duration;
use tokio::sync::Semaphore;

struct Probe {
    native_events: StdMutex<Vec<String>>,
    provider_progress: AtomicI64,
    fail_provider_progress: AtomicBool,
    provider_data_loss_calls: AtomicUsize,
    fail_provider_epoch: AtomicBool,
    block_provider_epoch: AtomicBool,
    provider_epoch_entered: Semaphore,
    provider_epoch_gate: Semaphore,
    native_writer: StdMutex<Option<Arc<dyn StateReplicator>>>,
    context: StdMutex<Option<ReplicatorFactoryContext>>,
    listener: StdMutex<Option<TcpListener>>,
    block_role: AtomicBool,
    role_gate: Semaphore,
    role_entered: Semaphore,
    fail_role: AtomicBool,
    block_epoch: AtomicBool,
    epoch_gate: Semaphore,
    epoch_entered: Semaphore,
    block_configuration: AtomicBool,
    configuration_gate: Semaphore,
    configuration_entered: Semaphore,
    configuration_calls: AtomicUsize,
    block_catch_up: AtomicBool,
    catch_up_gate: Semaphore,
    catch_up_entered: Semaphore,
    fail_catch_up: AtomicBool,
    block_close: AtomicBool,
    close_gate: Semaphore,
    close_entered: Semaphore,
    close_returned: Semaphore,
    block_application_close: AtomicBool,
    application_close_gate: Semaphore,
    application_close_entered: Semaphore,
    application_close_returned: Semaphore,
    fail_close: AtomicBool,
    block_build: AtomicBool,
    panic_build: AtomicBool,
    build_rebuild: StdMutex<Option<RemoteBuildTargetRebuildRequired>>,
    build_gate: Semaphore,
    build_entered: Semaphore,
    build_calls: AtomicUsize,
    open_calls: AtomicUsize,
    remove_calls: AtomicUsize,
    close_calls: AtomicUsize,
    progress_calls: AtomicUsize,
    progress_result: StdMutex<Option<crate::Result<i64>>>,
    capability_result: StdMutex<Option<crate::Result<i64>>>,
    block_progress: AtomicBool,
    progress_entered: Semaphore,
    progress_gate: Semaphore,
    aborts: AtomicUsize,
    aborted: Semaphore,
    on_abort: StdMutex<Option<Box<dyn Fn() + Send + Sync>>>,
    use_default_factory: AtomicBool,
    native_primary: StdMutex<Option<Arc<dyn PrimaryReplicator>>>,
    block_factory: AtomicBool,
    factory_gate: Semaphore,
    factory_entered: Semaphore,
    factory_listener: StdMutex<Option<TcpListener>>,
    replacement_return: StdMutex<Option<Arc<Probe>>>,
}

impl Probe {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            native_events: StdMutex::new(Vec::new()),
            provider_progress: AtomicI64::new(0),
            fail_provider_progress: AtomicBool::new(false),
            provider_data_loss_calls: AtomicUsize::new(0),
            fail_provider_epoch: AtomicBool::new(false),
            block_provider_epoch: AtomicBool::new(false),
            provider_epoch_entered: Semaphore::new(0),
            provider_epoch_gate: Semaphore::new(0),
            native_writer: StdMutex::new(None),
            context: StdMutex::new(None),
            listener: StdMutex::new(None),
            block_role: AtomicBool::new(false),
            role_gate: Semaphore::new(0),
            role_entered: Semaphore::new(0),
            fail_role: AtomicBool::new(false),
            block_epoch: AtomicBool::new(false),
            epoch_gate: Semaphore::new(0),
            epoch_entered: Semaphore::new(0),
            block_configuration: AtomicBool::new(false),
            configuration_gate: Semaphore::new(0),
            configuration_entered: Semaphore::new(0),
            configuration_calls: AtomicUsize::new(0),
            block_catch_up: AtomicBool::new(false),
            catch_up_gate: Semaphore::new(0),
            catch_up_entered: Semaphore::new(0),
            fail_catch_up: AtomicBool::new(false),
            block_close: AtomicBool::new(false),
            close_gate: Semaphore::new(0),
            close_entered: Semaphore::new(0),
            close_returned: Semaphore::new(0),
            block_application_close: AtomicBool::new(false),
            application_close_gate: Semaphore::new(0),
            application_close_entered: Semaphore::new(0),
            application_close_returned: Semaphore::new(0),
            fail_close: AtomicBool::new(false),
            block_build: AtomicBool::new(false),
            panic_build: AtomicBool::new(false),
            build_rebuild: StdMutex::new(None),
            build_gate: Semaphore::new(0),
            build_entered: Semaphore::new(0),
            build_calls: AtomicUsize::new(0),
            open_calls: AtomicUsize::new(0),
            remove_calls: AtomicUsize::new(0),
            close_calls: AtomicUsize::new(0),
            progress_calls: AtomicUsize::new(0),
            progress_result: StdMutex::new(None),
            capability_result: StdMutex::new(None),
            block_progress: AtomicBool::new(false),
            progress_entered: Semaphore::new(0),
            progress_gate: Semaphore::new(0),
            aborts: AtomicUsize::new(0),
            aborted: Semaphore::new(0),
            on_abort: StdMutex::new(None),
            use_default_factory: AtomicBool::new(false),
            native_primary: StdMutex::new(None),
            block_factory: AtomicBool::new(false),
            factory_gate: Semaphore::new(0),
            factory_entered: Semaphore::new(0),
            factory_listener: StdMutex::new(None),
            replacement_return: StdMutex::new(None),
        })
    }
}

#[path = "inspection_history_tests.rs"]
mod inspection_history;

struct Provider(Arc<Probe>);
#[async_trait::async_trait]
impl StateProvider for Provider {
    async fn update_epoch(&self, epoch: Epoch, _: i64) -> crate::Result<()> {
        self.0
            .native_events
            .lock()
            .unwrap()
            .push(format!("provider.epoch:{epoch:?}"));
        self.0.provider_epoch_entered.add_permits(1);
        if self.0.block_provider_epoch.load(Ordering::Acquire) {
            self.0.provider_epoch_gate.acquire().await.unwrap().forget();
        }
        if self.0.fail_provider_epoch.load(Ordering::Acquire) {
            return Err(RuntimeError::Application(
                "actual native provider epoch failed".into(),
            ));
        }
        Ok(())
    }
    async fn last_committed_lsn(&self) -> crate::Result<i64> {
        self.0
            .native_events
            .lock()
            .unwrap()
            .push("provider.progress".into());
        if self.0.fail_provider_progress.load(Ordering::Acquire) {
            return Err(RuntimeError::Application(
                "actual native provider progress failed".into(),
            ));
        }
        Ok(self.0.provider_progress.load(Ordering::Acquire))
    }
    async fn get_copy_context(&self) -> crate::Result<OperationDataStream> {
        Ok(Box::pin(futures::stream::empty()))
    }
    async fn get_copy_state(
        &self,
        _: i64,
        _: OperationDataStream,
    ) -> crate::Result<OperationDataStream> {
        Ok(Box::pin(futures::stream::empty()))
    }
    async fn on_data_loss(&self) -> crate::Result<bool> {
        self.0
            .provider_data_loss_calls
            .fetch_add(1, Ordering::AcqRel);
        Ok(false)
    }
}

struct Writer;
#[async_trait::async_trait]
impl StateReplicator for Writer {
    fn begin_replicate(&self, _: OperationData) -> crate::Result<Replication> {
        Err(RuntimeError::AuthorityNotAdmitted)
    }
    fn replication_operations(&self) -> crate::Result<OperationStream> {
        Err(RuntimeError::AuthorityNotAdmitted)
    }
    fn copy_operations(&self) -> crate::Result<OperationStream> {
        Err(RuntimeError::AuthorityNotAdmitted)
    }
    async fn update_replicator_settings(&self, _: ReplicatorSettings) -> crate::Result<()> {
        Ok(())
    }
}

struct OrdinaryFactory(Arc<Probe>);
#[async_trait::async_trait]
impl ReplicatorFactory for OrdinaryFactory {
    async fn create_replicator(
        &self,
        context: ReplicatorFactoryContext,
        _: Option<Arc<dyn StateProvider>>,
        _: ReplicatorSettings,
    ) -> crate::Result<ReplicatorInterfaces> {
        assert!(context.ordinary_hosting());
        *self.0.context.lock().unwrap() = Some(context);
        let interfaces = ReplicatorInterfaces::primary(
            self.0.clone(),
            Some(Arc::new(Writer)),
            ReplicatorCapabilities::conservative(),
        );
        if self.0.block_factory.load(Ordering::Acquire) {
            *self.0.factory_listener.lock().unwrap() =
                Some(TcpListener::bind("127.0.0.1:0").await.unwrap());
            self.0.factory_entered.add_permits(1);
            self.0.factory_gate.acquire().await.unwrap().forget();
        }
        Ok(interfaces)
    }
}

struct Application(Arc<Probe>);
#[async_trait::async_trait]
impl StatefulServiceReplica for Application {
    async fn open(self: Arc<Self>, context: OpenContext) -> crate::Result<Arc<dyn Replicator>> {
        let factory: Arc<dyn ReplicatorFactory> =
            if self.0.use_default_factory.load(Ordering::Acquire) {
                Arc::new(inspection_history::NativeFactory(self.0.clone()))
            } else {
                Arc::new(OrdinaryFactory(self.0.clone()))
            };
        let interfaces = context
            .partition
            .with_factory(factory)
            .create_replicator(
                Some(Arc::new(Provider(self.0.clone()))),
                Some(ReplicatorSettings {
                    replication_address: "http://127.0.0.1:0".into(),
                }),
            )
            .await?;
        if self.0.use_default_factory.load(Ordering::Acquire) {
            *self.0.native_writer.lock().unwrap() = interfaces.state_replicator();
        }
        let replacement = self.0.replacement_return.lock().unwrap().clone();
        if let Some(replacement) = replacement {
            replacement.open().await?;
            Ok(replacement)
        } else {
            Ok(interfaces.replicator())
        }
    }
    async fn change_role(&self, role: ReplicaRole) -> crate::Result<RoleChange> {
        self.0
            .native_events
            .lock()
            .unwrap()
            .push(format!("application.role:{role:?}"));
        Ok(RoleChange {
            service_address: None,
        })
    }
    async fn close(&self) -> crate::Result<()> {
        self.0.application_close_entered.add_permits(1);
        if self.0.block_application_close.load(Ordering::Acquire) {
            self.0
                .application_close_gate
                .acquire()
                .await
                .unwrap()
                .forget();
        }
        self.0.application_close_returned.add_permits(1);
        Ok(())
    }
    fn abort(&self) {}
}

#[async_trait::async_trait]
impl Replicator for Probe {
    async fn open(&self) -> crate::Result<String> {
        self.open_calls.fetch_add(1, Ordering::AcqRel);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        *self.listener.lock().unwrap() = Some(listener);
        Ok(endpoint)
    }
    async fn change_role(&self, _: Epoch, _: ReplicaRole) -> crate::Result<()> {
        self.role_entered.add_permits(1);
        if self.block_role.load(Ordering::Acquire) {
            self.role_gate.acquire().await.unwrap().forget();
        }
        if self.fail_role.load(Ordering::Acquire) {
            return Err(RuntimeError::Application("preparation failed".into()));
        }
        Ok(())
    }
    async fn update_epoch(&self, _: Epoch) -> crate::Result<()> {
        self.epoch_entered.add_permits(1);
        if self.block_epoch.load(Ordering::Acquire) {
            self.epoch_gate.acquire().await.unwrap().forget();
        }
        Ok(())
    }
    async fn close(&self) -> crate::Result<()> {
        self.close_calls.fetch_add(1, Ordering::AcqRel);
        self.close_entered.add_permits(1);
        if self.block_close.load(Ordering::Acquire) {
            self.close_gate.acquire().await.unwrap().forget();
        }
        self.listener.lock().unwrap().take();
        self.factory_listener.lock().unwrap().take();
        self.close_returned.add_permits(1);
        if self.fail_close.load(Ordering::Acquire) {
            return Err(RuntimeError::Application("ordinary Close failed".into()));
        }
        Ok(())
    }
    fn abort(&self) {
        if let Some(check) = &*self.on_abort.lock().unwrap() {
            check();
        }
        self.aborts.fetch_add(1, Ordering::AcqRel);
        self.aborted.add_permits(1);
    }
    async fn current_progress(&self) -> crate::Result<i64> {
        self.progress_calls.fetch_add(1, Ordering::AcqRel);
        self.progress_entered.add_permits(1);
        if self.block_progress.load(Ordering::Acquire) {
            self.progress_gate.acquire().await.unwrap().forget();
        }
        if let Some(result) = self.progress_result.lock().unwrap().take() {
            return result;
        }
        std::future::pending().await
    }
    async fn catch_up_capability(&self) -> crate::Result<i64> {
        self.progress_calls.fetch_add(1, Ordering::AcqRel);
        if let Some(result) = self.capability_result.lock().unwrap().take() {
            return result;
        }
        std::future::pending().await
    }
}

#[async_trait::async_trait]
impl PrimaryReplicator for Probe {
    async fn on_data_loss(&self) -> crate::Result<bool> {
        Ok(false)
    }
    async fn update_catch_up_replica_set_configuration(
        &self,
        _: ReplicaSetConfiguration,
        _: ReplicaSetConfiguration,
    ) -> crate::Result<()> {
        self.configuration_calls.fetch_add(1, Ordering::AcqRel);
        self.configuration_entered.add_permits(1);
        if self.block_configuration.load(Ordering::Acquire) {
            self.configuration_gate.acquire().await.unwrap().forget();
        }
        Ok(())
    }
    async fn wait_for_catch_up_quorum(&self, _: ReplicaSetQuorumMode) -> crate::Result<()> {
        self.catch_up_entered.add_permits(1);
        if self.block_catch_up.load(Ordering::Acquire) {
            self.catch_up_gate.acquire().await.unwrap().forget();
        }
        if self.fail_catch_up.load(Ordering::Acquire) {
            return Err(RuntimeError::Application("ordinary catch-up failed".into()));
        }
        Ok(())
    }
    async fn update_current_replica_set_configuration(
        &self,
        _: ReplicaSetConfiguration,
    ) -> crate::Result<()> {
        self.configuration_calls.fetch_add(1, Ordering::AcqRel);
        self.configuration_entered.add_permits(1);
        if self.block_configuration.load(Ordering::Acquire) {
            self.configuration_gate.acquire().await.unwrap().forget();
        }
        Ok(())
    }
    async fn build_replica(&self, replica: ReplicaInformation) -> crate::Result<()> {
        assert_eq!(replica.current_progress, INVALID_LSN);
        assert_eq!(replica.catch_up_capability, INVALID_LSN);
        self.build_calls.fetch_add(1, Ordering::AcqRel);
        self.build_entered.add_permits(1);
        if self.block_build.load(Ordering::Acquire) {
            self.build_gate.acquire().await.unwrap().forget();
        }
        assert!(
            !self.panic_build.load(Ordering::Acquire),
            "injected ordinary Build owner panic"
        );
        if let Some(failure) = self.build_rebuild.lock().unwrap().clone() {
            return Err(RuntimeError::ReplicaRebuildRequired(Box::new(
                ReplicaRebuildRequired::RemoteBuildTarget(Box::new(failure)),
            )));
        }
        Ok(())
    }
    async fn remove_replica(&self, _: ReplicaId) -> crate::Result<()> {
        self.remove_calls.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }
}

fn replica(id: i64) -> ReplicaIdentity {
    ReplicaIdentity {
        replica_id: ReplicaId::new(id),
        instance_id: ReplicaInstanceId::new(format!("pod-{id}")),
        agent_generation: AgentGeneration::new(format!("generation-{id}")),
    }
}

fn configuration() -> ReplicaConfigurationScope {
    ReplicaConfigurationScope {
        current: ConfigurationDescriptor::new(
            Epoch::new(0, 1),
            ReplicaId::new(1),
            vec![ConfigurationMember {
                identity: replica(1),
                role: ReplicaRole::Primary,
            }],
            1,
        ),
        previous: None,
        replicas: Vec::new(),
    }
}

fn m3_pair_configuration(
    fixture: &Fixture,
    number: i64,
    primary: i64,
) -> ReplicaConfigurationScope {
    let mut storage = fixture.goal.scope.storage.clone();
    storage.replica = replica(2);
    storage.storage = ReplicaStorageIdentity::Persistent {
        pod_uid: PodUid::new("pod-2"),
        pvc_uid: PvcUid::new("pvc-2"),
    };
    storage.storage_generation = StorageGeneration::new("storage-2");
    ReplicaConfigurationScope {
        current: ConfigurationDescriptor::new(
            Epoch::new(0, number),
            ReplicaId::new(primary),
            [1, 2]
                .into_iter()
                .map(|id| ConfigurationMember {
                    identity: replica(id),
                    role: if id == primary {
                        ReplicaRole::Primary
                    } else {
                        ReplicaRole::ActiveSecondary
                    },
                })
                .collect(),
            2,
        ),
        previous: None,
        replicas: vec![ReplicaPeerDescription {
            identity: replica(2),
            replication_address: "http://127.0.0.1:20002".into(),
            process_session_id: ProcessSessionId::new(uuid::Uuid::new_v4().to_string()),
            storage_incarnation: storage,
            role: if primary == 2 {
                ReplicaRole::Primary
            } else {
                ReplicaRole::ActiveSecondary
            },
            current_progress: 1,
            catch_up_capability: 1,
            is_up: true,
            is_ready: true,
            must_catchup: primary == 2,
        }],
    }
}

#[tokio::test]
async fn m3_ordinary_primary_recovery_drains_native_all_before_reentering_application_role() {
    use crate::host::state::PublicInstruction as I;
    let fixture = Fixture::new(1, false).await;
    let mut goal = fixture.goal.clone();
    goal.scope.configuration = m3_pair_configuration(&fixture, 2, 1);
    goal.scope.configuration.previous = Some(m3_pair_configuration(&fixture, 1, 1).current);
    goal.scope.epoch = Epoch::new(0, 2);
    goal.desired = ReplicaGoalKind::Reconfigure {
        role: ReplicaRole::Primary,
        catch_up: Some(PublicCatchUpMode::All),
    };
    goal.validate().unwrap();
    let actions = crate::host::public_lifecycle::program::operation_instructions(&goal.intent());
    assert_eq!(
        actions,
        [
            I::ReplicatorRole,
            I::ProgramEpoch,
            I::Configuration,
            I::FirstCatchUp,
            I::ApplicationRole,
            I::NativeDataLossDecision,
            I::NativeComplete,
        ]
    );
    goal.scope.configuration.previous.as_mut().unwrap().epoch = goal.scope.epoch;
    let ordinary = crate::host::public_lifecycle::program::operation_instructions(&goal.intent());
    assert_eq!(
        ordinary[2],
        I::ApplicationRole,
        "same-epoch configuration is not retained epoch recovery"
    );
    fixture.finish().await;
}

#[tokio::test]
async fn m3_ordinary_handoff_freezes_swap_and_revocation_does_not_withdraw_creation() {
    use crate::host::state::PublicInstruction as I;
    let fixture = Fixture::new(1, false).await;
    let mut goal = fixture.goal.clone();
    goal.operation_id = OperationId::new("source-handoff-projection");
    goal.revision += 1;
    goal.scope.configuration = m3_pair_configuration(&fixture, 2, 2);
    goal.scope.configuration.previous = Some(m3_pair_configuration(&fixture, 1, 1).current);
    goal.scope.epoch = Epoch::new(0, 2);
    goal.desired = ReplicaGoalKind::Reconfigure {
        role: ReplicaRole::ActiveSecondary,
        catch_up: Some(PublicCatchUpMode::All),
    };
    goal.validate().unwrap();
    let actions = crate::host::public_lifecycle::program::operation_instructions(&goal.intent());
    assert_eq!(
        actions,
        [
            I::Revoke,
            I::ProgramEpoch,
            I::RefreshedConfiguration,
            I::SecondCatchUp,
            I::ReplicatorRole,
            I::ApplicationRole,
            I::NativeComplete,
        ]
    );
    let PublicOperationProgram::Swap {
        refreshed, mode, ..
    } = crate::host::public_lifecycle::program::ordinary_handoff(&goal).unwrap()
    else {
        panic!("ordinary handoff must use existing Swap");
    };
    assert_eq!(refreshed.current.primary_id, ReplicaId::new(1));
    assert_eq!(refreshed.current.epoch, goal.scope.epoch);
    assert_eq!(mode, PublicCatchUpMode::All);
    assert!(
        refreshed
            .replicas
            .iter()
            .any(|peer| peer.identity == replica(2) && peer.must_catchup)
    );
    fixture.submit(&fixture.goal).await.unwrap();
    fixture.achieved(&fixture.goal).await;
    // Pure projection validation uses a controlled completed journal record;
    // the real PostgreSQL controller positives prove the physical effects.
    let state = fixture.store.load_state().await.unwrap();
    let mut record = state.public_v1_operation_state.as_ref().unwrap().operations
        [&fixture.goal.operation_id]
        .clone();
    let owner = record.ordinary.as_ref().unwrap().creation_owner.clone();
    record.intent = goal.intent();
    let mut frozen = crate::host::ordinary_goal::freeze(&state, &goal).unwrap();
    frozen.creation_owner = owner;
    frozen.accepted = true;
    frozen.host_settled = true;
    record.ordinary = Some(frozen);
    record.lifecycle.in_flight = None;
    record.lifecycle.outcomes = vec![
        crate::host::state::PublicInstructionOutcome::Done,
        crate::host::state::PublicInstructionOutcome::Done,
        crate::host::state::PublicInstructionOutcome::Done,
        crate::host::state::PublicInstructionOutcome::Done,
        crate::host::state::PublicInstructionOutcome::Done,
        crate::host::state::PublicInstructionOutcome::ApplicationRole(None),
        crate::host::state::PublicInstructionOutcome::NativeUnavailable,
    ];
    assert_eq!(
        crate::host::ordinary_goal::completed_handoff_receipts(&record),
        Some(SourceHandoffReceipt::ORDERED.to_vec())
    );
    let reject_projection = |change: fn(&mut crate::host::state::PublicOperationRecord)| {
        let mut changed = record.clone();
        change(&mut changed);
        assert!(crate::host::ordinary_goal::completed_handoff_receipts(&changed).is_none());
    };
    reject_projection(|record| record.ordinary.as_mut().unwrap().accepted = false);
    reject_projection(|record| record.ordinary.as_mut().unwrap().host_settled = false);
    reject_projection(|record| record.ordinary.as_mut().unwrap().action_format = 20);
    reject_projection(|record| record.lifecycle.in_flight = Some(I::SecondCatchUp));
    reject_projection(|record| {
        record.lifecycle.outcomes.pop();
    });
    reject_projection(|record| record.ordinary.as_mut().unwrap().actions.swap(3, 4));
    reject_projection(|record| {
        record.disposition = Some(crate::host::state::PublicOperationDisposition::Failed(
            "actual child effect failed".into(),
        ))
    });
    reject_projection(|record| record.superseded_by = Some(OperationId::new("later-owner")));
    let callbacks = fixture.runtime.public_lifecycle_callbacks().unwrap();
    (callbacks.revoke_access)(false).unwrap();
    assert!(!fixture.runtime.ordinary_withdrawn());
    (callbacks.revoke_access)(true).unwrap();
    assert!(fixture.runtime.ordinary_withdrawn());
    fixture.finish().await;
}

#[tokio::test]
async fn m3_primary_refresh_requires_actual_paired_roles_and_current_only_joint_proof() {
    let fixture = Fixture::new(1, false).await;
    let mut refresh = fixture.goal.clone();
    refresh.operation_id = OperationId::new("native-only-primary-refresh");
    refresh.revision += 1;
    refresh.desired = ReplicaGoalKind::RefreshPrimary { catch_up: None };
    let before = fixture.store.load_state().await.unwrap();
    assert!(fixture.submit(&refresh).await.is_err());
    assert_eq!(fixture.store.load_state().await.unwrap(), before);
    assert_eq!(fixture.probe.open_calls.load(Ordering::Acquire), 0);
    fixture.submit(&fixture.goal).await.unwrap();
    fixture.achieved(&fixture.goal).await;
    refresh.scope.epoch.configuration_number += 1;
    refresh.scope.configuration.current.epoch = refresh.scope.epoch;
    refresh.scope.configuration.current.configuration_id =
        refresh.scope.configuration.current.expected_id();
    let calls = fixture.probe.role_entered.available_permits();
    fixture.probe.block_role.store(true, Ordering::Release);
    fixture.submit(&refresh).await.unwrap();
    let completed = fixture.achieved(&refresh).await;
    assert_eq!(fixture.probe.role_entered.available_permits(), calls);
    assert_eq!(
        completed.publication.as_ref().unwrap().role,
        ReplicaRole::Primary
    );
    assert_ne!(completed.write_status, AccessStatus::Granted);
    let mut invalid_final = refresh;
    invalid_final.revision += 1;
    invalid_final.operation_id = OperationId::new("missing-joint-proof");
    invalid_final.desired = ReplicaGoalKind::RefreshPrimary {
        catch_up: Some(PublicCatchUpMode::WriteQuorum),
    };
    let before = fixture.store.load_state().await.unwrap();
    assert!(fixture.submit(&invalid_final).await.is_err());
    assert_eq!(fixture.store.load_state().await.unwrap(), before);
    assert_eq!(fixture.probe.role_entered.available_permits(), calls);
    fixture.finish().await;
}

struct Fixture {
    _root: tempfile::TempDir,
    store: Arc<SqliteStore>,
    runtime: Arc<PodRuntime>,
    service: AgentService<SqliteStore>,
    probe: Arc<Probe>,
    goal: ReplicaGoal,
    address: String,
    shutdown: watch::Sender<bool>,
    server: tokio::task::JoinHandle<Result<()>>,
    resolver: Arc<TestResolver>,
}

#[derive(Default)]
struct TestResolver(StdMutex<BTreeMap<ReplicaId, String>>);

impl crate::host::transport::ReplicaEndpointResolver for TestResolver {
    fn control_endpoint(&self, identity: &ReplicaIdentity) -> String {
        self.0
            .lock()
            .unwrap()
            .get(&identity.replica_id)
            .cloned()
            .unwrap_or_default()
    }
    fn replication_endpoint(&self, _: &ReplicaIdentity) -> String {
        String::new()
    }
}

impl Fixture {
    async fn new(id: i64, block_role: bool) -> Self {
        Self::with_policy(id, block_role, EffectivePolicy::fixed(1, 1).unwrap()).await
    }

    async fn with_policy(id: i64, block_role: bool, policy: EffectivePolicy) -> Self {
        let root = tempfile::Builder::new()
            .prefix(".ordinary-host-")
            .tempdir_in(
                std::env::var_os("TMPDIR")
                    .map(std::path::PathBuf::from)
                    .unwrap_or_else(|| std::env::current_dir().unwrap()),
            )
            .unwrap();
        let uid = ResourceUid::new("ordinary-resource");
        let local = replica(id);
        let storage = ReplicaStorageIncarnationContext {
            replica: local.clone(),
            storage: ReplicaStorageIdentity::Persistent {
                pod_uid: PodUid::new(format!("pod-{id}")),
                pvc_uid: PvcUid::new(format!("pvc-{id}")),
            },
            storage_generation: StorageGeneration::new(format!("storage-{id}")),
        };
        let identity = PublicV1Identity::new(1);
        let binding = PublicV1LifecycleBinding {
            identity: identity.clone(),
            resource_uid: uid.clone(),
            spec_generation: 1,
            state_persistence: StatePersistence::Persisted,
        };
        let store = Arc::new(
            SqliteStore::create_replacement_authorized(
                root.path().join("agent.sqlite3"),
                {
                    let mut state = AgentState::new(StorageIdentity {
                        schema_version: SCHEMA_VERSION,
                        resource_uid: uid.clone(),
                        pod_uid: PodUid::new(format!("pod-{id}")),
                        pvc_uid: PvcUid::new(format!("pvc-{id}")),
                        initialization_id: InitializationId::new(format!("init-{id}")),
                        local_identity: local.clone(),
                        effective_policy: policy,
                    });
                    state.application_storage =
                        Some(crate::host::state::ApplicationStorageBinding {
                            paths: BTreeMap::new(),
                            initializing: true,
                        });
                    state
                },
                binding.clone(),
            )
            .unwrap(),
        );
        let initialization =
            derive_history_initialization_id(&uid, PublicProtocolGeneration::new(1));
        let history =
            derive_bootstrap_history(&uid, PublicProtocolGeneration::new(1), &initialization);
        store
            .initialize_public_v1_history(&initialization, &history, &storage)
            .await
            .unwrap();
        let probe = Probe::new();
        probe.block_role.store(block_role, Ordering::Release);
        let runtime = Arc::new(PodRuntime::new_replacement(
            local,
            Arc::new(Application(probe.clone())),
            store.clone(),
            binding,
            "127.0.0.1:0".parse().unwrap(),
        ));
        runtime
            .configure_native_transport(Arc::from("ordinary-token"))
            .unwrap();
        let resolver = Arc::new(TestResolver::default());
        runtime
            .configure_discovery_resolver(resolver.clone())
            .unwrap();
        let service = AgentService::new(store.clone(), runtime.clone(), "ordinary-token").unwrap();
        runtime
            .bind_replica_session(uid, service.local_session().clone())
            .unwrap();
        service
            .public_operation_runtime(identity.clone())
            .await
            .unwrap();
        let check_runtime = Arc::downgrade(&runtime);
        *probe.on_abort.lock().unwrap() = Some(Box::new(move || {
            if let Some(runtime) = check_runtime.upgrade() {
                assert!(runtime.ordinary_withdrawn(), "Abort preceded withdrawal");
            }
        }));
        let goal = ReplicaGoal {
            format: ORDINARY_ACTION_FORMAT,
            identity,
            operation_id: OperationId::new(format!("prepare-{id}")),
            revision: 1,
            scope: ReplicaScope {
                history,
                storage,
                process_session_id: service.local_session().clone(),
                creation_id: runtime.replacement_replicator_creation_id().unwrap(),
                epoch: Epoch::new(0, 1),
                configuration: configuration(),
            },
            desired: ReplicaGoalKind::Prepare {
                rebuild: None,
                role: if id == 1 {
                    ReplicaRole::Primary
                } else {
                    ReplicaRole::IdleSecondary
                },
            },
        };
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = format!("http://{}", listener.local_addr().unwrap());
        resolver
            .0
            .lock()
            .unwrap()
            .insert(ReplicaId::new(id), address.clone());
        let (shutdown, shutdown_rx) = watch::channel(false);
        let (ready, mut ready_rx) = watch::channel(false);
        let server_service = service.clone();
        let server = tokio::spawn(async move {
            server_service
                .serve_control_only(listener, ready, shutdown_rx)
                .await
        });
        ready_rx.wait_for(|ready| *ready).await.unwrap();
        Self {
            _root: root,
            store,
            runtime,
            service,
            probe,
            goal,
            address,
            shutdown,
            server,
            resolver,
        }
    }

    fn link(&self, peer: &Self) {
        self.resolver.0.lock().unwrap().insert(
            peer.goal.scope.storage.replica.replica_id,
            peer.address.clone(),
        );
    }

    async fn submit(
        &self,
        goal: &ReplicaGoal,
    ) -> std::result::Result<ReplicaAgentSnapshot, tonic::Status> {
        let mut client =
            proto::agent_control_client::AgentControlClient::connect(self.address.clone())
                .await
                .unwrap();
        let reply = client
            .submit_goal(auth(proto::ReplicaGoalRequest {
                protocol_version: ORDINARY_ACTION_FORMAT,
                canonical_json: serde_json::to_vec(goal).unwrap(),
            }))
            .await?
            .into_inner();
        Ok(serde_json::from_slice(&reply.canonical_json).unwrap())
    }

    async fn snapshot(&self) -> ReplicaAgentSnapshot {
        self.snapshot_sampling(false).await
    }

    async fn snapshot_sampling(&self, sample_native_progress: bool) -> ReplicaAgentSnapshot {
        let mut client =
            proto::agent_control_client::AgentControlClient::connect(self.address.clone())
                .await
                .unwrap();
        let reply = client
            .get_snapshot(auth(proto::ReplicaSnapshotRequest {
                protocol_version: ORDINARY_ACTION_FORMAT,
                resource_uid: self.goal.scope.history.resource_uid.to_string(),
                target: Some(self.goal.scope.storage.replica.clone().into()),
                expected_process_session_id: self.goal.scope.process_session_id.to_string(),
                sample_native_progress,
            }))
            .await
            .unwrap()
            .into_inner();
        let ReplicaAgentQueryResult::Initialized(snapshot) =
            serde_json::from_slice(&reply.canonical_json).unwrap()
        else {
            panic!("initialized snapshot expected")
        };
        *snapshot
    }

    async fn achieved(&self, goal: &ReplicaGoal) -> ReplicaAgentSnapshot {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let snapshot = self.snapshot().await;
                if let Some(observation) = snapshot
                    .goals
                    .iter()
                    .find(|observation| observation.goal == *goal)
                {
                    assert!(
                        matches!(
                            observation.status,
                            ReplicaGoalStatus::Pending | ReplicaGoalStatus::Achieved
                        ),
                        "ordinary goal failed: {observation:?}"
                    );
                }
                if snapshot.goals.iter().any(|observation| {
                    observation.goal == *goal && observation.status == ReplicaGoalStatus::Achieved
                }) {
                    return snapshot;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap()
    }

    async fn finish(self) {
        self.probe.provider_epoch_gate.add_permits(100);
        self.probe.role_gate.add_permits(100);
        self.probe.factory_gate.add_permits(100);
        self.probe.close_gate.add_permits(100);
        self.probe.application_close_gate.add_permits(100);
        self.probe.build_gate.add_permits(100);
        self.probe.epoch_gate.add_permits(100);
        self.probe.configuration_gate.add_permits(100);
        self.probe.catch_up_gate.add_permits(100);
        self.runtime.withdraw_ordinary();
        self.shutdown.send_replace(true);
        tokio::time::timeout(Duration::from_secs(10), self.server)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        self.runtime.shutdown_and_drain().await.unwrap();
    }

    async fn acknowledge(&self, goal: &ReplicaGoal) {
        let mut client =
            proto::agent_control_client::AgentControlClient::connect(self.address.clone())
                .await
                .unwrap();
        client
            .acknowledge_goal(auth(proto::ReplicaGoalRequest {
                protocol_version: ORDINARY_ACTION_FORMAT,
                canonical_json: serde_json::to_vec(&ReplicaGoalAcknowledgement {
                    goal: goal.clone(),
                    logically_retired: false,
                })
                .unwrap(),
            }))
            .await
            .unwrap();
    }

    async fn failed(&self, goal: &ReplicaGoal) -> ReplicaAgentSnapshot {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let snapshot = self.snapshot().await;
                if snapshot.goals.iter().any(|observation| {
                    observation.goal == *goal
                        && matches!(
                            observation.status,
                            ReplicaGoalStatus::Failed(_) | ReplicaGoalStatus::Uncertain(_)
                        )
                }) {
                    return snapshot;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap()
    }

    async fn assert_reopens(&self) {
        let binding = PublicV1LifecycleBinding {
            identity: self.goal.identity.clone(),
            resource_uid: self.goal.scope.history.resource_uid.clone(),
            spec_generation: 1,
            state_persistence: StatePersistence::Persisted,
        };
        let reopened = SqliteStore::open_replacement_bound_existing(
            self._root.path().join("agent.sqlite3"),
            None,
            &binding,
        )
        .unwrap();
        assert_eq!(
            reopened.load_state().await.unwrap(),
            self.store.load_state().await.unwrap()
        );
    }
}

fn auth<T>(request: T) -> Request<T> {
    let mut request = Request::new(request);
    request
        .metadata_mut()
        .insert("authorization", "Bearer ordinary-token".parse().unwrap());
    request
}

async fn entered(semaphore: &Semaphore) {
    tokio::time::timeout(Duration::from_secs(10), semaphore.acquire())
        .await
        .unwrap()
        .unwrap()
        .forget();
}

#[tokio::test]
async fn ordinary_goal_actual_owner_is_durable_before_the_first_open_selection() {
    let fixture = Fixture::new(1, false).await;
    let intent = fixture.goal.intent();
    let selected = fixture
        .store
        .begin_public_operation(&intent, &[], &[])
        .await
        .unwrap();
    let crate::host::store::BeginPublicOperation::Ready(record) = selected else {
        panic!("initial ordinary goal was not selected Ready");
    };
    let owner = record
        .ordinary
        .as_ref()
        .unwrap()
        .creation_owner
        .as_ref()
        .unwrap();
    assert_eq!(owner.process_id, std::process::id());
    assert_eq!(
        owner.process_lifetime,
        crate::host::ordinary_goal::retained::process_lifetime_id()
    );
    assert_eq!(
        owner.process_session_id,
        fixture.goal.scope.process_session_id
    );
    assert_eq!(owner.creation_id, fixture.goal.scope.creation_id);
    assert!(owner.builtin_factory.is_none());
    assert!(record.lifecycle.in_flight.is_none() && record.lifecycle.outcomes.is_empty());
    assert_eq!(fixture.probe.open_calls.load(Ordering::Acquire), 0);
    fixture
        .store
        .advance_public_operation(
            &intent.operation_id,
            intent.revision,
            &intent.process_session_id,
            PublicOperationStage::Ready,
            PublicOperationStage::Running,
            None,
        )
        .await
        .unwrap();
    fixture
        .store
        .public_instruction(
            &intent,
            0,
            crate::host::state::PublicInstruction::ApplicationOpen,
            None,
        )
        .await
        .unwrap();
    let state = fixture.store.load_state().await.unwrap();
    let applied =
        &state.public_v1_operation_state.as_ref().unwrap().operations[&intent.operation_id];
    assert_eq!(
        applied.ordinary.as_ref().unwrap().creation_owner.as_ref(),
        Some(owner)
    );
    assert_eq!(
        applied.lifecycle.in_flight,
        Some(crate::host::state::PublicInstruction::ApplicationOpen)
    );
    assert_eq!(fixture.probe.open_calls.load(Ordering::Acquire), 0);
    let durable =
        SqliteStore::read_retained_recovery_state(&fixture._root.path().join("agent.sqlite3"))
            .unwrap();
    assert_eq!(
        durable
            .public_v1_operation_state
            .as_ref()
            .unwrap()
            .operations[&intent.operation_id]
            .ordinary
            .as_ref()
            .unwrap()
            .creation_owner
            .as_ref(),
        Some(owner)
    );
    fixture.finish().await;
}

#[tokio::test]
async fn ordinary_goal_missing_or_crossed_owner_rejects_reopen_without_mutation() {
    let fixture = Fixture::new(1, false).await;
    let intent = fixture.goal.intent();
    fixture
        .store
        .begin_public_operation(&intent, &[], &[])
        .await
        .unwrap();
    let original = fixture.store.load_state().await.unwrap();
    let mut cases = Vec::new();
    for kind in 0..6 {
        let mut state = original.clone();
        let ordinary = state
            .public_v1_operation_state
            .as_mut()
            .unwrap()
            .operations
            .get_mut(&intent.operation_id)
            .unwrap()
            .ordinary
            .as_mut()
            .unwrap();
        match kind {
            0 => ordinary.creation_owner = None,
            1 => ordinary.creation_owner.as_mut().unwrap().process_id = 0,
            2 => ordinary
                .creation_owner
                .as_mut()
                .unwrap()
                .process_lifetime
                .clear(),
            3 => {
                ordinary.creation_owner.as_mut().unwrap().process_session_id =
                    ProcessSessionId::new("foreign-process")
            }
            4 => {
                ordinary.creation_owner.as_mut().unwrap().creation_id =
                    ReplicatorCreationId::new("foreign-creation")
            }
            _ => {
                ordinary.creation_owner.as_mut().unwrap().builtin_factory =
                    Some(OperationId::new("foreign-factory-source"))
            }
        }
        cases.push(state);
    }
    let path = fixture._root.path().join("agent.sqlite3");
    let binding = PublicV1LifecycleBinding {
        identity: fixture.goal.identity.clone(),
        resource_uid: fixture.goal.scope.history.resource_uid.clone(),
        spec_generation: 1,
        state_persistence: StatePersistence::Persisted,
    };
    let write = |state: &AgentState| {
        let connection = rusqlite::Connection::open(&path).unwrap();
        connection
            .execute(
                "UPDATE agent_state SET state_json = ?1 WHERE singleton = 1",
                [serde_json::to_string(state).unwrap()],
            )
            .unwrap();
    };
    for state in cases {
        write(&state);
        let bytes = std::fs::read(&path).unwrap();
        let files = std::fs::read_dir(fixture._root.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<std::collections::BTreeSet<_>>();
        assert!(
            SqliteStore::open_replacement_bound_existing(path.clone(), None, &binding,).is_err()
        );
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        assert_eq!(
            std::fs::read_dir(fixture._root.path())
                .unwrap()
                .map(|entry| entry.unwrap().file_name())
                .collect::<std::collections::BTreeSet<_>>(),
            files
        );
        assert_eq!(fixture.probe.open_calls.load(Ordering::Acquire), 0);
    }
    write(&original);
    fixture.finish().await;
}

#[tokio::test]
async fn ordinary_goal_pending_discovery_is_receipted_callback_free_and_access_closed() {
    let source = Fixture::new(1, true).await;
    let target = Fixture::new(2, true).await;
    source.link(&target);
    target.link(&source);
    source.submit(&source.goal).await.unwrap();
    target.submit(&target.goal).await.unwrap();
    entered(&source.probe.role_entered).await;
    entered(&target.probe.role_entered).await;
    for (local, peer) in [(&source, &target), (&target, &source)] {
        let snapshot = local.snapshot_sampling(true).await;
        let publication = snapshot.publication.unwrap();
        assert_eq!(publication.stage, ReplicaPublicationStage::DiscoveryPending);
        assert_eq!(publication.role, ReplicaRole::None);
        tokio::net::TcpStream::connect(
            publication
                .replication_address
                .trim_start_matches("http://"),
        )
        .await
        .unwrap();
        let context = local.probe.context.lock().unwrap().clone().unwrap();
        let discovery = context
            .discover_peer(&ReplicaDiscoveryQuery {
                format: ORDINARY_ACTION_FORMAT,
                history: peer.goal.scope.history.clone(),
                replica: peer.goal.scope.storage.replica.clone(),
                process_session_id: None,
            })
            .await
            .unwrap()
            .unwrap();
        assert_eq!(discovery.stage, ReplicaPublicationStage::DiscoveryPending);
        assert_ne!(context.read_status().await.unwrap(), AccessStatus::Granted);
        assert_ne!(context.write_status().await.unwrap(), AccessStatus::Granted);
        assert_eq!(local.probe.progress_calls.load(Ordering::Acquire), 0);
        let mut control =
            proto::agent_control_client::AgentControlClient::connect(local.address.clone())
                .await
                .unwrap();
        tokio::time::timeout(
            Duration::from_secs(2),
            control.get_status(auth(proto::GetAgentStatusRequest::default())),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(local.probe.progress_calls.load(Ordering::Acquire), 0);
        let mut build = local.goal.clone();
        build.operation_id = OperationId::new("premature-build");
        build.revision = 2;
        build.desired = ReplicaGoalKind::Build {
            remote: Box::new(RemoteReplicaSelection {
                original_member: None,
                source: publication,
                target: discovery,
                target_control_address: peer.address.clone(),
                generation: 1,
            }),
        };
        assert!(local.submit(&build).await.is_err());
        assert_eq!(local.probe.build_calls.load(Ordering::Acquire), 0);
    }
    source.probe.role_gate.add_permits(1);
    target.probe.role_gate.add_permits(1);
    assert_eq!(
        source
            .achieved(&source.goal)
            .await
            .publication
            .unwrap()
            .stage,
        ReplicaPublicationStage::AchievedScope
    );
    assert_eq!(
        target
            .achieved(&target.goal)
            .await
            .publication
            .unwrap()
            .role,
        ReplicaRole::IdleSecondary
    );
    source.finish().await;
    target.finish().await;
}

#[cfg(feature = "testing")]
#[tokio::test]
async fn m3_scoped_instruction_cut_cannot_credit_another_remote_build_on_the_same_actor() {
    use crate::host::public_lifecycle::{PublicCutPosition, PublicOperationCut};
    use futures::FutureExt;
    let (source, target, remote) = remote_pair().await;
    let other = Fixture::new(3, false).await;
    source.link(&other);
    other.link(&source);
    other.submit(&other.goal).await.unwrap();
    let other_publication = other.achieved(&other.goal).await.publication.unwrap();
    let mut first = source.goal.clone();
    first.operation_id = OperationId::new("already-pending-build-a");
    first.revision = 2;
    first.desired = ReplicaGoalKind::Build {
        remote: Box::new(remote.clone()),
    };
    source.probe.block_build.store(true, Ordering::Release);
    source.submit(&first).await.unwrap();
    entered(&source.probe.build_entered).await;
    // A already owns a live callback. B will capture the newly installed cut.
    let cut = Arc::new(PublicOperationCut::for_instruction(
        &first,
        "Build",
        PublicCutPosition::InstructionSelected,
        0,
    ));
    source.runtime.set_public_operation_cut(Some(cut.clone()));
    source.probe.block_build.store(false, Ordering::Release);
    let mut second = source.goal.clone();
    second.operation_id = OperationId::new("different-remote-build-b");
    second.revision = 3;
    second.desired = ReplicaGoalKind::Build {
        remote: Box::new(RemoteReplicaSelection {
            original_member: None,
            source: remote.source,
            target: other_publication,
            target_control_address: other.address.clone(),
            generation: 1,
        }),
    };
    source.submit(&second).await.unwrap();
    source.achieved(&second).await;
    let snapshot = source.snapshot().await;
    let pending = snapshot
        .goals
        .iter()
        .find(|entry| entry.goal == first)
        .unwrap();
    assert_eq!(pending.status, ReplicaGoalStatus::Pending);
    assert_eq!(pending.selected_call.as_deref(), Some("Build"));
    assert_eq!(pending.receipts, 0);
    assert!(
        cut.entered.notified().now_or_never().is_none(),
        "B's identical name/index/prefix must not be attributed to pending A"
    );
    tokio::time::timeout(
        Duration::from_secs(1),
        cut.pause_instruction(
            PublicCutPosition::InstructionSelected,
            0,
            &first.intent(),
            crate::host::state::PublicInstruction::Remove,
        ),
    )
    .await
    .unwrap();
    assert!(cut.entered.notified().now_or_never().is_none());
    assert_eq!(source.probe.build_calls.load(Ordering::Acquire), 2);
    cut.release_all();
    source.runtime.set_public_operation_cut(None);
    source.probe.build_gate.add_permits(1);
    source.achieved(&first).await;
    source.finish().await;
    target.finish().await;
    other.finish().await;
}

#[cfg(feature = "testing")]
#[tokio::test]
async fn m3_instruction_cut_disarm_releases_all_captured_and_late_waiters() {
    use crate::host::public_lifecycle::{PublicCutPosition, PublicOperationCut};
    let cut = Arc::new(PublicOperationCut::new(
        PublicCutPosition::InstructionSelected,
        0,
    ));
    let first = tokio::spawn({
        let cut = cut.clone();
        async move { cut.pause(PublicCutPosition::InstructionSelected, 0).await }
    });
    tokio::time::timeout(Duration::from_secs(5), cut.entered.notified())
        .await
        .unwrap();
    let second = tokio::spawn({
        let cut = cut.clone();
        async move { cut.pause(PublicCutPosition::InstructionSelected, 0).await }
    });
    tokio::time::timeout(Duration::from_secs(5), cut.entered.notified())
        .await
        .unwrap();
    cut.release_all();
    tokio::time::timeout(Duration::from_secs(1), async {
        first.await.unwrap();
        second.await.unwrap();
    })
    .await
    .unwrap();
    tokio::time::timeout(
        Duration::from_secs(1),
        cut.pause(PublicCutPosition::InstructionSelected, 0),
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn ordinary_goal_build_is_retained_exact_two_ended_and_remove_is_local_only() {
    let source = Fixture::new(1, false).await;
    let target = Fixture::new(2, false).await;
    source.link(&target);
    target.link(&source);
    source.submit(&source.goal).await.unwrap();
    target.submit(&target.goal).await.unwrap();
    let source_publication = source.achieved(&source.goal).await.publication.unwrap();
    let target_publication = target.achieved(&target.goal).await.publication.unwrap();
    let selection = RemoteReplicaSelection {
        original_member: None,
        source: source_publication,
        target: target_publication,
        target_control_address: target.address.clone(),
        generation: 1,
    };
    let mut build = source.goal.clone();
    build.operation_id = OperationId::new("build");
    build.revision = 2;
    build.desired = ReplicaGoalKind::Build {
        remote: Box::new(selection.clone()),
    };
    let mut jumped = build.clone();
    if let ReplicaGoalKind::Build { remote } = &mut jumped.desired {
        remote.generation = 99;
    }
    assert!(source.submit(&jumped).await.is_err());
    assert_eq!(source.probe.build_calls.load(Ordering::Acquire), 0);
    source.probe.block_build.store(true, Ordering::Release);
    let observer = source.submit(&build).await.unwrap();
    drop(observer);
    entered(&source.probe.build_entered).await;
    assert_eq!(
        source.submit(&build).await.unwrap().remote_operations[0].state,
        RemoteReplicaState::InBuild
    );
    assert_eq!(source.probe.build_calls.load(Ordering::Acquire), 1);
    let mut changed = build.clone();
    changed.revision = 3;
    assert!(source.submit(&changed).await.is_err());
    source.probe.build_gate.add_permits(1);
    assert_eq!(
        source.achieved(&build).await.remote_operations[0].state,
        RemoteReplicaState::Ready
    );
    let context = target.probe.context.lock().unwrap().clone().unwrap();
    assert_ne!(context.write_status().await.unwrap(), AccessStatus::Granted);
    let mut remove = build.clone();
    remove.operation_id = OperationId::new("remove");
    remove.revision = 3;
    remove.desired = ReplicaGoalKind::Remove {
        remote: Box::new(selection.clone()),
    };
    source.submit(&remove).await.unwrap();
    source.achieved(&remove).await;
    assert_eq!(source.probe.remove_calls.load(Ordering::Acquire), 1);
    assert_eq!(target.probe.close_calls.load(Ordering::Acquire), 0);
    assert!(target.snapshot().await.publication.is_some());
    source.acknowledge(&remove).await;
    let accepted = source.store.load_state().await.unwrap();
    let watermark = accepted
        .public_v1_operation_state
        .as_ref()
        .unwrap()
        .remote_watermarks
        .get(&crate::host::ordinary_goal::remote_key(&selection))
        .copied();
    assert_eq!(watermark, Some((remove.scope.epoch, selection.generation)));
    let accepted_snapshot = source.snapshot().await;
    let target_snapshot = target.snapshot().await;
    assert!(target_snapshot.provider_writer_held);
    assert_eq!(
        source.runtime.provider_writer_lease_count(),
        1,
        "local retirement is not creation containment"
    );
    source.acknowledge(&remove).await;
    assert_eq!(source.store.load_state().await.unwrap(), accepted);
    assert_eq!(source.snapshot().await, accepted_snapshot);
    assert_eq!(target.snapshot().await, target_snapshot);
    assert!(
        accepted_snapshot
            .remote_operations
            .iter()
            .all(|remote| remote.state == RemoteReplicaState::Dropped)
    );
    assert_eq!(source.probe.remove_calls.load(Ordering::Acquire), 1);
    assert_eq!(source.probe.build_calls.load(Ordering::Acquire), 1);
    assert_eq!(source.probe.aborts.load(Ordering::Acquire), 0);
    assert_eq!(target.probe.aborts.load(Ordering::Acquire), 0);
    assert_eq!(target.probe.close_calls.load(Ordering::Acquire), 0);
    for case in [
        "creation", "process", "revision", "unknown", "storage", "build",
    ] {
        let mut invalid = remove.clone();
        match case {
            "creation" => {
                invalid.scope.creation_id = ReplicatorCreationId::new("stale-ack-creation")
            }
            "process" => {
                invalid.scope.process_session_id = ProcessSessionId::new("stale-ack-process")
            }
            "revision" => invalid.revision += 1,
            "unknown" => invalid.operation_id = OperationId::new("unknown-remove"),
            "storage" => invalid.scope.storage = target.goal.scope.storage.clone(),
            "build" => invalid = build.clone(),
            _ => unreachable!(),
        }
        assert!(
            source
                .service
                .acknowledge_ordinary_goal(proto::ReplicaGoalRequest {
                    protocol_version: ORDINARY_ACTION_FORMAT,
                    canonical_json: serde_json::to_vec(&ReplicaGoalAcknowledgement {
                        goal: invalid,
                        logically_retired: false,
                    })
                    .unwrap(),
                })
                .await
                .is_err(),
            "invalid {case} acknowledgement was accepted"
        );
        assert_eq!(source.store.load_state().await.unwrap(), accepted);
    }
    assert!(source.submit(&remove).await.is_err());
    assert!(source.submit(&build).await.is_err());
    let mut delayed = build.clone();
    delayed.operation_id = OperationId::new("delayed-build");
    delayed.revision = 4;
    assert!(source.submit(&delayed).await.is_err());
    source.finish().await;
    target.finish().await;
}

#[tokio::test]
async fn ordinary_goal_abort_retains_callback_close_service_and_provider_writer_until_actual_joins()
{
    let fixture = Fixture::new(1, true).await;
    fixture.probe.block_close.store(true, Ordering::Release);
    fixture
        .probe
        .block_application_close
        .store(true, Ordering::Release);
    fixture.submit(&fixture.goal).await.unwrap();
    entered(&fixture.probe.role_entered).await;
    assert_eq!(fixture.runtime.provider_writer_lease_count(), 1);
    let mut abort = fixture.goal.clone();
    abort.operation_id = OperationId::new("abort");
    abort.revision = 2;
    abort.desired = ReplicaGoalKind::Abort;
    fixture.submit(&abort).await.unwrap();
    entered(&fixture.probe.aborted).await;
    entered(&fixture.probe.close_entered).await;
    assert!(fixture.snapshot().await.publication.is_none());
    assert_eq!(fixture.runtime.provider_writer_lease_count(), 1);
    fixture.probe.close_gate.add_permits(1);
    entered(&fixture.probe.application_close_entered).await;
    assert_eq!(fixture.runtime.provider_writer_lease_count(), 1);
    fixture.probe.application_close_gate.add_permits(1);
    tokio::task::yield_now().await;
    assert_eq!(
        fixture.runtime.provider_writer_lease_count(),
        1,
        "held ordinary callback was dropped"
    );
    fixture.probe.role_gate.add_permits(1);
    fixture.achieved(&abort).await;
    assert_eq!(fixture.runtime.provider_writer_lease_count(), 0);
    assert_eq!(fixture.probe.remove_calls.load(Ordering::Acquire), 0);
    assert_eq!(fixture.probe.close_calls.load(Ordering::Acquire), 1);
    assert!(fixture.submit(&fixture.goal).await.is_err());
    fixture.finish().await;
}

#[tokio::test]
async fn ordinary_goal_failed_preparation_withdraws_before_abort_and_retains_failed_close() {
    let fixture = Fixture::new(1, true).await;
    fixture.probe.fail_role.store(true, Ordering::Release);
    fixture.probe.fail_close.store(true, Ordering::Release);
    fixture.probe.block_close.store(true, Ordering::Release);
    fixture.submit(&fixture.goal).await.unwrap();
    entered(&fixture.probe.role_entered).await;
    assert!(fixture.snapshot().await.publication.is_some());
    fixture.probe.role_gate.add_permits(1);
    entered(&fixture.probe.aborted).await;
    entered(&fixture.probe.close_entered).await;
    assert!(fixture.snapshot().await.publication.is_none());
    assert_eq!(fixture.runtime.provider_writer_lease_count(), 1);
    fixture.probe.close_gate.add_permits(1);
    entered(&fixture.probe.application_close_entered).await;
    let snapshot = fixture.snapshot().await;
    assert!(
        matches!(&snapshot.goals[0].status, ReplicaGoalStatus::Failed(error) if error.contains("preparation failed"))
    );
    assert_eq!(fixture.runtime.provider_writer_lease_count(), 1);
    assert!(fixture.runtime.shutdown_and_drain().await.is_err());
    assert!(fixture.runtime.shutdown_and_drain().await.is_err());
    assert_eq!(fixture.probe.close_calls.load(Ordering::Acquire), 1);
    fixture.shutdown.send_replace(true);
    let _ = fixture.server.await.unwrap();
}

#[tokio::test]
async fn ordinary_goal_format_identity_and_scope_rejections_precede_store_and_callback_mutation() {
    let fixture = Fixture::new(1, false).await;
    let before = fixture.store.load_state().await.unwrap();
    for change in 0..5 {
        let mut changed = fixture.goal.clone();
        match change {
            0 => changed.format -= 1,
            1 => changed.identity.protocol_version -= 1,
            2 => changed.scope.process_session_id = ProcessSessionId::new("stale"),
            3 => changed.scope.creation_id = ReplicatorCreationId::new("stale"),
            _ => changed.scope.storage.storage_generation = StorageGeneration::new("other"),
        }
        assert!(fixture.submit(&changed).await.is_err());
        assert_eq!(fixture.store.load_state().await.unwrap(), before);
        assert_eq!(fixture.probe.open_calls.load(Ordering::Acquire), 0);
    }
    fixture.submit(&fixture.goal).await.unwrap();
    fixture.achieved(&fixture.goal).await;
    let before = fixture.store.load_state().await.unwrap();
    let mut changed = fixture.goal.clone();
    changed.scope.epoch = Epoch::new(0, 0);
    changed.scope.configuration.current = ConfigurationDescriptor::new(
        changed.scope.epoch,
        ReplicaId::new(1),
        vec![ConfigurationMember {
            identity: replica(1),
            role: ReplicaRole::Primary,
        }],
        1,
    );
    changed.revision = 99;
    changed.operation_id = OperationId::new("lower-epoch");
    assert!(fixture.submit(&changed).await.is_err());
    let mut changed = fixture.goal.clone();
    changed.scope.configuration.current = ConfigurationDescriptor::new(
        changed.scope.epoch,
        ReplicaId::new(2),
        vec![
            ConfigurationMember {
                identity: replica(2),
                role: ReplicaRole::Primary,
            },
            ConfigurationMember {
                identity: replica(1),
                role: ReplicaRole::ActiveSecondary,
            },
        ],
        1,
    );
    changed.revision = 99;
    changed.operation_id = OperationId::new("different-primary");
    changed.desired = ReplicaGoalKind::Reconfigure {
        role: ReplicaRole::ActiveSecondary,
        catch_up: None,
    };
    assert!(fixture.submit(&changed).await.is_err());
    assert_eq!(fixture.store.load_state().await.unwrap(), before);
    assert_eq!(fixture.probe.open_calls.load(Ordering::Acquire), 1);
    fixture.finish().await;
}

#[tokio::test]
async fn ordinary_goal_reopen_preserves_selected_call_frozen_prefix_and_rejects_old_format_unchanged()
 {
    use crate::host::state::{PublicInstruction as I, PublicInstructionOutcome as O};
    let fixture = Fixture::new(1, false).await;
    let intent = fixture.goal.intent();
    fixture
        .store
        .begin_public_operation(&intent, &[], &[])
        .await
        .unwrap();
    fixture
        .store
        .advance_public_operation(
            &intent.operation_id,
            intent.revision,
            &intent.process_session_id,
            PublicOperationStage::Ready,
            PublicOperationStage::Running,
            None,
        )
        .await
        .unwrap();
    fixture
        .store
        .public_instruction(&intent, 0, I::ApplicationOpen, None)
        .await
        .unwrap();
    fixture
        .store
        .public_instruction(&intent, 0, I::ApplicationOpen, Some(O::Done))
        .await
        .unwrap();
    fixture
        .store
        .public_instruction(&intent, 1, I::ReplicatorOpen, None)
        .await
        .unwrap();
    let binding = PublicV1LifecycleBinding {
        identity: fixture.goal.identity.clone(),
        resource_uid: fixture.goal.scope.history.resource_uid.clone(),
        spec_generation: 1,
        state_persistence: StatePersistence::Persisted,
    };
    let database = fixture._root.path().join("agent.sqlite3");
    let reopened = SqliteStore::open_replacement_bound_existing(&database, None, &binding).unwrap();
    let before = fixture.store.load_state().await.unwrap();
    assert_eq!(reopened.load_state().await.unwrap(), before);
    let record = &before
        .public_v1_operation_state
        .as_ref()
        .unwrap()
        .operations[&intent.operation_id];
    assert_eq!(record.lifecycle.outcomes, vec![O::Done]);
    assert_eq!(record.lifecycle.in_flight, Some(I::ReplicatorOpen));
    assert!(!crate::host::public_lifecycle::replayable(record));
    assert!(
        crate::host::ordinary_goal::publication(
            &before,
            &ProcessSessionId::new("new-process"),
            &ReplicatorCreationId::new("new-creation"),
            false
        )
        .is_none()
    );
    assert_eq!(fixture.probe.open_calls.load(Ordering::Acquire), 0);
    let connection = rusqlite::Connection::open(&database).unwrap();
    connection.pragma_update(None, "user_version", 14).unwrap();
    drop(connection);
    let bytes = std::fs::read(&database).unwrap();
    assert!(SqliteStore::open_replacement_bound_existing(&database, None, &binding).is_err());
    assert_eq!(std::fs::read(&database).unwrap(), bytes);
    // No live call exists in this explicit selection/receipt-gap store fixture.
    fixture.shutdown.send_replace(true);
    let _ = fixture.server.await.unwrap();
}

#[tokio::test]
async fn ordinary_goal_builtin_factory_opens_actual_engine_listener_and_writer() {
    let fixture = Fixture::new(1, false).await;
    fixture
        .probe
        .use_default_factory
        .store(true, Ordering::Release);
    fixture.submit(&fixture.goal).await.unwrap();
    let snapshot = fixture.achieved(&fixture.goal).await;
    let publication = snapshot.publication.unwrap();
    assert_eq!(publication.stage, ReplicaPublicationStage::AchievedScope);
    assert_eq!(publication.role, ReplicaRole::Primary);
    tokio::net::TcpStream::connect(
        publication
            .replication_address
            .trim_start_matches("http://")
            .trim_end_matches('/'),
    )
    .await
    .unwrap();
    assert_eq!(fixture.runtime.provider_writer_lease_count(), 1);
    assert_eq!(fixture.probe.open_calls.load(Ordering::Acquire), 0);
    assert!(fixture.probe.listener.lock().unwrap().is_none());
    assert!(
        !serde_json::to_value(
            fixture
                .store
                .load_state()
                .await
                .unwrap()
                .public_v1_operation_state
                .unwrap()
        )
        .unwrap()
        .as_object()
        .unwrap()
        .contains_key("currentInboundBuildAuthority")
    );
    fixture.finish().await;
}

#[tokio::test]
async fn m3_retained_builtin_provenance_comes_from_registered_native_bundle_not_capabilities() {
    assert_eq!(
        ORDINARY_ACTION_FORMAT, 23,
        "requires the coordinated final matched format"
    );
    for builtin in [true, false] {
        let fixture = Fixture::new(1, false).await;
        fixture
            .probe
            .use_default_factory
            .store(builtin, Ordering::Release);
        fixture.submit(&fixture.goal).await.unwrap();
        fixture.achieved(&fixture.goal).await;
        let state = fixture.store.load_state().await.unwrap();
        let record = &state.public_v1_operation_state.as_ref().unwrap().operations
            [&fixture.goal.operation_id];
        assert_eq!(record.lifecycle.outcomes.iter().any(|outcome| matches!(outcome,
            crate::host::state::PublicInstructionOutcome::BuiltinNativeEndpoint {
                process_id, process_lifetime, ..
            } if *process_id == std::process::id()
                && process_lifetime == crate::host::ordinary_goal::retained::process_lifetime_id()
        )), builtin);
        assert!(record.ordinary.as_ref().unwrap().retained.is_none());
        fixture.finish().await;
    }
}

#[tokio::test]
async fn ordinary_goal_discovery_and_snapshot_require_authentication_before_mutation() {
    let fixture = Fixture::new(1, true).await;
    fixture.submit(&fixture.goal).await.unwrap();
    entered(&fixture.probe.role_entered).await;
    let before = fixture.store.load_state().await.unwrap();
    let mut client =
        proto::replica_peer_client::ReplicaPeerClient::connect(fixture.address.clone())
            .await
            .unwrap();
    let error = client
        .discover(proto::ReplicaDiscoveryRequest {
            protocol_version: ORDINARY_ACTION_FORMAT,
            canonical_json: serde_json::to_vec(&ReplicaDiscoveryQuery {
                format: ORDINARY_ACTION_FORMAT,
                history: fixture.goal.scope.history.clone(),
                replica: fixture.goal.scope.storage.replica.clone(),
                process_session_id: Some(fixture.goal.scope.process_session_id.clone()),
            })
            .unwrap(),
        })
        .await
        .unwrap_err();
    assert_eq!(error.code(), tonic::Code::Unauthenticated);
    assert_eq!(fixture.store.load_state().await.unwrap(), before);
    assert_eq!(fixture.probe.progress_calls.load(Ordering::Acquire), 0);
    fixture.finish().await;
}

#[tokio::test]
async fn m3_ordinary_primary_change_requires_strict_epoch_and_exact_creation() {
    let source = Fixture::new(1, false).await;
    let target = Fixture::new(2, false).await;
    let prior = source.goal.scope.clone();
    let mut next = prior.clone();
    let members = vec![
        ConfigurationMember {
            identity: prior.storage.replica.clone(),
            role: ReplicaRole::ActiveSecondary,
        },
        ConfigurationMember {
            identity: target.goal.scope.storage.replica.clone(),
            role: ReplicaRole::Primary,
        },
    ];
    next.configuration.current = ConfigurationDescriptor::new(
        prior.epoch,
        target.goal.scope.storage.replica.replica_id,
        members.clone(),
        2,
    );
    assert!(!prior.compatible_with(&next));
    next.epoch.configuration_number += 1;
    next.configuration.current = ConfigurationDescriptor::new(
        next.epoch,
        target.goal.scope.storage.replica.replica_id,
        members,
        2,
    );
    assert!(prior.compatible_with(&next));
    assert!(!next.compatible_with(&prior));
    for crossed in ["history", "storage", "process", "creation", "data-loss"] {
        let mut invalid = next.clone();
        match crossed {
            "history" => invalid.history.history_id = HistoryId::new("crossed-history"),
            "storage" => {
                invalid.storage.storage_generation = StorageGeneration::new("crossed-storage")
            }
            "process" => invalid.process_session_id = ProcessSessionId::new("other-process"),
            "creation" => invalid.creation_id = ReplicatorCreationId::new("other-creation"),
            "data-loss" => invalid.epoch.data_loss_number += 1,
            _ => unreachable!(),
        }
        assert!(!prior.compatible_with(&invalid), "{crossed}");
    }
    source.finish().await;
    target.finish().await;
}

#[tokio::test]
async fn m3_ordinary_explicit_sampling_preserves_native_query_outcomes_without_access() {
    let fixture = Fixture::new(1, false).await;
    fixture.submit(&fixture.goal).await.unwrap();
    fixture.achieved(&fixture.goal).await;
    for (progress, capability, expected_progress, expected_capability) in [
        (
            Ok(7),
            Ok(3),
            ProgressQueryResult::Value(7),
            ProgressQueryResult::Value(3),
        ),
        (
            Err(RuntimeError::ProgressNotReady),
            Err(RuntimeError::Application("capability failed".into())),
            ProgressQueryResult::NotReady,
            ProgressQueryResult::Failed("application error: capability failed".into()),
        ),
        (
            Ok(INVALID_LSN),
            Ok(INVALID_LSN),
            ProgressQueryResult::Value(INVALID_LSN),
            ProgressQueryResult::Value(INVALID_LSN),
        ),
    ] {
        *fixture.probe.progress_result.lock().unwrap() = Some(progress);
        *fixture.probe.capability_result.lock().unwrap() = Some(capability);
        let before = fixture.store.load_state().await.unwrap();
        let snapshot = fixture.snapshot_sampling(true).await;
        assert_eq!(snapshot.progress, expected_progress);
        assert_eq!(snapshot.capability, expected_capability);
        assert_ne!(snapshot.read_status, AccessStatus::Granted);
        assert_ne!(snapshot.write_status, AccessStatus::Granted);
        assert!(!snapshot.withdrawn);
        assert_eq!(fixture.store.load_state().await.unwrap(), before);
        let unsampled = fixture.snapshot().await;
        assert!(matches!(
            unsampled.progress,
            ProgressQueryResult::NotQueried(_)
        ));
        assert!(matches!(
            unsampled.capability,
            ProgressQueryResult::NotQueried(_)
        ));
    }
    assert_eq!(fixture.probe.progress_calls.load(Ordering::Acquire), 6);
    fixture.finish().await;
}

#[tokio::test]
async fn m3_ordinary_sampling_discards_results_when_publication_changes_during_query() {
    let fixture = Fixture::new(1, false).await;
    fixture.submit(&fixture.goal).await.unwrap();
    fixture.achieved(&fixture.goal).await;
    while let Ok(permit) = fixture.probe.role_entered.try_acquire() {
        permit.forget();
    }
    fixture.probe.block_progress.store(true, Ordering::Release);
    *fixture.probe.progress_result.lock().unwrap() = Some(Ok(41));
    *fixture.probe.capability_result.lock().unwrap() = Some(Ok(40));
    let mut next = fixture.goal.clone();
    next.operation_id = OperationId::new("m3-query-superseded-scope");
    next.revision += 1;
    next.scope.epoch.configuration_number += 1;
    next.scope.configuration.current = ConfigurationDescriptor::new(
        next.scope.epoch,
        fixture.goal.scope.storage.replica.replica_id,
        fixture.goal.scope.configuration.current.members.clone(),
        fixture.goal.scope.configuration.current.write_quorum,
    );
    next.desired = ReplicaGoalKind::Reconfigure {
        role: ReplicaRole::Primary,
        catch_up: None,
    };
    let change = async {
        entered(&fixture.probe.progress_entered).await;
        fixture.probe.block_role.store(true, Ordering::Release);
        fixture.submit(&next).await.unwrap();
        entered(&fixture.probe.role_entered).await;
        fixture.probe.progress_gate.add_permits(1);
    };
    let (snapshot, ()) = tokio::join!(fixture.snapshot_sampling(true), change);
    assert_eq!(
        snapshot.publication.as_ref().unwrap().stage,
        ReplicaPublicationStage::DiscoveryPending
    );
    assert!(matches!(
        snapshot.progress,
        ProgressQueryResult::NotQueried(_)
    ));
    assert!(matches!(
        snapshot.capability,
        ProgressQueryResult::NotQueried(_)
    ));
    assert_ne!(snapshot.write_status, AccessStatus::Granted);
    fixture.probe.role_gate.add_permits(1);
    fixture.achieved(&next).await;
    fixture.finish().await;
}

#[tokio::test]
async fn m3_ordinary_sampling_auth_identity_and_predecessor_format_reject_before_callbacks() {
    let fixture = Fixture::new(1, false).await;
    fixture.submit(&fixture.goal).await.unwrap();
    fixture.achieved(&fixture.goal).await;
    let before = fixture.store.load_state().await.unwrap();
    let request = proto::ReplicaSnapshotRequest {
        protocol_version: ORDINARY_ACTION_FORMAT,
        resource_uid: fixture.goal.scope.history.resource_uid.to_string(),
        target: Some(fixture.goal.scope.storage.replica.clone().into()),
        expected_process_session_id: fixture.goal.scope.process_session_id.to_string(),
        sample_native_progress: true,
    };
    let mut client =
        proto::agent_control_client::AgentControlClient::connect(fixture.address.clone())
            .await
            .unwrap();
    assert_eq!(
        client
            .get_snapshot(request.clone())
            .await
            .unwrap_err()
            .code(),
        tonic::Code::Unauthenticated
    );
    let mut stale = request.clone();
    stale.expected_process_session_id = "predecessor-process".into();
    assert_eq!(
        client.get_snapshot(auth(stale)).await.unwrap_err().code(),
        tonic::Code::FailedPrecondition
    );
    let mut predecessor_format = request;
    predecessor_format.protocol_version = 16;
    assert_eq!(
        client
            .get_snapshot(auth(predecessor_format))
            .await
            .unwrap_err()
            .code(),
        tonic::Code::FailedPrecondition
    );
    assert_eq!(fixture.probe.progress_calls.load(Ordering::Acquire), 0);
    assert_eq!(fixture.store.load_state().await.unwrap(), before);
    fixture.finish().await;
}

#[cfg(feature = "testing")]
#[tokio::test]
async fn ordinary_goal_open_return_without_durable_receipt_is_not_discoverable() {
    use crate::host::public_lifecycle::{PublicCutPosition, PublicOperationCut};
    let fixture = Fixture::new(1, true).await;
    let cut = Arc::new(PublicOperationCut::new(
        PublicCutPosition::CallbackSuccess,
        1,
    ));
    fixture.runtime.set_public_operation_cut(Some(cut.clone()));
    fixture.submit(&fixture.goal).await.unwrap();
    cut.entered.notified().await;
    assert_eq!(fixture.probe.open_calls.load(Ordering::Acquire), 1);
    assert!(fixture.snapshot().await.publication.is_none());
    cut.release.notify_one();
    entered(&fixture.probe.role_entered).await;
    assert_eq!(
        fixture.snapshot().await.publication.unwrap().stage,
        ReplicaPublicationStage::DiscoveryPending
    );
    fixture.probe.role_gate.add_permits(1);
    fixture.achieved(&fixture.goal).await;
    fixture.finish().await;
}

#[tokio::test]
async fn ordinary_goal_ready_requires_fresh_target_on_replay_acceptance_and_separate_admission() {
    let (source, target, selection) = remote_pair().await;
    let mut build = source.goal.clone();
    build.operation_id = OperationId::new("completed-build");
    build.revision = 2;
    build.desired = ReplicaGoalKind::Build {
        remote: Box::new(selection),
    };
    source.submit(&build).await.unwrap();
    source.achieved(&build).await;
    let mut reconfigure = target.goal.clone();
    reconfigure.operation_id = OperationId::new("target-new-publication");
    reconfigure.revision = 2;
    reconfigure.desired = ReplicaGoalKind::Reconfigure {
        role: ReplicaRole::IdleSecondary,
        catch_up: None,
    };
    target.submit(&reconfigure).await.unwrap();
    target.achieved(&reconfigure).await;
    assert!(
        source
            .snapshot()
            .await
            .remote_operations
            .iter()
            .all(|remote| remote.state != RemoteReplicaState::Ready)
    );
    assert!(source.submit(&build).await.is_err());
    let acknowledgement = ReplicaGoalAcknowledgement {
        goal: build,
        logically_retired: false,
    };
    let mut client =
        proto::agent_control_client::AgentControlClient::connect(source.address.clone())
            .await
            .unwrap();
    assert!(
        client
            .acknowledge_goal(auth(proto::ReplicaGoalRequest {
                protocol_version: ORDINARY_ACTION_FORMAT,
                canonical_json: serde_json::to_vec(&acknowledgement).unwrap(),
            }))
            .await
            .is_err()
    );
    assert_eq!(source.probe.build_calls.load(Ordering::Acquire), 1);
    source.finish().await;
    target.finish().await;
}

#[tokio::test]
async fn ordinary_goal_voter_admission_retires_idle_ownership_without_remove_or_target_containment()
{
    let (source, target, selection) = remote_pair().await;
    let mut build = source.goal.clone();
    build.operation_id = OperationId::new("admission-build");
    build.revision = 2;
    build.desired = ReplicaGoalKind::Build {
        remote: Box::new(selection),
    };
    source.submit(&build).await.unwrap();
    source.achieved(&build).await;
    let mut admit = source.goal.clone();
    admit.operation_id = OperationId::new("admission");
    admit.revision = 3;
    admit.scope.configuration.current = ConfigurationDescriptor::new(
        admit.scope.epoch,
        ReplicaId::new(1),
        vec![
            ConfigurationMember {
                identity: replica(1),
                role: ReplicaRole::Primary,
            },
            ConfigurationMember {
                identity: replica(2),
                role: ReplicaRole::ActiveSecondary,
            },
        ],
        1,
    );
    let target_publication = &build.remote().unwrap().target;
    admit.scope.configuration.replicas = vec![ReplicaPeerDescription {
        identity: target_publication.scope.storage.replica.clone(),
        replication_address: target_publication.replication_address.clone(),
        process_session_id: target_publication.scope.process_session_id.clone(),
        storage_incarnation: target_publication.scope.storage.clone(),
        role: ReplicaRole::ActiveSecondary,
        current_progress: INVALID_LSN,
        catch_up_capability: INVALID_LSN,
        is_up: true,
        is_ready: true,
        must_catchup: false,
    }];
    admit.desired = ReplicaGoalKind::Reconfigure {
        role: ReplicaRole::Primary,
        catch_up: Some(PublicCatchUpMode::WriteQuorum),
    };
    assert!(
        source.submit(&admit).await.is_err(),
        "unaccepted Ready admitted a voter"
    );
    let mut client =
        proto::agent_control_client::AgentControlClient::connect(source.address.clone())
            .await
            .unwrap();
    assert!(
        client
            .acknowledge_goal(auth(proto::ReplicaGoalRequest {
                protocol_version: ORDINARY_ACTION_FORMAT,
                canonical_json: serde_json::to_vec(&ReplicaGoalAcknowledgement {
                    goal: build.clone(),
                    logically_retired: true,
                })
                .unwrap(),
            }))
            .await
            .is_err(),
        "controller invented idle retirement without ordinary receipts"
    );
    client
        .acknowledge_goal(auth(proto::ReplicaGoalRequest {
            protocol_version: ORDINARY_ACTION_FORMAT,
            canonical_json: serde_json::to_vec(&ReplicaGoalAcknowledgement {
                goal: build,
                logically_retired: false,
            })
            .unwrap(),
        }))
        .await
        .unwrap();
    source.submit(&admit).await.unwrap();
    let snapshot = source.achieved(&admit).await;
    assert!(
        snapshot
            .remote_operations
            .iter()
            .all(|remote| remote.state == RemoteReplicaState::Dropped)
    );
    assert_eq!(source.probe.remove_calls.load(Ordering::Acquire), 0);
    assert_eq!(target.probe.close_calls.load(Ordering::Acquire), 0);
    assert_eq!(
        target.snapshot().await.publication.unwrap().role,
        ReplicaRole::IdleSecondary
    );
    source.finish().await;
    target.finish().await;
}

#[tokio::test]
async fn ordinary_goal_inbuild_cancellation_uses_whole_creation_abort_and_never_remove() {
    let (source, target, selection) = remote_pair().await;
    source.probe.block_build.store(true, Ordering::Release);
    source.probe.block_close.store(true, Ordering::Release);
    let mut build = source.goal.clone();
    build.operation_id = OperationId::new("inbuild");
    build.revision = 2;
    build.desired = ReplicaGoalKind::Build {
        remote: Box::new(selection.clone()),
    };
    source.submit(&build).await.unwrap();
    entered(&source.probe.build_entered).await;
    let mut remove = build.clone();
    remove.operation_id = OperationId::new("invalid-selective-remove");
    remove.revision = 3;
    remove.desired = ReplicaGoalKind::Remove {
        remote: Box::new(selection),
    };
    assert!(source.submit(&remove).await.is_err());
    let mut abort = source.goal.clone();
    abort.operation_id = OperationId::new("abort-inbuild");
    abort.revision = 4;
    abort.desired = ReplicaGoalKind::Abort;
    source.submit(&abort).await.unwrap();
    entered(&source.probe.aborted).await;
    entered(&source.probe.close_entered).await;
    assert!(source.snapshot().await.publication.is_none());
    assert_eq!(source.runtime.provider_writer_lease_count(), 1);
    assert_eq!(source.probe.remove_calls.load(Ordering::Acquire), 0);
    assert!(target.snapshot().await.publication.is_some());
    assert_eq!(target.probe.close_calls.load(Ordering::Acquire), 0);
    source.probe.build_gate.add_permits(1);
    source.probe.close_gate.add_permits(1);
    source.achieved(&abort).await;
    assert!(
        source
            .snapshot()
            .await
            .remote_operations
            .iter()
            .all(|remote| remote.state != RemoteReplicaState::Ready)
    );
    source.finish().await;
    target.finish().await;
}

#[tokio::test]
async fn ordinary_goal_creation_budget_never_reclaims_unaccepted_remote_outcomes() {
    use crate::host::state::{OrdinaryGoalRecord, PublicOperationRecord};
    let (source, target, selection) = remote_pair().await;
    let mut state = source.store.load_state().await.unwrap();
    for index in 0..MAX_REMOTE_SELECTIONS {
        let mut remote = selection.clone();
        let identity = replica(index as i64 + 2);
        remote.target.scope.storage.replica = identity;
        let mut goal = source.goal.clone();
        goal.operation_id = OperationId::new(format!("unaccepted-{index}"));
        goal.revision = index as u64 + 2;
        goal.desired = ReplicaGoalKind::Remove {
            remote: Box::new(remote),
        };
        let intent = goal.intent();
        let record = PublicOperationRecord {
            intent: intent.clone(),
            ordinary: Some(OrdinaryGoalRecord {
                native_input: None,
                action_format: ORDINARY_ACTION_FORMAT,
                actions: Vec::new(),
                open_existing: None,
                already_open: false,
                accepted: false,
                logically_retired: false,
                host_settled: true,
                ready_admissions: Vec::new(),
                retained: None,
                reconstruction: None,
                creation_owner: Some(crate::host::ordinary_goal::capture_creation_owner(
                    &state, &goal,
                )),
            }),
            attempt: 1,
            attempt_results: Default::default(),
            retry_failure: None,
            accepted_history: Some(goal.scope.history),
            accepted_storage: Some(goal.scope.storage),
            stage: PublicOperationStage::Completed,
            disposition: Some(PublicOperationDisposition::Succeeded),
            containment: crate::host::state::PublicOperationContainment::Complete,
            superseded_by: None,
            blockers: Default::default(),
            lifecycle: Default::default(),
        };
        state
            .public_v1_operation_state
            .as_mut()
            .unwrap()
            .operations
            .insert(intent.operation_id, record);
    }
    let mut incoming = source.goal.clone();
    incoming.operation_id = OperationId::new("creation-exhaustion");
    incoming.revision = u64::MAX;
    incoming.desired = ReplicaGoalKind::Remove {
        remote: Box::new(selection),
    };
    let before = state.clone();
    assert!(matches!(
        crate::host::ordinary_goal::validate_admission(&state, &incoming),
        Err(crate::host::HostError::Backpressure(_))
    ));
    assert_eq!(state, before);
    source.finish().await;
    target.finish().await;
}

#[tokio::test]
async fn ordinary_goal_late_factory_bundle_is_fenced_and_its_actual_close_is_joined() {
    let fixture = Fixture::new(1, false).await;
    fixture.probe.block_factory.store(true, Ordering::Release);
    fixture.probe.block_close.store(true, Ordering::Release);
    fixture.submit(&fixture.goal).await.unwrap();
    entered(&fixture.probe.factory_entered).await;
    let endpoint = fixture
        .probe
        .factory_listener
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .local_addr()
        .unwrap();
    let mut abort = fixture.goal.clone();
    abort.operation_id = OperationId::new("abort-factory");
    abort.revision = 2;
    abort.desired = ReplicaGoalKind::Abort;
    fixture.submit(&abort).await.unwrap();
    assert!(fixture.snapshot().await.publication.is_none());
    assert_eq!(fixture.probe.open_calls.load(Ordering::Acquire), 0);
    fixture.probe.factory_gate.add_permits(1);
    entered(&fixture.probe.close_entered).await;
    tokio::net::TcpStream::connect(endpoint).await.unwrap();
    assert!(
        fixture
            .snapshot()
            .await
            .goals
            .iter()
            .any(|goal| goal.goal == abort && goal.status == ReplicaGoalStatus::Pending)
    );
    assert!(fixture.submit(&fixture.goal).await.is_err());
    fixture.probe.close_gate.add_permits(1);
    fixture.achieved(&abort).await;
    assert!(tokio::net::TcpStream::connect(endpoint).await.is_err());
    assert_eq!(fixture.probe.close_calls.load(Ordering::Acquire), 1);
    fixture.finish().await;
}

#[tokio::test]
async fn ordinary_goal_late_factory_close_failure_is_sticky_containment_failure_not_callback_failure()
 {
    let fixture = Fixture::new(1, false).await;
    fixture.probe.block_factory.store(true, Ordering::Release);
    fixture.probe.block_close.store(true, Ordering::Release);
    fixture.probe.fail_close.store(true, Ordering::Release);
    fixture.submit(&fixture.goal).await.unwrap();
    entered(&fixture.probe.factory_entered).await;
    let mut abort = fixture.goal.clone();
    abort.operation_id = OperationId::new("late-failed-close-abort");
    abort.revision = 2;
    abort.desired = ReplicaGoalKind::Abort;
    fixture.submit(&abort).await.unwrap();
    fixture.probe.factory_gate.add_permits(1);
    entered(&fixture.probe.close_entered).await;
    fixture.probe.close_gate.add_permits(1);
    let snapshot = fixture.failed(&abort).await;
    assert!(snapshot.publication.is_none());
    for _ in 0..2 {
        let error = fixture.runtime.shutdown_and_drain().await.unwrap_err();
        assert!(
            error
                .to_string()
                .contains("unregistered ordinary creation Close failed"),
            "{error}"
        );
    }
    let state = fixture.store.load_state().await.unwrap();
    let record = &state.public_v1_operation_state.as_ref().unwrap().operations[&abort.operation_id];
    assert_ne!(
        record.containment,
        crate::host::state::PublicOperationContainment::Complete
    );
    assert_ne!(
        record.disposition,
        Some(PublicOperationDisposition::Succeeded)
    );
    assert_eq!(fixture.probe.aborts.load(Ordering::Acquire), 1);
    assert_eq!(fixture.probe.close_calls.load(Ordering::Acquire), 1);
    assert_eq!(fixture.probe.open_calls.load(Ordering::Acquire), 0);
    fixture.shutdown.send_replace(true);
    let _ = fixture.server.await.unwrap();
}

#[tokio::test]
async fn ordinary_goal_terminal_ids_reject_before_mutation_and_exact_retries_reopen() {
    for close_first in [true, false] {
        for fail_close in [false, true] {
            let fixture = Fixture::new(1, false).await;
            fixture.submit(&fixture.goal).await.unwrap();
            fixture.achieved(&fixture.goal).await;
            fixture.probe.block_close.store(true, Ordering::Release);
            fixture
                .probe
                .fail_close
                .store(fail_close, Ordering::Release);
            let mut terminal = fixture.goal.clone();
            terminal.operation_id = OperationId::new("canonical-terminal");
            terminal.revision = 2;
            terminal.desired = if close_first {
                ReplicaGoalKind::Close
            } else {
                ReplicaGoalKind::Abort
            };
            fixture.submit(&terminal).await.unwrap();
            entered(&fixture.probe.close_entered).await;
            let mut other = terminal.clone();
            other.operation_id = OperationId::new("new-terminal-id");
            other.revision = 3;
            other.desired = if close_first {
                ReplicaGoalKind::Abort
            } else {
                ReplicaGoalKind::Close
            };
            let mut changed_same_id = terminal.clone();
            changed_same_id.desired = other.desired.clone();
            for _ in 0..2 {
                fixture.submit(&terminal).await.unwrap();
                let before = fixture.store.load_state().await.unwrap();
                assert!(fixture.submit(&other).await.is_err());
                assert!(fixture.submit(&changed_same_id).await.is_err());
                assert_eq!(fixture.store.load_state().await.unwrap(), before);
                fixture.assert_reopens().await;
            }
            fixture.probe.close_gate.add_permits(1);
            if fail_close {
                fixture.failed(&terminal).await;
                assert!(fixture.runtime.shutdown_and_drain().await.is_err());
            } else {
                fixture.achieved(&terminal).await;
                fixture.runtime.shutdown_and_drain().await.unwrap();
            }
            for _ in 0..2 {
                let before = fixture.store.load_state().await.unwrap();
                let immutable = before
                    .public_v1_operation_state
                    .as_ref()
                    .unwrap()
                    .operations[&terminal.operation_id]
                    .clone();
                fixture.submit(&terminal).await.unwrap();
                assert!(fixture.submit(&other).await.is_err());
                assert!(fixture.submit(&changed_same_id).await.is_err());
                let after = fixture.store.load_state().await.unwrap();
                assert_eq!(
                    after.public_v1_operation_state.as_ref().unwrap().operations
                        [&terminal.operation_id],
                    immutable
                );
                assert!(
                    !after
                        .public_v1_operation_state
                        .as_ref()
                        .unwrap()
                        .operations
                        .contains_key(&other.operation_id)
                );
                fixture.assert_reopens().await;
            }
            assert_eq!(fixture.probe.close_calls.load(Ordering::Acquire), 1);
            assert_eq!(
                fixture.probe.aborts.load(Ordering::Acquire),
                usize::from(!close_first || fail_close)
            );
            fixture.shutdown.send_replace(true);
            let _ = fixture.server.await.unwrap();
        }
    }
}

fn catch_up_goal(source: &Fixture, revision: u64) -> ReplicaGoal {
    let mut goal = source.goal.clone();
    goal.operation_id = OperationId::new(format!("catch-up-{revision}"));
    goal.revision = revision;
    goal.desired = ReplicaGoalKind::Reconfigure {
        role: ReplicaRole::Primary,
        catch_up: Some(PublicCatchUpMode::WriteQuorum),
    };
    goal
}

async fn assert_access(fixture: &Fixture, expected: AccessStatus) {
    let context = fixture.probe.context.lock().unwrap().clone().unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if context.write_status().await.unwrap() == expected {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    if expected == AccessStatus::Granted {
        assert_eq!(context.read_status().await.unwrap(), AccessStatus::Granted);
    }
}

#[tokio::test]
async fn ordinary_goal_primary_activation_requires_current_only_successful_catch_up_and_quorum() {
    for outcome in ["held-success", "failed", "missing", "joint", "no-quorum"] {
        let fixture = Fixture::with_policy(
            1,
            false,
            if outcome == "no-quorum" {
                EffectivePolicy::fixed(3, 2).unwrap()
            } else {
                EffectivePolicy::fixed(1, 1).unwrap()
            },
        )
        .await;
        fixture.submit(&fixture.goal).await.unwrap();
        fixture.achieved(&fixture.goal).await;
        assert_access(&fixture, AccessStatus::ReconfigurationPending).await;
        let mut catch_up = catch_up_goal(&fixture, 2);
        if outcome == "joint" {
            catch_up.scope.configuration.previous =
                Some(catch_up.scope.configuration.current.clone());
            catch_up.scope.epoch = Epoch::new(0, 2);
            catch_up.scope.configuration.current = ConfigurationDescriptor::new(
                catch_up.scope.epoch,
                ReplicaId::new(1),
                catch_up.scope.configuration.current.members.clone(),
                1,
            );
        }
        if outcome == "missing" {
            catch_up.desired = ReplicaGoalKind::Reconfigure {
                role: ReplicaRole::Primary,
                catch_up: None,
            };
        }
        if outcome == "failed" {
            fixture.probe.fail_catch_up.store(true, Ordering::Release);
        }
        fixture
            .probe
            .block_catch_up
            .store(outcome == "held-success", Ordering::Release);
        fixture.submit(&catch_up).await.unwrap();
        if outcome == "held-success" {
            entered(&fixture.probe.catch_up_entered).await;
            assert_access(&fixture, AccessStatus::ReconfigurationPending).await;
            fixture.probe.catch_up_gate.add_permits(1);
        }
        if outcome == "failed" {
            fixture.failed(&catch_up).await;
            fixture.runtime.shutdown_and_drain().await.unwrap();
            assert_eq!(
                fixture.runtime.provider_writer_lease_count(),
                0,
                "ordinary callback failure poisoned successful real cleanup"
            );
        } else {
            fixture.achieved(&catch_up).await;
            assert_access(
                &fixture,
                match outcome {
                    "held-success" => AccessStatus::Granted,
                    "no-quorum" => AccessStatus::NoWriteQuorum,
                    _ => AccessStatus::ReconfigurationPending,
                },
            )
            .await;
            fixture.assert_reopens().await;
        }
        fixture.finish().await;
    }
}

#[tokio::test]
async fn m2_ordinary_creation_cleanup_is_retained_for_a_different_returned_control() {
    let fixture = Fixture::new(1, false).await;
    let replacement = Probe::new();
    replacement.block_close.store(true, Ordering::Release);
    fixture.probe.block_close.store(true, Ordering::Release);
    *fixture.probe.replacement_return.lock().unwrap() = Some(replacement.clone());
    fixture.submit(&fixture.goal).await.unwrap();
    entered(&replacement.close_entered).await;
    let address = replacement
        .listener
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .local_addr()
        .unwrap();
    let pending = fixture.snapshot().await;
    assert!(pending.publication.is_none() && pending.provider_writer_held);
    assert_ne!(pending.read_status, AccessStatus::Granted);
    assert_ne!(pending.write_status, AccessStatus::Granted);
    replacement.close_gate.add_permits(1);
    entered(&fixture.probe.close_entered).await;
    assert!(fixture.snapshot().await.provider_writer_held);
    fixture.probe.close_gate.add_permits(1);
    tokio::time::timeout(Duration::from_secs(10), async {
        while fixture.snapshot().await.provider_writer_held {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let settled = fixture.snapshot().await;
    assert!(settled.withdrawn && settled.publication.is_none());
    assert!(
        settled.goals.iter().any(|entry| entry.goal == fixture.goal
            && matches!(entry.status, ReplicaGoalStatus::Failed(_)))
    );
    assert_eq!(replacement.close_calls.load(Ordering::Acquire), 1);
    assert_eq!(fixture.probe.close_calls.load(Ordering::Acquire), 1);
    assert!(tokio::net::TcpStream::connect(address).await.is_err());
    fixture.finish().await;
}

#[tokio::test]
async fn m2_ordinary_creation_cleanup_failure_of_different_returned_control_is_sticky() {
    let fixture = Fixture::new(1, false).await;
    let replacement = Probe::new();
    replacement.fail_close.store(true, Ordering::Release);
    *fixture.probe.replacement_return.lock().unwrap() = Some(replacement.clone());
    fixture.submit(&fixture.goal).await.unwrap();
    fixture.failed(&fixture.goal).await;
    for _ in 0..2 {
        let error = fixture.runtime.shutdown_and_drain().await.unwrap_err();
        assert!(
            error
                .to_string()
                .contains("different returned Replicator Close failed"),
            "{error}"
        );
    }
    let snapshot = fixture.snapshot().await;
    assert!(snapshot.withdrawn && snapshot.publication.is_none() && snapshot.provider_writer_held);
    assert_eq!(replacement.close_calls.load(Ordering::Acquire), 1);
    assert_eq!(fixture.probe.close_calls.load(Ordering::Acquire), 1);
    fixture.shutdown.send_replace(true);
    let _ = fixture.server.await.unwrap();
}

#[tokio::test]
async fn ordinary_goal_build_does_not_replace_serving_primary_access_owner() {
    let (source, target, mut selection) = remote_pair().await;
    let catch_up = catch_up_goal(&source, 2);
    source.submit(&catch_up).await.unwrap();
    selection.source = source.achieved(&catch_up).await.publication.unwrap();
    assert_access(&source, AccessStatus::Granted).await;
    let mut control =
        proto::agent_control_client::AgentControlClient::connect(source.address.clone())
            .await
            .unwrap();
    let response = control
        .get_status(auth(proto::GetAgentStatusRequest::default()))
        .await
        .unwrap()
        .into_inner();
    let report = crate::control::normalize_public_v1_agent_status_report(response).unwrap();
    assert_eq!(report.role, ReplicaRole::Primary);
    assert_eq!(report.progress.role, PublicProgressRole::Primary);
    assert!(matches!(
        report.progress.current_progress,
        ProgressQueryResult::NotQueried(_)
    ));
    assert_eq!(report.write_status, AccessStatus::Granted);
    assert_eq!(source.probe.progress_calls.load(Ordering::Acquire), 0);
    let mut build = source.goal.clone();
    build.operation_id = OperationId::new("serving-build");
    build.revision = 3;
    build.desired = ReplicaGoalKind::Build {
        remote: Box::new(selection),
    };
    source.probe.block_build.store(true, Ordering::Release);
    source.submit(&build).await.unwrap();
    entered(&source.probe.build_entered).await;
    assert_access(&source, AccessStatus::Granted).await;
    let target_context = target.probe.context.lock().unwrap().clone().unwrap();
    assert_ne!(
        target_context.write_status().await.unwrap(),
        AccessStatus::Granted
    );
    source.probe.build_gate.add_permits(1);
    source.achieved(&build).await;
    assert_access(&source, AccessStatus::Granted).await;
    source.finish().await;
    target.finish().await;
}

fn admission_goal(
    source: &Fixture,
    selection: &RemoteReplicaSelection,
    revision: u64,
) -> ReplicaGoal {
    let mut goal = catch_up_goal(source, revision);
    goal.operation_id = OperationId::new(format!("admission-{revision}"));
    goal.scope.configuration.current = ConfigurationDescriptor::new(
        goal.scope.epoch,
        ReplicaId::new(1),
        vec![
            ConfigurationMember {
                identity: replica(1),
                role: ReplicaRole::Primary,
            },
            ConfigurationMember {
                identity: selection.target.scope.storage.replica.clone(),
                role: ReplicaRole::ActiveSecondary,
            },
        ],
        1,
    );
    goal.scope.configuration.replicas = vec![ReplicaPeerDescription {
        identity: selection.target.scope.storage.replica.clone(),
        replication_address: selection.target.replication_address.clone(),
        process_session_id: selection.target.scope.process_session_id.clone(),
        storage_incarnation: selection.target.scope.storage.clone(),
        role: ReplicaRole::ActiveSecondary,
        current_progress: INVALID_LSN,
        catch_up_capability: INVALID_LSN,
        is_up: true,
        is_ready: true,
        must_catchup: false,
    }];
    goal
}

#[tokio::test]
async fn ordinary_goal_admission_revalidates_after_role_epoch_and_configuration_awaits() {
    for boundary in ["role", "epoch", "configuration"] {
        let (source, target, selection) = remote_pair().await;
        let mut build = source.goal.clone();
        build.operation_id = OperationId::new("admission-race-build");
        build.revision = 2;
        build.desired = ReplicaGoalKind::Build {
            remote: Box::new(selection.clone()),
        };
        source.submit(&build).await.unwrap();
        source.achieved(&build).await;
        source.acknowledge(&build).await;
        source
            .probe
            .role_entered
            .forget_permits(source.probe.role_entered.available_permits());
        source
            .probe
            .epoch_entered
            .forget_permits(source.probe.epoch_entered.available_permits());
        source
            .probe
            .configuration_entered
            .forget_permits(source.probe.configuration_entered.available_permits());
        source
            .probe
            .block_role
            .store(boundary == "role", Ordering::Release);
        source
            .probe
            .block_epoch
            .store(boundary == "epoch", Ordering::Release);
        source
            .probe
            .block_configuration
            .store(boundary == "configuration", Ordering::Release);
        let admit = admission_goal(&source, &selection, 3);
        source.submit(&admit).await.unwrap();
        entered(match boundary {
            "role" => &source.probe.role_entered,
            "epoch" => &source.probe.epoch_entered,
            _ => &source.probe.configuration_entered,
        })
        .await;
        if boundary == "epoch" {
            let mut withdraw = target.goal.clone();
            withdraw.operation_id = OperationId::new("target-withdraw");
            withdraw.revision = 2;
            withdraw.desired = ReplicaGoalKind::Abort;
            target.submit(&withdraw).await.unwrap();
            target.achieved(&withdraw).await;
        } else {
            let mut republish = target.goal.clone();
            republish.operation_id = OperationId::new("target-republication");
            republish.revision = 2;
            republish.desired = ReplicaGoalKind::Reconfigure {
                role: ReplicaRole::IdleSecondary,
                catch_up: None,
            };
            target.submit(&republish).await.unwrap();
            target.achieved(&republish).await;
        }
        source.probe.role_gate.add_permits(1);
        source.probe.epoch_gate.add_permits(1);
        source.probe.configuration_gate.add_permits(1);
        let snapshot = source.failed(&admit).await;
        assert!(
            snapshot
                .goals
                .iter()
                .all(|observation| observation.goal != admit
                    || observation.status != ReplicaGoalStatus::Achieved)
        );
        source.runtime.shutdown_and_drain().await.unwrap();
        let state = source.store.load_state().await.unwrap();
        let preview = state.public_v1_operation_state.as_ref().unwrap();
        let record = &preview.operations[&admit.operation_id];
        assert_eq!(
            record.lifecycle.in_flight,
            Some(crate::host::state::PublicInstruction::Configuration)
        );
        assert!(
            !preview.operations[&build.operation_id]
                .ordinary
                .as_ref()
                .unwrap()
                .logically_retired
        );
        assert_eq!(
            source.probe.configuration_calls.load(Ordering::Acquire),
            if boundary == "configuration" { 2 } else { 1 }
        );
        let context = source.probe.context.lock().unwrap().clone().unwrap();
        assert_ne!(context.write_status().await.unwrap(), AccessStatus::Granted);
        assert!(source.snapshot().await.publication.is_none());
        source.assert_reopens().await;
        source.finish().await;
        target.finish().await;
    }
}

#[tokio::test]
async fn ordinary_goal_tombstone_remove_and_absent_targets_cannot_bypass_new_voter_ready() {
    for previous in ["absent", "tombstone", "built-removed"] {
        let (source, target, selection) = remote_pair().await;
        let mut revision = 2;
        if previous == "built-removed" {
            let mut build = source.goal.clone();
            build.operation_id = OperationId::new("removed-build");
            build.revision = revision;
            build.desired = ReplicaGoalKind::Build {
                remote: Box::new(selection.clone()),
            };
            source.submit(&build).await.unwrap();
            source.achieved(&build).await;
            source.acknowledge(&build).await;
            revision += 1;
        }
        if previous != "absent" {
            let mut remove = source.goal.clone();
            remove.operation_id = OperationId::new("voter-remove");
            remove.revision = revision;
            remove.desired = ReplicaGoalKind::Remove {
                remote: Box::new(selection.clone()),
            };
            source.submit(&remove).await.unwrap();
            source.achieved(&remove).await;
            revision += 1;
        }
        let before = source.store.load_state().await.unwrap();
        let admit = admission_goal(&source, &selection, revision);
        let mut substituted = admit.clone();
        substituted.scope.configuration.replicas[0] = ReplicaPeerDescription {
            identity: source.goal.scope.storage.replica.clone(),
            replication_address: selection.source.replication_address.clone(),
            process_session_id: source.goal.scope.process_session_id.clone(),
            storage_incarnation: source.goal.scope.storage.clone(),
            role: ReplicaRole::Primary,
            current_progress: INVALID_LSN,
            catch_up_capability: INVALID_LSN,
            is_up: true,
            is_ready: true,
            must_catchup: false,
        };
        let callback_revision = source.probe.role_entered.available_permits();
        assert!(
            source.submit(&substituted).await.is_err(),
            "{previous}: local descriptor substituted for remote voter"
        );
        assert_eq!(source.store.load_state().await.unwrap(), before);
        assert_eq!(
            source.probe.role_entered.available_permits(),
            callback_revision
        );
        assert!(
            source.submit(&admit).await.is_err(),
            "{previous} bypassed Ready"
        );
        assert_eq!(source.store.load_state().await.unwrap(), before);
        assert_eq!(source.probe.configuration_calls.load(Ordering::Acquire), 1);
        source.assert_reopens().await;
        source.finish().await;
        target.finish().await;
    }
}

#[tokio::test]
async fn ordinary_goal_admitted_voters_keep_config_authority_but_leaving_never_resurrects_ready() {
    let (source, target, selection) = remote_pair().await;
    let mut build = source.goal.clone();
    build.operation_id = OperationId::new("admitted-build");
    build.revision = 2;
    build.desired = ReplicaGoalKind::Build {
        remote: Box::new(selection.clone()),
    };
    source.submit(&build).await.unwrap();
    source.achieved(&build).await;
    source.acknowledge(&build).await;
    let admit = admission_goal(&source, &selection, 3);
    let mut stale_ready_epoch = admit.clone();
    stale_ready_epoch.scope.epoch = Epoch::new(0, 2);
    stale_ready_epoch.scope.configuration.current = ConfigurationDescriptor::new(
        stale_ready_epoch.scope.epoch,
        ReplicaId::new(1),
        stale_ready_epoch
            .scope
            .configuration
            .current
            .members
            .clone(),
        1,
    );
    let before = source.store.load_state().await.unwrap();
    assert!(source.submit(&stale_ready_epoch).await.is_err());
    assert_eq!(source.store.load_state().await.unwrap(), before);
    source.submit(&admit).await.unwrap();
    source.achieved(&admit).await;
    assert_access(&source, AccessStatus::Granted).await;
    source.assert_reopens().await;
    let mut compatible = admit.clone();
    compatible.operation_id = OperationId::new("compatible-admitted-config");
    compatible.revision = 4;
    source.submit(&compatible).await.unwrap();
    source.achieved(&compatible).await;
    assert_access(&source, AccessStatus::Granted).await;
    source.assert_reopens().await;
    let before = source.store.load_state().await.unwrap();
    let callbacks = source.probe.configuration_calls.load(Ordering::Acquire);
    let roles = source.probe.role_entered.available_permits();
    let mut substituted = compatible.clone();
    substituted.operation_id = OperationId::new("substituted-third-voter");
    substituted.revision = 5;
    let mut members = compatible.scope.configuration.current.members.clone();
    members.push(ConfigurationMember {
        identity: replica(3),
        role: ReplicaRole::ActiveSecondary,
    });
    substituted.scope.configuration.current =
        ConfigurationDescriptor::new(substituted.scope.epoch, ReplicaId::new(1), members, 1);
    substituted
        .scope
        .configuration
        .replicas
        .push(ReplicaPeerDescription {
            identity: source.goal.scope.storage.replica.clone(),
            replication_address: selection.source.replication_address.clone(),
            process_session_id: source.goal.scope.process_session_id.clone(),
            storage_incarnation: source.goal.scope.storage.clone(),
            role: ReplicaRole::Primary,
            current_progress: INVALID_LSN,
            catch_up_capability: INVALID_LSN,
            is_up: true,
            is_ready: true,
            must_catchup: false,
        });
    assert!(source.submit(&substituted).await.is_err());
    assert_eq!(source.store.load_state().await.unwrap(), before);
    assert_eq!(source.probe.role_entered.available_permits(), roles);
    assert_eq!(
        source.probe.configuration_calls.load(Ordering::Acquire),
        callbacks
    );
    let leave = catch_up_goal(&source, 5);
    source.submit(&leave).await.unwrap();
    source.achieved(&leave).await;
    assert_access(&source, AccessStatus::Granted).await;
    assert!(
        source
            .snapshot()
            .await
            .remote_operations
            .iter()
            .all(|remote| remote.state == RemoteReplicaState::Dropped)
    );
    let before = source.store.load_state().await.unwrap();
    let readd = admission_goal(&source, &selection, 6);
    assert!(source.submit(&readd).await.is_err());
    assert_eq!(source.store.load_state().await.unwrap(), before);
    let mut negative = selection.clone();
    negative.source = source.snapshot().await.publication.unwrap();
    negative.generation = 2;
    let mut tombstone = source.goal.clone();
    tombstone.operation_id = OperationId::new("newer-admission-watermark");
    tombstone.revision = 7;
    tombstone.desired = ReplicaGoalKind::Remove {
        remote: Box::new(negative),
    };
    source.submit(&tombstone).await.unwrap();
    source.achieved(&tombstone).await;
    source.acknowledge(&tombstone).await;
    assert!(
        source
            .store
            .load_state()
            .await
            .unwrap()
            .public_v1_operation_state
            .as_ref()
            .unwrap()
            .operations
            .contains_key(&build.operation_id),
        "compaction erased frozen admission evidence"
    );
    source.assert_reopens().await;
    source.finish().await;
    target.finish().await;
}

#[tokio::test]
async fn m3_consumed_build_removes_from_current_source_scope_once_without_renewing_ready() {
    let (source, target, selection) = remote_pair().await;
    let mut build = source.goal.clone();
    build.operation_id = OperationId::new("m3-consumed-build");
    build.revision = 2;
    build.desired = ReplicaGoalKind::Build {
        remote: Box::new(selection.clone()),
    };
    source.submit(&build).await.unwrap();
    source.achieved(&build).await;
    source.acknowledge(&build).await;
    let admit = admission_goal(&source, &selection, 3);
    source.submit(&admit).await.unwrap();
    source.achieved(&admit).await;
    let mut leave = catch_up_goal(&source, 4);
    leave.scope.epoch.configuration_number += 1;
    leave.scope.configuration.current = ConfigurationDescriptor::new(
        leave.scope.epoch,
        leave.scope.configuration.current.primary_id,
        leave.scope.configuration.current.members.clone(),
        leave.scope.configuration.current.write_quorum,
    );
    source.submit(&leave).await.unwrap();
    let live = source.achieved(&leave).await.publication.unwrap();
    let state = source.store.load_state().await.unwrap();
    assert!(
        state.public_v1_operation_state.as_ref().unwrap().operations[&build.operation_id]
            .ordinary
            .as_ref()
            .unwrap()
            .logically_retired
    );
    let mut remove = source.goal.clone();
    remove.operation_id = OperationId::new("m3-current-source-removal");
    remove.revision = 5;
    remove.scope = live.scope;
    remove.desired = ReplicaGoalKind::Remove {
        remote: Box::new(selection),
    };
    remove.validate().unwrap();
    source.submit(&remove).await.unwrap();
    source.achieved(&remove).await;
    source.submit(&remove).await.unwrap();
    source.achieved(&remove).await;
    assert_eq!(source.probe.remove_calls.load(Ordering::Acquire), 1);
    source.acknowledge(&remove).await;
    let before = source.store.load_state().await.unwrap();
    assert!(source.submit(&remove).await.is_err());
    source.acknowledge(&remove).await;
    assert_eq!(source.store.load_state().await.unwrap(), before);
    assert_eq!(target.probe.close_calls.load(Ordering::Acquire), 0);
    assert!(target.snapshot().await.publication.is_some());
    let mut duplicate = remove.clone();
    duplicate.operation_id = OperationId::new("m3-cannot-reinvoke-old-removal");
    duplicate.revision += 1;
    assert!(source.submit(&duplicate).await.is_err());
    assert!(source.submit(&build).await.is_err());
    assert_eq!(source.probe.build_calls.load(Ordering::Acquire), 1);
    assert_eq!(source.probe.remove_calls.load(Ordering::Acquire), 1);
    assert_eq!(source.store.load_state().await.unwrap(), before);
    assert!(
        source
            .snapshot()
            .await
            .remote_operations
            .iter()
            .all(|remote| { remote.state == RemoteReplicaState::Dropped })
    );
    source.assert_reopens().await;
    source.finish().await;
    target.finish().await;
}

#[tokio::test]
async fn ordinary_goal_matching_crash_image_opens_fresh_process_but_never_replays_interrupted_build()
 {
    let (source, target, selection) = remote_pair().await;
    source.probe.block_build.store(true, Ordering::Release);
    let mut build = source.goal.clone();
    build.operation_id = OperationId::new("crash-build");
    build.revision = 2;
    build.desired = ReplicaGoalKind::Build {
        remote: Box::new(selection),
    };
    source.submit(&build).await.unwrap();
    entered(&source.probe.build_entered).await;
    let root = tempfile::Builder::new()
        .prefix(".ordinary-crash-image-")
        .tempdir_in(
            std::env::var_os("TMPDIR")
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|| std::env::current_dir().unwrap()),
        )
        .unwrap();
    let database = root.path().join("agent.sqlite3");
    // The selected Build is held after its committed selection, with no open
    // SQLite transaction. This is an isolated crash-image fixture, not a claim
    // that killing a process is ordinary callback cancellation.
    std::fs::copy(source._root.path().join("agent.sqlite3"), &database).unwrap();
    let binding = PublicV1LifecycleBinding {
        identity: source.goal.identity.clone(),
        resource_uid: source.goal.scope.history.resource_uid.clone(),
        spec_generation: 1,
        state_persistence: StatePersistence::Persisted,
    };
    let store =
        Arc::new(SqliteStore::open_replacement_bound_existing(&database, None, &binding).unwrap());
    let probe = Probe::new();
    probe.block_role.store(true, Ordering::Release);
    let runtime = Arc::new(PodRuntime::new_replacement(
        source.goal.scope.storage.replica.clone(),
        Arc::new(Application(probe.clone())),
        store.clone(),
        binding,
        "127.0.0.1:0".parse().unwrap(),
    ));
    runtime
        .configure_native_transport(Arc::from("ordinary-token"))
        .unwrap();
    runtime
        .configure_discovery_resolver(source.resolver.clone())
        .unwrap();
    let service = AgentService::new(store.clone(), runtime.clone(), "ordinary-token").unwrap();
    runtime
        .bind_replica_session(
            source.goal.scope.history.resource_uid.clone(),
            service.local_session().clone(),
        )
        .unwrap();
    let registry = service
        .public_operation_runtime(source.goal.identity.clone())
        .await
        .unwrap();
    registry.recover_unowned().await.unwrap();
    let snapshot = service.ordinary_snapshot().await.unwrap();
    assert!(snapshot.publication.is_none());
    assert!(
        snapshot
            .goals
            .iter()
            .any(|observation| observation.goal == build
                && matches!(observation.status, ReplicaGoalStatus::Uncertain(_)))
    );
    let mut fresh = source.goal.clone();
    fresh.operation_id = OperationId::new("fresh-process-prepare");
    fresh.revision = 3;
    fresh.scope.process_session_id = service.local_session().clone();
    fresh.scope.creation_id = runtime.replacement_replicator_creation_id().unwrap();
    service
        .submit_ordinary_goal(proto::ReplicaGoalRequest {
            protocol_version: ORDINARY_ACTION_FORMAT,
            canonical_json: serde_json::to_vec(&fresh).unwrap(),
        })
        .await
        .unwrap();
    entered(&probe.role_entered).await;
    let publication = service
        .ordinary_snapshot()
        .await
        .unwrap()
        .publication
        .unwrap();
    assert_eq!(
        publication.scope.process_session_id,
        fresh.scope.process_session_id
    );
    assert_eq!(publication.scope.creation_id, fresh.scope.creation_id);
    assert_eq!(publication.stage, ReplicaPublicationStage::DiscoveryPending);
    assert_eq!(probe.open_calls.load(Ordering::Acquire), 1);
    assert_eq!(probe.build_calls.load(Ordering::Acquire), 0);
    assert_eq!(source.probe.build_calls.load(Ordering::Acquire), 1);
    probe.role_gate.add_permits(1);
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if service
                .ordinary_snapshot()
                .await
                .unwrap()
                .goals
                .iter()
                .any(|observation| {
                    observation.goal == fresh && observation.status == ReplicaGoalStatus::Achieved
                })
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    runtime.withdraw_ordinary();
    service
        .operation_runtime
        .lock()
        .await
        .take()
        .unwrap()
        .shutdown()
        .await
        .unwrap();
    runtime.shutdown_and_drain().await.unwrap();
    source.finish().await;
    target.finish().await;
}

#[tokio::test]
async fn ordinary_goal_discovery_stage_opens_unknown_peer_endpoints_before_frozen_role_recovery() {
    let source = Fixture::new(1, true).await;
    let target = Fixture::new(2, true).await;
    source.link(&target);
    target.link(&source);
    let topology = ConfigurationDescriptor::new(
        Epoch::new(0, 1),
        ReplicaId::new(1),
        vec![
            ConfigurationMember {
                identity: replica(1),
                role: ReplicaRole::Primary,
            },
            ConfigurationMember {
                identity: replica(2),
                role: ReplicaRole::ActiveSecondary,
            },
        ],
        1,
    );
    let mut opened = Vec::new();
    for fixture in [&source, &target] {
        let mut discovery = fixture.goal.clone();
        discovery.desired = ReplicaGoalKind::Discover;
        discovery.operation_id = OperationId::new(format!(
            "discover-{}",
            discovery.scope.storage.replica.replica_id
        ));
        discovery.scope.configuration.current = topology.clone();
        fixture.submit(&discovery).await.unwrap();
        let snapshot = fixture.achieved(&discovery).await;
        let publication = snapshot.publication.unwrap();
        assert_eq!(publication.stage, ReplicaPublicationStage::DiscoveryPending);
        let context = fixture.probe.context.lock().unwrap().clone().unwrap();
        assert_eq!(
            context.creation_id().unwrap(),
            publication.scope.creation_id
        );
        assert_eq!(
            context.local_publication().await.unwrap().unwrap(),
            publication
        );
        opened.push(publication);
    }
    let mut prepared = Vec::new();
    for (index, fixture) in [&source, &target].into_iter().enumerate() {
        let context = fixture.probe.context.lock().unwrap().clone().unwrap();
        let local = context.local_publication().await.unwrap().unwrap();
        let peer_identity = local
            .scope
            .configuration
            .current
            .members
            .iter()
            .find(|member| member.identity != local.scope.storage.replica)
            .unwrap()
            .identity
            .clone();
        let peer = context
            .discover_peer(&ReplicaDiscoveryQuery {
                format: ORDINARY_ACTION_FORMAT,
                history: local.scope.history.clone(),
                replica: peer_identity,
                process_session_id: None,
            })
            .await
            .unwrap()
            .unwrap();
        assert_eq!(peer.scope.storage, opened[1 - index].scope.storage);
        assert_eq!(
            peer.scope.process_session_id,
            opened[1 - index].scope.process_session_id
        );
        assert_eq!(peer.scope.creation_id, opened[1 - index].scope.creation_id);
        assert_eq!(
            peer.replication_address,
            opened[1 - index].replication_address
        );
        assert_eq!(peer.stage, ReplicaPublicationStage::DiscoveryPending);
        tokio::net::TcpStream::connect(peer.replication_address.trim_start_matches("http://"))
            .await
            .unwrap();
        let mut prepare = fixture.goal.clone();
        prepare.revision = 2;
        prepare.scope.configuration.current = topology.clone();
        prepare.scope.configuration.replicas = vec![ReplicaPeerDescription {
            identity: peer.scope.storage.replica.clone(),
            replication_address: peer.replication_address,
            process_session_id: peer.scope.process_session_id,
            storage_incarnation: peer.scope.storage,
            role: if index == 0 {
                ReplicaRole::ActiveSecondary
            } else {
                ReplicaRole::Primary
            },
            current_progress: INVALID_LSN,
            catch_up_capability: INVALID_LSN,
            is_up: true,
            is_ready: true,
            must_catchup: false,
        }];
        prepare.desired = ReplicaGoalKind::Prepare {
            rebuild: None,
            role: if index == 0 {
                ReplicaRole::Primary
            } else {
                ReplicaRole::ActiveSecondary
            },
        };
        fixture.submit(&prepare).await.unwrap();
        entered(&fixture.probe.role_entered).await;
        assert_eq!(fixture.probe.open_calls.load(Ordering::Acquire), 1);
        assert_eq!(
            fixture.snapshot().await.publication.unwrap().stage,
            ReplicaPublicationStage::DiscoveryPending
        );
        assert_ne!(context.write_status().await.unwrap(), AccessStatus::Granted);
        assert_eq!(fixture.probe.progress_calls.load(Ordering::Acquire), 0);
        prepared.push(prepare);
    }
    for (fixture, prepare) in [&source, &target].into_iter().zip(prepared) {
        fixture.probe.role_gate.add_permits(1);
        fixture.achieved(&prepare).await;
        let binding = PublicV1LifecycleBinding {
            identity: fixture.goal.identity.clone(),
            resource_uid: fixture.goal.scope.history.resource_uid.clone(),
            spec_generation: 1,
            state_persistence: StatePersistence::Persisted,
        };
        SqliteStore::open_replacement_bound_existing(
            fixture._root.path().join("agent.sqlite3"),
            None,
            &binding,
        )
        .unwrap();
    }
    source.finish().await;
    target.finish().await;
}

#[cfg(feature = "testing")]
#[tokio::test]
async fn ordinary_goal_lost_rpc_before_callback_launch_still_executes_one_retained_creation() {
    use crate::host::public_lifecycle::{PublicCutPosition, PublicOperationCut};
    let fixture = Fixture::new(1, true).await;
    let cut = Arc::new(PublicOperationCut::new(
        PublicCutPosition::BeforeAccessReservation,
        0,
    ));
    fixture.runtime.set_public_operation_cut(Some(cut.clone()));
    let mut client =
        proto::agent_control_client::AgentControlClient::connect(fixture.address.clone())
            .await
            .unwrap();
    let request = auth(proto::ReplicaGoalRequest {
        protocol_version: ORDINARY_ACTION_FORMAT,
        canonical_json: serde_json::to_vec(&fixture.goal).unwrap(),
    });
    let observer = tokio::spawn(async move { client.submit_goal(request).await });
    cut.entered.notified().await;
    assert!(
        fixture
            .store
            .load_state()
            .await
            .unwrap()
            .public_v1_operation_state
            .unwrap()
            .operations
            .contains_key(&fixture.goal.operation_id)
    );
    // Only the remote response observer is cancelled. The retained server-side
    // goal dispatcher has not yet started an ordinary callback.
    observer.abort();
    let _ = observer.await;
    fixture.runtime.set_public_operation_cut(None);
    cut.release.notify_one();
    entered(&fixture.probe.role_entered).await;
    fixture.submit(&fixture.goal).await.unwrap();
    assert_eq!(fixture.probe.open_calls.load(Ordering::Acquire), 1);
    fixture.probe.role_gate.add_permits(1);
    fixture.achieved(&fixture.goal).await;
    fixture.finish().await;
}

async fn remote_pair() -> (Fixture, Fixture, RemoteReplicaSelection) {
    let source = Fixture::new(1, false).await;
    let target = Fixture::new(2, false).await;
    source.link(&target);
    target.link(&source);
    source.submit(&source.goal).await.unwrap();
    target.submit(&target.goal).await.unwrap();
    let selection = RemoteReplicaSelection {
        original_member: None,
        source: source.achieved(&source.goal).await.publication.unwrap(),
        target: target.achieved(&target.goal).await.publication.unwrap(),
        target_control_address: target.address.clone(),
        generation: 1,
    };
    (source, target, selection)
}

#[tokio::test]
async fn m2_ordinary_rebuild_error_preserves_exact_goal_wire_and_reopen() {
    let (source, target, remote) = remote_pair().await;
    let mut goal = source.goal.clone();
    goal.operation_id = OperationId::new("ordinary-rebuild-cancelled-observer");
    goal.revision = 2;
    goal.desired = ReplicaGoalKind::Build {
        remote: Box::new(remote.clone()),
    };
    let failure = RemoteBuildTargetRebuildRequired {
        reason: ReplicaRebuildReason::RemoteCopyConflict,
        epoch: goal.scope.epoch,
        target: remote.target.scope.storage.replica.clone(),
        target_session: remote.target.scope.process_session_id.clone(),
    };
    let before = source.store.load_state().await.unwrap();
    let input = PublicBuildInput {
        attempt: goal.operation_id.clone(),
        epoch: goal.scope.epoch,
        current_configuration_id: Some(goal.scope.configuration.current.configuration_id.clone()),
        replica: failure.target.clone(),
        target_storage: remote.target.scope.storage.clone(),
        process_session_id: failure.target_session.clone(),
        source_address: remote.source.replication_address.clone(),
        replication_address: remote.target.replication_address.clone(),
        history: Some(goal.scope.history.clone()),
    };
    for dimension in ["target", "process", "epoch"] {
        let mut changed = failure.clone();
        match dimension {
            "target" => changed.target = replica(3),
            "process" => changed.target_session = ProcessSessionId::new("stale-target"),
            "epoch" => changed.epoch.configuration_number += 1,
            _ => unreachable!(),
        }
        assert!(
            crate::host::public_lifecycle::enrich_remote_rebuild(
                &before.identity,
                &goal.scope.storage,
                &goal.intent(),
                &input,
                ReplicaRebuildRequired::RemoteBuildTarget(Box::new(changed)),
            )
            .is_err()
        );
        assert_eq!(source.store.load_state().await.unwrap(), before);
    }
    *source.probe.build_rebuild.lock().unwrap() = Some(failure);
    let cut = Arc::new(crate::host::public_lifecycle::PublicOperationCut::new(
        crate::host::public_lifecycle::PublicCutPosition::InstructionApplied,
        0,
    ));
    source.runtime.set_public_operation_cut(Some(cut.clone()));
    source.submit(&goal).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), cut.entered.notified())
        .await
        .unwrap();
    let registry = source.service.registry.get().unwrap();
    let operation = registry
        .operation_for_validation(&goal.operation_id)
        .await
        .unwrap();
    let retained = source
        .store
        .public_operation_records()
        .await
        .unwrap()
        .into_iter()
        .find(|record| record.intent == goal.intent())
        .unwrap();
    let canonical = match retained.lifecycle.outcomes.as_slice() {
        [crate::host::state::PublicInstructionOutcome::RebuildRequired(error)] => error.clone(),
        outcomes => panic!("missing actual Build failure receipt: {outcomes:?}"),
    };
    assert!(
        matches!(canonical.as_ref(), CanonicalReplicaRebuildRequired::RemoteBuildTarget {
        goal: captured, ..
    } if captured.as_ref() == &goal)
    );
    let observer = tokio::spawn({
        let operation = operation.clone();
        async move { operation.cancel_root().await }
    });
    tokio::time::sleep(Duration::from_millis(30)).await;
    assert!(!observer.is_finished());
    observer.abort();
    assert!(observer.await.unwrap_err().is_cancelled());
    source.runtime.set_public_operation_cut(None);
    cut.release.notify_one();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let snapshot = source.snapshot().await;
            if snapshot.goals.iter().any(|entry| {
                entry.goal == goal && matches!(entry.status, ReplicaGoalStatus::Failed(_))
            }) && !snapshot.provider_writer_held
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(source.submit(&goal).await.is_err());
    assert_eq!(source.probe.build_calls.load(Ordering::Acquire), 1);
    let stored = source.store.load_state().await.unwrap();
    let preview = stored.public_v1_operation_state.as_ref().unwrap();
    let record = &preview.operations[&goal.operation_id];
    assert_eq!(record.lifecycle.outcomes, retained.lifecycle.outcomes);
    assert_ne!(
        record.disposition,
        Some(crate::host::state::PublicOperationDisposition::Succeeded)
    );
    let reopened = SqliteStore::open_replacement_bound_existing(
        source._root.path().join("agent.sqlite3"),
        Some(&stored.identity),
        preview.binding.as_ref().unwrap(),
    )
    .unwrap();
    assert_eq!(reopened.load_state().await.unwrap(), stored);
    let projection = source
        .runtime
        .replacement_report_projection(goal.scope.process_session_id.clone(), 1)
        .await
        .unwrap();
    let report = crate::host::public_lifecycle::public_v1_report(
        source.store.as_ref(),
        &goal.identity,
        projection,
    )
    .await
    .unwrap();
    assert!(report.completed_operations.iter().any(|completion| {
        matches!(&completion.result, PublicV1OperationResult::RebuildRequired(error) if error == &canonical)
    }));
    let envelope = crate::control::convert::public_v1_agent_status_to_proto(&report);
    assert_eq!(
        crate::control::convert::normalize_public_v1_agent_status_report(envelope.clone()).unwrap(),
        report
    );
    for dimension in [
        "operation",
        "revision",
        "source-process",
        "target-process",
        "creation",
        "storage",
        "endpoint",
        "generation",
    ] {
        let mut changed = envelope.clone();
        let failure = changed
            .public_v1
            .as_mut()
            .unwrap()
            .completed_operations
            .iter_mut()
            .find_map(|completion| completion.rebuild_required.as_mut())
            .unwrap();
        let mut captured: ReplicaGoal =
            serde_json::from_str(failure.goal_json.as_ref().unwrap()).unwrap();
        match dimension {
            "operation" => captured.operation_id = OperationId::new("crossed-operation"),
            "revision" => captured.revision += 1,
            "source-process" => {
                captured.scope.process_session_id = ProcessSessionId::new("crossed-source")
            }
            _ => {
                let ReplicaGoalKind::Build { remote } = &mut captured.desired else {
                    unreachable!()
                };
                match dimension {
                    "target-process" => {
                        remote.target.scope.process_session_id =
                            ProcessSessionId::new("crossed-target")
                    }
                    "creation" => {
                        remote.target.scope.creation_id =
                            ReplicatorCreationId::new("crossed-creation")
                    }
                    "storage" => {
                        remote.target.scope.storage.storage_generation =
                            StorageGeneration::new("crossed-storage")
                    }
                    "endpoint" => remote.target.replication_address = "http://127.0.0.1:1".into(),
                    "generation" => remote.generation += 1,
                    _ => unreachable!(),
                }
            }
        }
        failure.goal_json = Some(serde_json::to_string(&captured).unwrap());
        assert!(
            crate::control::convert::normalize_public_v1_agent_status_report(changed).is_err(),
            "{dimension}"
        );
    }
    source.finish().await;
    target.finish().await;
}

#[tokio::test]
async fn ordinary_goal_remote_republication_invalidates_late_success_and_exact_replay() {
    let (source, target, selection) = remote_pair().await;
    let mut build = source.goal.clone();
    build.operation_id = OperationId::new("held-build");
    build.revision = 2;
    build.desired = ReplicaGoalKind::Build {
        remote: Box::new(selection),
    };
    source.probe.block_build.store(true, Ordering::Release);
    source.submit(&build).await.unwrap();
    entered(&source.probe.build_entered).await;
    let mut reconfigure = target.goal.clone();
    reconfigure.operation_id = OperationId::new("target-republication");
    reconfigure.revision = 2;
    reconfigure.desired = ReplicaGoalKind::Reconfigure {
        role: ReplicaRole::IdleSecondary,
        catch_up: None,
    };
    target.submit(&reconfigure).await.unwrap();
    target.achieved(&reconfigure).await;
    source.probe.build_gate.add_permits(1);
    entered(&source.probe.aborted).await;
    let snapshot = source.snapshot().await;
    assert!(
        snapshot
            .remote_operations
            .iter()
            .all(|remote| remote.state != RemoteReplicaState::Ready)
    );
    assert!(source.submit(&build).await.is_err());
    assert_eq!(source.probe.build_calls.load(Ordering::Acquire), 1);
    source.finish().await;
    target.finish().await;
}

#[tokio::test]
async fn ordinary_goal_remove_tombstones_survive_reopen_and_budget_requires_safe_watermark() {
    let (source, target, selection) = remote_pair().await;
    let mut first = None;
    let mut last = None;
    for index in 1..=MAX_REMOTE_GENERATIONS {
        let mut remote = selection.clone();
        remote.generation = index as u64;
        let mut remove = source.goal.clone();
        remove.operation_id = OperationId::new(format!("tombstone-{index}"));
        remove.revision = index as u64 + 1;
        remove.desired = ReplicaGoalKind::Remove {
            remote: Box::new(remote),
        };
        source.submit(&remove).await.unwrap();
        source.achieved(&remove).await;
        first.get_or_insert(remove.clone());
        last = Some(remove);
    }
    assert_eq!(source.probe.remove_calls.load(Ordering::Acquire), 0);
    let mut exhausted = last.clone().unwrap();
    exhausted.operation_id = OperationId::new("exhausted");
    exhausted.revision += 1;
    if let ReplicaGoalKind::Remove { remote } = &mut exhausted.desired {
        remote.generation += 1;
    }
    assert_eq!(
        source.submit(&exhausted).await.unwrap_err().code(),
        tonic::Code::ResourceExhausted
    );
    for remove in [first.unwrap(), last.unwrap()] {
        source
            .service
            .store
            .acknowledge_ordinary_goal(&ReplicaGoalAcknowledgement {
                goal: remove,
                logically_retired: true,
            })
            .await
            .unwrap();
    }
    let binding = PublicV1LifecycleBinding {
        identity: source.goal.identity.clone(),
        resource_uid: source.goal.scope.history.resource_uid.clone(),
        spec_generation: 1,
        state_persistence: StatePersistence::Persisted,
    };
    let reopened = SqliteStore::open_replacement_bound_existing(
        source._root.path().join("agent.sqlite3"),
        None,
        &binding,
    )
    .unwrap();
    let state = reopened.load_state().await.unwrap();
    assert!(
        !state
            .public_v1_operation_state
            .as_ref()
            .unwrap()
            .remote_watermarks
            .is_empty()
    );
    let mut delayed = source.goal.clone();
    delayed.operation_id = OperationId::new("delayed-after-watermark");
    delayed.revision = 999;
    delayed.desired = ReplicaGoalKind::Build {
        remote: Box::new(selection),
    };
    assert!(source.submit(&delayed).await.is_err());
    assert_eq!(source.probe.build_calls.load(Ordering::Acquire), 0);
    source.finish().await;
    target.finish().await;
}

#[cfg(feature = "testing")]
async fn cut_entered(entered: &tokio::sync::Notify) {
    tokio::time::timeout(Duration::from_secs(10), entered.notified())
        .await
        .unwrap();
}

#[cfg(feature = "testing")]
#[tokio::test]
async fn ordinary_goal_cancelled_join_observer_keeps_held_build_and_sticky_failure_owned() {
    for panic_build in [false, true] {
        let (source, target, selection) = remote_pair().await;
        source.probe.block_build.store(true, Ordering::Release);
        source
            .probe
            .panic_build
            .store(panic_build, Ordering::Release);
        source.probe.block_close.store(true, Ordering::Release);
        source
            .probe
            .block_application_close
            .store(true, Ordering::Release);
        let mut build = source.goal.clone();
        build.operation_id = OperationId::new("cancelled-join-build");
        build.revision = 2;
        build.desired = ReplicaGoalKind::Build {
            remote: Box::new(selection),
        };
        source.submit(&build).await.unwrap();
        entered(&source.probe.build_entered).await;
        let registry = source.service.registry.get().unwrap().clone();
        let operation = registry
            .operation_for_validation(&build.operation_id)
            .await
            .unwrap();
        let joining = Arc::new(tokio::sync::Notify::new());
        operation.notify_ordinary_join(joining.clone());

        let before = source.store.load_state().await.unwrap();
        let error = tokio::time::timeout(
            Duration::from_secs(10),
            source
                .service
                .acknowledge_ordinary_goal(proto::ReplicaGoalRequest {
                    protocol_version: ORDINARY_ACTION_FORMAT,
                    canonical_json: serde_json::to_vec(&ReplicaGoalAcknowledgement {
                        goal: build.clone(),
                        logically_retired: false,
                    })
                    .unwrap(),
                }),
        )
        .await
        .unwrap()
        .unwrap_err();
        assert!(error.message().contains("unsettled"), "{error}");
        assert_eq!(source.store.load_state().await.unwrap(), before);

        // Exercise the shared join primitive even though premature RPC
        // acknowledgements now reject before reaching it.
        let observer_registry = registry.clone();
        let id = build.operation_id.clone();
        let observer = tokio::spawn(async move { observer_registry.join_ordinary_goal(&id).await });
        cut_entered(&joining).await;
        observer.abort();
        assert!(observer.await.unwrap_err().is_cancelled());

        let mut abort = source.goal.clone();
        abort.operation_id = OperationId::new("abort-cancelled-join");
        abort.revision = 3;
        abort.desired = ReplicaGoalKind::Abort;
        source.submit(&abort).await.unwrap();
        entered(&source.probe.aborted).await;
        entered(&source.probe.close_entered).await;
        source.probe.close_gate.add_permits(1);
        entered(&source.probe.close_returned).await;
        entered(&source.probe.application_close_entered).await;
        source.probe.application_close_gate.add_permits(1);
        entered(&source.probe.application_close_returned).await;
        cut_entered(&joining).await;
        assert_eq!(source.runtime.provider_writer_lease_count(), 1);
        let snapshot = source.snapshot().await;
        assert!(snapshot.publication.is_none());
        assert!(snapshot.withdrawn);
        assert!(snapshot.goals.iter().any(|observation| {
            observation.goal == abort && observation.status != ReplicaGoalStatus::Achieved
        }));
        source.probe.build_gate.add_permits(1);

        if panic_build {
            source.failed(&abort).await;
            let first = registry
                .join_ordinary_goal(&build.operation_id)
                .await
                .unwrap_err()
                .to_string();
            assert!(
                first.contains("injected ordinary Build owner panic"),
                "{first}"
            );
            assert_eq!(
                registry
                    .join_ordinary_goal(&build.operation_id)
                    .await
                    .unwrap_err()
                    .to_string(),
                first
            );
            for _ in 0..2 {
                let error = tokio::time::timeout(
                    Duration::from_secs(10),
                    source.runtime.shutdown_and_drain(),
                )
                .await
                .unwrap()
                .unwrap_err();
                assert!(error.to_string().contains("ordinary callback"), "{error}");
                assert_eq!(source.runtime.provider_writer_lease_count(), 1);
            }
            source.shutdown.send_replace(true);
            assert!(
                tokio::time::timeout(Duration::from_secs(10), source.server)
                    .await
                    .unwrap()
                    .unwrap()
                    .is_err()
            );
        } else {
            source.achieved(&abort).await;
            assert_eq!(source.runtime.provider_writer_lease_count(), 0);
            assert_eq!(source.probe.build_calls.load(Ordering::Acquire), 1);
            assert_eq!(source.probe.close_calls.load(Ordering::Acquire), 1);
            assert_eq!(source.probe.remove_calls.load(Ordering::Acquire), 0);
            source.finish().await;
        }
        target.finish().await;
    }
}

#[cfg(feature = "testing")]
#[tokio::test]
async fn ordinary_goal_lost_acknowledgement_after_join_entry_cannot_settle_build_early() {
    use crate::host::operation::RegistryTestCut;

    let (source, target, selection) = remote_pair().await;
    source.probe.block_build.store(true, Ordering::Release);
    source.probe.block_close.store(true, Ordering::Release);
    source
        .probe
        .block_application_close
        .store(true, Ordering::Release);
    let mut build = source.goal.clone();
    build.operation_id = OperationId::new("lost-ack-build");
    build.revision = 2;
    build.desired = ReplicaGoalKind::Build {
        remote: Box::new(selection),
    };
    source.submit(&build).await.unwrap();
    entered(&source.probe.build_entered).await;
    let registry = source.service.registry.get().unwrap().clone();
    let operation = registry
        .operation_for_validation(&build.operation_id)
        .await
        .unwrap();
    let root_return = Arc::new(RegistryTestCut::default());
    operation.set_ordinary_return_cut(root_return.clone());
    let joining = Arc::new(tokio::sync::Notify::new());
    operation.notify_ordinary_join(joining.clone());
    source.probe.build_gate.add_permits(1);
    cut_entered(&root_return.entered).await;
    source.achieved(&build).await;

    let acknowledgement = ReplicaGoalAcknowledgement {
        goal: build.clone(),
        logically_retired: true,
    };
    let before = source.store.load_state().await.unwrap();
    let error = tokio::time::timeout(
        Duration::from_secs(10),
        source
            .service
            .acknowledge_ordinary_goal(proto::ReplicaGoalRequest {
                protocol_version: ORDINARY_ACTION_FORMAT,
                canonical_json: serde_json::to_vec(&acknowledgement).unwrap(),
            }),
    )
    .await
    .unwrap()
    .unwrap_err();
    assert!(error.message().contains("cannot invent"), "{error}");
    assert_eq!(source.store.load_state().await.unwrap(), before);

    let service = source.service.clone();
    let request = proto::ReplicaGoalRequest {
        protocol_version: ORDINARY_ACTION_FORMAT,
        canonical_json: serde_json::to_vec(&ReplicaGoalAcknowledgement {
            logically_retired: false,
            ..acknowledgement
        })
        .unwrap(),
    };
    // Drop the server's actual acknowledgement future, not merely the network
    // client: it has entered the retained root join after durable completion.
    let observer = tokio::spawn(async move { service.acknowledge_ordinary_goal(request).await });
    cut_entered(&joining).await;
    observer.abort();
    assert!(observer.await.unwrap_err().is_cancelled());
    assert!(
        !source
            .store
            .load_state()
            .await
            .unwrap()
            .public_v1_operation_state
            .unwrap()
            .operations[&build.operation_id]
            .ordinary
            .as_ref()
            .unwrap()
            .accepted
    );
    let mut abort = source.goal.clone();
    abort.operation_id = OperationId::new("abort-lost-ack");
    abort.revision = 3;
    abort.desired = ReplicaGoalKind::Abort;
    source.submit(&abort).await.unwrap();
    entered(&source.probe.close_entered).await;
    source.probe.close_gate.add_permits(1);
    entered(&source.probe.close_returned).await;
    entered(&source.probe.application_close_entered).await;
    source.probe.application_close_gate.add_permits(1);
    entered(&source.probe.application_close_returned).await;
    cut_entered(&joining).await;
    assert_eq!(source.runtime.provider_writer_lease_count(), 1);
    let pending = source.snapshot().await;
    assert!(pending.publication.is_none());
    assert!(pending.goals.iter().any(|observation| {
        observation.goal == abort && observation.status != ReplicaGoalStatus::Achieved
    }));
    root_return.release.notify_one();
    source.achieved(&abort).await;
    assert_eq!(source.runtime.provider_writer_lease_count(), 0);
    assert_eq!(source.probe.build_calls.load(Ordering::Acquire), 1);
    assert_eq!(source.probe.close_calls.load(Ordering::Acquire), 1);
    assert_eq!(source.probe.remove_calls.load(Ordering::Acquire), 0);
    source.finish().await;
    target.finish().await;
}

#[cfg(feature = "testing")]
#[tokio::test]
async fn ordinary_goal_shutdown_releases_admission_before_joining_lost_rpc_dispatcher() {
    use crate::host::operation::RegistryTestCut;

    for fail_close in [false, true] {
        let fixture = Fixture::new(1, false).await;
        fixture.submit(&fixture.goal).await.unwrap();
        fixture.achieved(&fixture.goal).await;
        let configuration_calls = fixture.probe.configuration_calls.load(Ordering::Acquire);
        fixture
            .probe
            .fail_close
            .store(fail_close, Ordering::Release);
        fixture.probe.block_close.store(true, Ordering::Release);
        fixture
            .probe
            .block_application_close
            .store(true, Ordering::Release);
        let registry = fixture.service.registry.get().unwrap().clone();
        let mut goal = fixture.goal.clone();
        goal.operation_id = OperationId::new("shutdown-racing-dispatch");
        goal.revision = 2;
        goal.desired = ReplicaGoalKind::Reconfigure {
            role: ReplicaRole::Primary,
            catch_up: None,
        };
        let admission = Arc::new(RegistryTestCut::default());
        registry.set_admission_cut(goal.operation_id.clone(), admission.clone());
        let shutdown = Arc::new(RegistryTestCut::default());
        registry.set_shutdown_cut(shutdown.clone());
        let mut client =
            proto::agent_control_client::AgentControlClient::connect(fixture.address.clone())
                .await
                .unwrap();
        let request = auth(proto::ReplicaGoalRequest {
            protocol_version: ORDINARY_ACTION_FORMAT,
            canonical_json: serde_json::to_vec(&goal).unwrap(),
        });
        let observer = tokio::spawn(async move { client.submit_goal(request).await });
        cut_entered(&admission.entered).await;
        observer.abort();
        assert!(observer.await.unwrap_err().is_cancelled());
        let runtime = fixture
            .service
            .operation_runtime
            .lock()
            .await
            .take()
            .unwrap();
        let stopping = tokio::spawn(async move { runtime.shutdown().await });
        cut_entered(&shutdown.entered).await;
        assert!(registry.is_fenced());
        let before = fixture.store.load_state().await.unwrap();
        let mut late = goal.clone();
        late.operation_id = OperationId::new("post-shutdown-dispatch");
        late.revision = 3;
        let error = fixture
            .service
            .submit_ordinary_goal(proto::ReplicaGoalRequest {
                protocol_version: ORDINARY_ACTION_FORMAT,
                canonical_json: serde_json::to_vec(&late).unwrap(),
            })
            .await
            .unwrap_err();
        assert!(error.message().contains("shutting down"), "{error}");
        admission.release.notify_one();
        shutdown.release.notify_one();
        entered(&fixture.probe.close_entered).await;
        fixture.probe.close_gate.add_permits(1);
        entered(&fixture.probe.close_returned).await;
        entered(&fixture.probe.application_close_entered).await;
        assert!(!stopping.is_finished());
        assert_eq!(fixture.runtime.provider_writer_lease_count(), 1);
        fixture.probe.application_close_gate.add_permits(1);
        entered(&fixture.probe.application_close_returned).await;
        let result = tokio::time::timeout(Duration::from_secs(10), stopping)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(fixture.store.load_state().await.unwrap(), before);
        assert_eq!(
            fixture.probe.configuration_calls.load(Ordering::Acquire),
            configuration_calls
        );
        assert_eq!(fixture.probe.close_calls.load(Ordering::Acquire), 1);
        if fail_close {
            assert!(
                result
                    .unwrap_err()
                    .to_string()
                    .contains("ordinary Close failed")
            );
            for _ in 0..2 {
                assert!(
                    fixture
                        .runtime
                        .shutdown_and_drain()
                        .await
                        .unwrap_err()
                        .to_string()
                        .contains("ordinary Close failed")
                );
                assert_eq!(fixture.runtime.provider_writer_lease_count(), 1);
            }
        } else {
            result.unwrap();
            fixture.runtime.shutdown_and_drain().await.unwrap();
            assert_eq!(fixture.runtime.provider_writer_lease_count(), 0);
        }
        assert_eq!(fixture.probe.close_calls.load(Ordering::Acquire), 1);
        fixture.shutdown.send_replace(true);
        tokio::time::timeout(Duration::from_secs(10), fixture.server)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }
}

#[cfg(feature = "testing")]
#[tokio::test]
async fn ordinary_goal_stale_terminal_after_preflight_rejects_without_fencing_revision_or_epoch() {
    use crate::host::operation::RegistryTestCut;

    for stale_epoch in [false, true] {
        let fixture = Fixture::new(1, false).await;
        fixture.submit(&fixture.goal).await.unwrap();
        fixture.achieved(&fixture.goal).await;
        let registry = fixture.service.registry.get().unwrap().clone();
        let mut terminal = fixture.goal.clone();
        terminal.operation_id = OperationId::new("stale-preflight-terminal");
        terminal.revision = if stale_epoch { 5 } else { 3 };
        terminal.desired = ReplicaGoalKind::Abort;
        let admission = Arc::new(RegistryTestCut::default());
        registry.set_admission_cut(terminal.operation_id.clone(), admission.clone());
        let service = fixture.service.clone();
        let request = proto::ReplicaGoalRequest {
            protocol_version: ORDINARY_ACTION_FORMAT,
            canonical_json: serde_json::to_vec(&terminal).unwrap(),
        };
        let observer = tokio::spawn(async move { service.submit_ordinary_goal(request).await });
        cut_entered(&admission.entered).await;
        let mut newer = fixture.goal.clone();
        newer.operation_id = OperationId::new("accepted-after-terminal-preflight");
        newer.revision = 4;
        newer.desired = ReplicaGoalKind::Reconfigure {
            role: ReplicaRole::Primary,
            catch_up: None,
        };
        if stale_epoch {
            newer.scope.epoch = Epoch::new(0, 2);
            let current = &newer.scope.configuration.current;
            newer.scope.configuration.current = ConfigurationDescriptor::new(
                newer.scope.epoch,
                current.primary_id,
                current.members.clone(),
                current.write_quorum,
            );
        }
        fixture.submit(&newer).await.unwrap();
        fixture.achieved(&newer).await;
        let before = fixture.store.load_state().await.unwrap();
        let snapshot = fixture.snapshot().await;
        let configuration_calls = fixture.probe.configuration_calls.load(Ordering::Acquire);
        admission.release.notify_one();
        let error = tokio::time::timeout(Duration::from_secs(10), observer)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err();
        assert!(
            error.message().contains("revision or epoch is stale"),
            "{error}"
        );
        assert_eq!(fixture.store.load_state().await.unwrap(), before);
        assert_eq!(fixture.snapshot().await, snapshot);
        assert!(!registry.is_fenced());
        assert!(!fixture.runtime.ordinary_withdrawn());
        assert_eq!(fixture.probe.aborts.load(Ordering::Acquire), 0);
        assert_eq!(fixture.probe.close_calls.load(Ordering::Acquire), 0);
        assert_eq!(
            fixture.probe.configuration_calls.load(Ordering::Acquire),
            configuration_calls
        );
        assert!(
            registry
                .operation_for_validation(&terminal.operation_id)
                .await
                .is_none()
        );
        newer.operation_id = OperationId::new("valid-after-rejected-terminal");
        newer.revision = 6;
        fixture.submit(&newer).await.unwrap();
        fixture.achieved(&newer).await;
        assert_eq!(
            fixture.probe.configuration_calls.load(Ordering::Acquire),
            configuration_calls + 1
        );
        fixture.finish().await;
    }
}

#[cfg(feature = "testing")]
#[tokio::test]
async fn ordinary_goal_shutdown_joins_completion_and_terminal_owners_registered_during_drain() {
    use crate::host::operation::RegistryTestCut;

    let fixture = Fixture::new(1, false).await;
    fixture.submit(&fixture.goal).await.unwrap();
    fixture.achieved(&fixture.goal).await;
    fixture
        .probe
        .block_configuration
        .store(true, Ordering::Release);
    fixture.probe.block_close.store(true, Ordering::Release);
    fixture
        .probe
        .block_application_close
        .store(true, Ordering::Release);
    let registry = fixture.service.registry.get().unwrap().clone();
    let mut goal = fixture.goal.clone();
    goal.operation_id = OperationId::new("late-completion-registration");
    goal.revision = 2;
    goal.desired = ReplicaGoalKind::Reconfigure {
        role: ReplicaRole::Primary,
        catch_up: None,
    };
    let completion = Arc::new(RegistryTestCut::default());
    registry.set_completion_cut(goal.operation_id.clone(), completion.clone());
    let drain = Arc::new(RegistryTestCut::default());
    registry.set_drain_cut(drain.clone());
    let service = fixture.service.clone();
    let request = proto::ReplicaGoalRequest {
        protocol_version: ORDINARY_ACTION_FORMAT,
        canonical_json: serde_json::to_vec(&goal).unwrap(),
    };
    let observer = tokio::spawn(async move { service.submit_ordinary_goal(request).await });
    cut_entered(&completion.entered).await;
    entered(&fixture.probe.configuration_entered).await;
    observer.abort();
    assert!(observer.await.unwrap_err().is_cancelled());
    let runtime = fixture
        .service
        .operation_runtime
        .lock()
        .await
        .take()
        .unwrap();
    let stopping = tokio::spawn(async move { runtime.shutdown().await });
    entered(&fixture.probe.close_entered).await;
    fixture.probe.configuration_gate.add_permits(1);
    cut_entered(&drain.entered).await;
    // The initial drain snapshot already contains the dispatcher. Its retained
    // completion, and that completion's terminal recovery, do not exist yet.
    completion.release.notify_one();
    drain.release.notify_one();
    assert!(!stopping.is_finished());
    assert_eq!(fixture.runtime.provider_writer_lease_count(), 1);
    fixture.probe.close_gate.add_permits(1);
    entered(&fixture.probe.close_returned).await;
    entered(&fixture.probe.application_close_entered).await;
    assert!(!stopping.is_finished());
    fixture.probe.application_close_gate.add_permits(1);
    entered(&fixture.probe.application_close_returned).await;
    tokio::time::timeout(Duration::from_secs(10), stopping)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(registry.owners_drained().await);
    assert_eq!(fixture.runtime.provider_writer_lease_count(), 0);
    assert_eq!(fixture.probe.close_calls.load(Ordering::Acquire), 1);
    fixture.runtime.shutdown_and_drain().await.unwrap();
    fixture.shutdown.send_replace(true);
    tokio::time::timeout(Duration::from_secs(10), fixture.server)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}
