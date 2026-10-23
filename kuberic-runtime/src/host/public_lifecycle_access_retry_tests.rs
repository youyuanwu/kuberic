use super::*;
use crate::application::RoleChange;
use crate::host::operation::{PartitionOperation, PartitionOperationRegistry};
use crate::host::public_lifecycle::{
    PublicCallbackRetryProof, PublicLifecycleCallbacks, launch_bound_attempt,
};
use crate::host::sqlite_store::SqliteStore;
use crate::host::state::{
    AgentState, PublicCallbackFamily, PublicInstruction, PublicInstructionOutcome,
    PublicOperationDisposition, PublicOperationRecord, PublicOperationStage, StorageIdentity,
};
use crate::host::store::AgentStore;
use crate::protocol::public_operations::*;
use crate::protocol::types::*;
use crate::replicator::{
    ReplicaInformation, ReplicaSetConfiguration, ReplicaSetQuorumMode, ReplicatorFactory,
    ReplicatorInterfaces, ReplicatorSettings,
};
use std::sync::atomic::AtomicUsize;
use std::time::Duration;
use tokio::sync::Notify;

#[derive(Default)]
struct Probe {
    events: StdMutex<Vec<String>>,
    role: StdMutex<Option<ReplicaRole>>,
    fail_application: AtomicBool,
    block_catchup: AtomicBool,
    catchup_calls: AtomicUsize,
    catchup_entered: Notify,
    catchup_release: Notify,
}

impl Probe {
    fn event(&self, event: impl Into<String>) {
        self.events.lock().unwrap().push(event.into());
    }
}

#[async_trait]
impl Replicator for Probe {
    async fn open(&self) -> Result<String> {
        Ok("http://127.0.0.1:1".into())
    }
    async fn change_role(&self, _: Epoch, role: ReplicaRole) -> Result<()> {
        self.event(format!("replicator.{role:?}"));
        *self.role.lock().unwrap() = Some(role);
        Ok(())
    }
    async fn update_epoch(&self, _: Epoch) -> Result<()> {
        self.event("epoch");
        Ok(())
    }
    async fn close(&self) -> Result<()> {
        Ok(())
    }
    fn abort(&self) {}
    async fn current_progress(&self) -> Result<i64> {
        Ok(0)
    }
    async fn catch_up_capability(&self) -> Result<i64> {
        Ok(0)
    }
}

#[async_trait]
impl PrimaryReplicator for Probe {
    async fn on_data_loss(&self) -> Result<bool> {
        unreachable!("ordinary promotion never invokes data loss")
    }
    async fn update_catch_up_replica_set_configuration(
        &self,
        _: ReplicaSetConfiguration,
        _: ReplicaSetConfiguration,
    ) -> Result<()> {
        self.event("configuration");
        Ok(())
    }
    async fn update_current_replica_set_configuration(
        &self,
        _: ReplicaSetConfiguration,
    ) -> Result<()> {
        self.event("configuration");
        Ok(())
    }
    async fn wait_for_catch_up_quorum(&self, _: ReplicaSetQuorumMode) -> Result<()> {
        assert_eq!(*self.role.lock().unwrap(), Some(ReplicaRole::Primary));
        self.event("catchup");
        self.catchup_calls.fetch_add(1, Ordering::AcqRel);
        if self.block_catchup.load(Ordering::Acquire) {
            self.catchup_entered.notify_one();
            self.catchup_release.notified().await;
        }
        Ok(())
    }
    async fn build_replica(&self, _: ReplicaInformation) -> Result<()> {
        unreachable!("promotion does not Build")
    }
    async fn remove_replica(&self, _: ReplicaId) -> Result<()> {
        unreachable!("promotion does not Remove")
    }
}

struct Factory(Arc<Probe>);

#[async_trait]
impl ReplicatorFactory for Factory {
    async fn create_replicator(
        &self,
        _: ReplicatorFactoryContext,
        _: Option<Arc<dyn StateProvider>>,
        _: ReplicatorSettings,
    ) -> Result<ReplicatorInterfaces> {
        Ok(ReplicatorInterfaces::primary(
            self.0.clone(),
            None,
            ReplicatorCapabilities::conservative(),
        ))
    }
}

struct Application(Arc<Probe>);

#[async_trait]
impl StatefulServiceReplica for Application {
    async fn open(self: Arc<Self>, context: OpenContext) -> Result<Arc<dyn Replicator>> {
        Ok(context
            .partition
            .with_factory(Arc::new(Factory(self.0.clone())))
            .create_replicator(
                None,
                Some(ReplicatorSettings {
                    replication_address: "http://127.0.0.1:1".into(),
                }),
            )
            .await?
            .replicator())
    }
    async fn change_role(&self, role: ReplicaRole) -> Result<RoleChange> {
        assert_eq!(*self.0.role.lock().unwrap(), Some(role));
        self.0.event(format!("application.{role:?}"));
        if self.0.fail_application.load(Ordering::Acquire) {
            return Err(RuntimeError::Application(
                "declared quiescent application role failure".into(),
            ));
        }
        Ok(RoleChange {
            service_address: None,
        })
    }
    async fn close(&self) -> Result<()> {
        Ok(())
    }
    fn abort(&self) {}
}

struct Fixture {
    _root: tempfile::TempDir,
    runtime: PodRuntime,
    registry: Arc<PartitionOperationRegistry>,
    store: Arc<SqliteStore>,
    probe: Arc<Probe>,
}

impl Fixture {
    async fn new() -> Self {
        let root = tempfile::Builder::new()
            .prefix(".kuberic-access-retry-")
            .tempdir_in(std::env::current_dir().unwrap())
            .unwrap();
        let resource = ResourceUid::new("access-retry");
        let local = ReplicaIdentity {
            replica_id: ReplicaId::new(1),
            instance_id: ReplicaInstanceId::new("pod"),
            agent_generation: AgentGeneration::new("agent"),
        };
        let binding = PublicV1LifecycleBinding {
            identity: PublicV1Identity::new(6),
            resource_uid: resource.clone(),
            spec_generation: 1,
            state_persistence: StatePersistence::Persisted,
        };
        let store = Arc::new(
            SqliteStore::create_replacement_authorized(
                root.path().join("agent.sqlite3"),
                AgentState::new(StorageIdentity {
                    schema_version: crate::host::state::SCHEMA_VERSION,
                    resource_uid: resource.clone(),
                    pod_uid: PodUid::new("pod"),
                    pvc_uid: PvcUid::new("pvc"),
                    initialization_id: InitializationId::new("initialization"),
                    local_identity: local.clone(),
                    effective_policy: EffectivePolicy::fixed(1, 30).unwrap(),
                }),
                binding.clone(),
            )
            .unwrap(),
        );
        let generation = PublicProtocolGeneration::new(6);
        let initialization = derive_history_initialization_id(&resource, generation);
        let history = derive_bootstrap_history(&resource, generation, &initialization);
        store
            .initialize_public_v1_history(
                &initialization,
                &history,
                &ReplicaStorageIncarnationContext {
                    replica: local.clone(),
                    storage: ReplicaStorageIdentity::Persistent {
                        pod_uid: PodUid::new("pod"),
                        pvc_uid: PvcUid::new("pvc"),
                    },
                    storage_generation: StorageGeneration::new("storage"),
                },
            )
            .await
            .unwrap();
        let session = ProcessSessionId::new("session");
        let registry = PartitionOperationRegistry::new(
            store.clone(),
            binding.identity.clone(),
            session.clone(),
        )
        .unwrap();
        let probe = Arc::new(Probe::default());
        let application = Arc::new(Application(probe.clone()));
        let runtime = PodRuntime::new_replacement(
            local,
            application.clone(),
            store.clone(),
            binding,
            "127.0.0.1:1".parse().unwrap(),
        );
        runtime.bind_replica_session(resource, session).unwrap();
        application
            .open(runtime.public_open_context(OpenMode::Existing).await)
            .await
            .unwrap();
        Self {
            _root: root,
            runtime,
            registry,
            store,
            probe,
        }
    }

    fn configuration(&self, epoch: Epoch) -> PublicConfiguration {
        let local = self.runtime.host.identity.clone();
        PublicConfiguration {
            current: ConfigurationDescriptor::new(
                epoch,
                local.replica_id,
                vec![ConfigurationMember {
                    identity: local,
                    role: ReplicaRole::Primary,
                }],
                1,
            ),
            previous: None,
            replicas: Vec::new(),
        }
    }

    fn intent(&self, program: PublicOperationProgram, revision: u64) -> PublicOperationIntent {
        PublicOperationIntent {
            identity: PublicV1Identity::new(6),
            operation_id: OperationId::new(format!("access-retry-{revision}")),
            revision,
            process_session_id: ProcessSessionId::new("session"),
            class: PublicOperationClass::Authority,
            input_digest: program.digest(),
            replay_disposition: program.replay_disposition(),
            lifecycle: None,
            program: Some(program),
        }
    }

    fn promotion(&self, epoch: Epoch, revision: u64) -> PublicOperationIntent {
        let configuration = self.configuration(epoch);
        let input = PublicLifecycleInput {
            recipe: PublicLifecycleRecipe::FailoverPromotion,
            replica: self.runtime.host.identity.clone(),
            epoch,
            possible_data_loss: PossibleDataLossIntent::NotPossible,
            current: configuration.current,
            previous: Some(
                self.configuration(Epoch::new(
                    epoch.data_loss_number,
                    epoch.configuration_number - 1,
                ))
                .current,
            ),
            replicas: Vec::new(),
            data_loss_recovery: None,
        };
        let mut intent = self.intent(PublicOperationProgram::Epoch { epoch }, revision);
        intent.input_digest = input.digest();
        intent.lifecycle = Some(input);
        intent.program = None;
        intent
    }

    fn callbacks(&self) -> PublicLifecycleCallbacks {
        let mut callbacks = self.runtime.public_lifecycle_callbacks().unwrap();
        assert!(callbacks.access_factory.is_some());
        callbacks.retry_proof =
            PublicCallbackRetryProof::declared([PublicCallbackFamily::ApplicationRole]);
        callbacks
    }

    async fn launch(
        &self,
        intent: PublicOperationIntent,
    ) -> (Arc<PartitionOperation>, u64, AccessRootGuard) {
        launch_bound_attempt(
            &self.registry,
            self.store.clone(),
            intent,
            self.callbacks(),
            |operation| self.runtime.public_access_completion_guard(operation),
        )
        .await
        .unwrap()
    }

    async fn succeed(
        &self,
        operation: &Arc<PartitionOperation>,
        attempt: u64,
        mut guard: AccessRootGuard,
    ) -> PublicOperationRecord {
        let result =
            tokio::time::timeout(Duration::from_secs(5), operation.wait_for_attempt(attempt))
                .await
                .unwrap()
                .unwrap();
        assert_eq!(result.stage, PublicOperationStage::Completed);
        assert_eq!(
            result.disposition,
            Some(PublicOperationDisposition::Succeeded)
        );
        self.runtime.complete_public_access(operation);
        guard.succeeded();
        result
    }

    async fn status(&self) -> (AccessStatus, AccessStatus) {
        let view = HostAccessView {
            host: Arc::downgrade(&self.runtime.host),
            partition_information: self
                .runtime
                .host
                .state
                .read()
                .await
                .partition_information
                .clone(),
        };
        (
            view.read_status().await.unwrap(),
            view.write_status().await.unwrap(),
        )
    }

    async fn seed_historical_catchup(&self) {
        let (operation, attempt, guard) = self.launch(self.promotion(Epoch::new(0, 1), 1)).await;
        self.succeed(&operation, attempt, guard).await;
        assert_eq!(
            self.status().await,
            (AccessStatus::Granted, AccessStatus::Granted)
        );
        assert_eq!(self.probe.catchup_calls.load(Ordering::Acquire), 1);
    }
}

#[tokio::test]
async fn promotion_retry_restores_exact_prefix_but_requires_fresh_hosted_catchup() {
    let fixture = Fixture::new().await;
    fixture.seed_historical_catchup().await;
    fixture.probe.events.lock().unwrap().clear();
    fixture
        .probe
        .fail_application
        .store(true, Ordering::Release);
    let epoch = Epoch::new(0, 2);
    let intent = fixture.promotion(epoch, 2);
    let (operation, first_attempt, earlier_guard) = fixture.launch(intent.clone()).await;
    let failed = operation.wait_for_attempt(first_attempt).await.unwrap();
    assert_eq!(failed.stage, PublicOperationStage::RetryPending);
    assert_eq!(
        failed.lifecycle.outcomes,
        [
            PublicInstructionOutcome::Done,
            PublicInstructionOutcome::Done
        ]
    );
    assert_eq!(
        failed.retry_failure.as_ref().unwrap().instruction,
        PublicInstruction::ApplicationPrimary
    );
    assert_eq!(
        fixture.status().await,
        (AccessStatus::NotPrimary, AccessStatus::NotPrimary)
    );
    let earlier_generation = earlier_guard
        .owner
        .as_ref()
        .expect("the failed waiter must retain an actual access owner")
        .generation;
    let earlier_waiter = operation.wait_for_attempt(first_attempt);

    fixture
        .probe
        .fail_application
        .store(false, Ordering::Release);
    fixture.probe.block_catchup.store(true, Ordering::Release);
    let (retry, attempt, guard) = fixture.launch(intent.clone()).await;
    assert!(Arc::ptr_eq(&operation, &retry));
    assert_eq!(attempt, first_attempt + 1);
    assert!(
        guard
            .owner
            .as_ref()
            .expect("the retried waiter must retain an actual access owner")
            .generation
            > earlier_generation
    );
    tokio::time::timeout(
        Duration::from_secs(5),
        fixture.probe.catchup_entered.notified(),
    )
    .await
    .unwrap();
    {
        let access = fixture.runtime.host.selected_access.lock().unwrap();
        assert!(access.generation > earlier_generation);
        assert!(access.valid && access.active && access.promotion);
        assert_eq!(access.replicator_role, Some(ReplicaRole::Primary));
        assert_eq!(access.application_role, Some(ReplicaRole::Primary));
        assert_eq!(access.epoch, Some(epoch));
        assert_eq!(
            access.configuration,
            Some(intent.lifecycle.as_ref().unwrap().configuration())
        );
        assert!(
            !access.caught_up,
            "the earlier promotion's catch-up cannot qualify this attempt"
        );
    }
    assert_eq!(
        fixture.status().await,
        (
            AccessStatus::ReconfigurationPending,
            AccessStatus::ReconfigurationPending
        )
    );
    assert_eq!(fixture.probe.catchup_calls.load(Ordering::Acquire), 2);
    assert_eq!(
        *fixture.probe.events.lock().unwrap(),
        [
            "replicator.Primary",
            "epoch",
            "application.Primary",
            "application.Primary",
            "configuration",
            "catchup",
        ]
    );
    fixture.probe.catchup_release.notify_one();
    let succeeded = fixture.succeed(&retry, attempt, guard).await;
    assert_eq!(succeeded.intent, intent);
    assert_eq!(
        succeeded.lifecycle.outcomes[..failed.lifecycle.outcomes.len()],
        failed.lifecycle.outcomes
    );
    assert_eq!(
        succeeded.attempt_results[&first_attempt],
        failed.attempt_results[&first_attempt]
    );
    assert_eq!(
        *fixture.probe.events.lock().unwrap(),
        [
            "replicator.Primary",
            "epoch",
            "application.Primary",
            "application.Primary",
            "configuration",
            "catchup",
            "application.Primary",
        ]
    );
    assert_eq!(
        fixture.status().await,
        (AccessStatus::Granted, AccessStatus::Granted)
    );
    let generation = fixture
        .runtime
        .host
        .selected_access
        .lock()
        .unwrap()
        .generation;
    assert_eq!(earlier_waiter.await.unwrap(), failed);
    drop(earlier_guard);
    assert_eq!(
        fixture
            .runtime
            .host
            .selected_access
            .lock()
            .unwrap()
            .generation,
        generation
    );
    assert_eq!(
        fixture.status().await,
        (AccessStatus::Granted, AccessStatus::Granted)
    );
    fixture.registry.shutdown().await.unwrap();
}

#[tokio::test]
async fn split_primary_retry_restores_role_without_borrowing_historical_catchup() {
    let fixture = Fixture::new().await;
    fixture.seed_historical_catchup().await;
    let epoch = Epoch::new(0, 2);
    let (secondary, attempt, guard) = fixture
        .launch(fixture.intent(
            PublicOperationProgram::Role {
                epoch,
                role: ReplicaRole::ActiveSecondary,
            },
            2,
        ))
        .await;
    fixture.succeed(&secondary, attempt, guard).await;
    fixture.probe.events.lock().unwrap().clear();
    fixture
        .probe
        .fail_application
        .store(true, Ordering::Release);
    let epoch = Epoch::new(0, 3);
    let intent = fixture.intent(
        PublicOperationProgram::Role {
            epoch,
            role: ReplicaRole::Primary,
        },
        3,
    );
    let (operation, first_attempt, earlier_guard) = fixture.launch(intent.clone()).await;
    let failed = operation.wait_for_attempt(first_attempt).await.unwrap();
    assert_eq!(failed.stage, PublicOperationStage::RetryPending);
    assert_eq!(failed.lifecycle.outcomes, [PublicInstructionOutcome::Done]);
    let earlier_waiter = operation.wait_for_attempt(first_attempt);
    fixture
        .probe
        .fail_application
        .store(false, Ordering::Release);
    let (retry, attempt, guard) = fixture.launch(intent).await;
    fixture.succeed(&retry, attempt, guard).await;
    {
        let access = fixture.runtime.host.selected_access.lock().unwrap();
        assert!(access.promotion);
        assert_eq!(access.replicator_role, Some(ReplicaRole::Primary));
        assert_eq!(access.application_role, Some(ReplicaRole::Primary));
        assert_eq!(access.epoch, Some(epoch));
        assert!(access.configuration.is_none());
        assert!(!access.caught_up);
    }
    assert_ne!(fixture.status().await.1, AccessStatus::Granted);
    assert_eq!(fixture.probe.catchup_calls.load(Ordering::Acquire), 1);
    assert_eq!(
        *fixture.probe.events.lock().unwrap(),
        [
            "replicator.Primary",
            "application.Primary",
            "application.Primary"
        ]
    );

    fixture.probe.block_catchup.store(true, Ordering::Release);
    let (catchup, attempt, guard) = fixture
        .launch(fixture.intent(
            PublicOperationProgram::CatchUp {
                configuration: fixture.configuration(epoch),
                mode: PublicCatchUpMode::WriteQuorum,
            },
            4,
        ))
        .await;
    tokio::time::timeout(
        Duration::from_secs(5),
        fixture.probe.catchup_entered.notified(),
    )
    .await
    .unwrap();
    assert_ne!(fixture.status().await.1, AccessStatus::Granted);
    fixture.probe.catchup_release.notify_one();
    fixture.succeed(&catchup, attempt, guard).await;
    assert_eq!(fixture.probe.catchup_calls.load(Ordering::Acquire), 2);
    assert_eq!(
        fixture.status().await,
        (AccessStatus::Granted, AccessStatus::Granted)
    );
    assert_eq!(earlier_waiter.await.unwrap(), failed);
    drop(earlier_guard);
    assert_eq!(
        fixture.status().await,
        (AccessStatus::Granted, AccessStatus::Granted)
    );
    fixture.registry.shutdown().await.unwrap();
}
