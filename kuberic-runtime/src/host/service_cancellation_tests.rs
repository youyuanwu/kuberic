use super::*;
use crate::application::{
    CopyChunk, DurableApplicationProgress, OpenContext, Operation, OperationDataStream, RoleChange,
};
use crate::engine::{DurableState, RetainedOperationStream};
use crate::host::sqlite_store::SqliteStore;
use crate::host::state::{AgentState, StorageIdentity};
use crate::protocol::public_operations::*;
use crate::protocol::types::*;
use crate::replicator::{DefaultReplicatorFactory, Replicator, ReplicatorSettings};
use crate::{StateProvider, StatefulServiceReplica};

struct EmptyProvider;
struct Application;

#[async_trait::async_trait]
impl StateProvider for EmptyProvider {
    async fn update_epoch(&self, _: Epoch, _: i64) -> crate::Result<()> {
        Ok(())
    }
    async fn last_committed_lsn(&self) -> crate::Result<i64> {
        Ok(0)
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
        Ok(false)
    }
}
#[async_trait::async_trait]
impl DurableState for EmptyProvider {
    async fn get_replication_operations(
        &self,
        _: i64,
        _: i64,
    ) -> crate::Result<RetainedOperationStream> {
        Ok(Box::pin(futures::stream::empty()))
    }
    async fn apply_copy_chunk(&self, _: &OperationId, _: u64, _: CopyChunk) -> crate::Result<()> {
        Ok(())
    }
    async fn verify_copy_chunk(
        &self,
        _: &OperationId,
        _: u64,
        _: &CopyChunk,
    ) -> crate::Result<bool> {
        Ok(true)
    }
    async fn finish_copy(
        &self,
        _: &OperationId,
        _: i64,
        _: i64,
    ) -> crate::Result<DurableApplicationProgress> {
        Ok(Default::default())
    }
    async fn apply(&self, _: Operation) -> crate::Result<DurableApplicationProgress> {
        Err(RuntimeError::Application("read-only fixture".into()))
    }
    async fn durable_progress(&self) -> crate::Result<DurableApplicationProgress> {
        Ok(Default::default())
    }
    async fn verify_applied(&self, _: &Operation) -> crate::Result<bool> {
        Ok(false)
    }
    async fn commit(&self, _: i64) -> crate::Result<DurableApplicationProgress> {
        Ok(Default::default())
    }
}
#[async_trait::async_trait]
impl StatefulServiceReplica for Application {
    async fn open(self: Arc<Self>, context: OpenContext) -> crate::Result<Arc<dyn Replicator>> {
        Ok(context
            .partition
            .with_factory(Arc::new(DefaultReplicatorFactory::new(Arc::new(
                EmptyProvider,
            ))))
            .create_replicator(
                Some(Arc::new(EmptyProvider)),
                Some(ReplicatorSettings {
                    replication_address: "http://127.0.0.1:0".into(),
                }),
            )
            .await?
            .replicator())
    }
    async fn change_role(&self, _: ReplicaRole) -> crate::Result<RoleChange> {
        Ok(RoleChange {
            service_address: None,
        })
    }
    async fn close(&self) -> crate::Result<()> {
        Ok(())
    }
    fn abort(&self) {}
}

#[tokio::test]
async fn cancelled_configuration_request_does_not_strand_durable_completion_behind_access_fence() {
    let root = tempfile::Builder::new()
        .prefix(".kuberic-service-cancellation-")
        .tempdir_in(std::env::current_dir().unwrap())
        .unwrap();
    let uid = ResourceUid::new("cancelled-reply-resource");
    let local = ReplicaIdentity {
        replica_id: ReplicaId::new(1),
        instance_id: ReplicaInstanceId::new("pod"),
        agent_generation: AgentGeneration::new("generation"),
    };
    let storage = ReplicaStorageIncarnationContext {
        replica: local.clone(),
        storage: ReplicaStorageIdentity::Persistent {
            pod_uid: PodUid::new("pod"),
            pvc_uid: PvcUid::new("pvc"),
        },
        storage_generation: StorageGeneration::new("storage"),
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
            AgentState::new(StorageIdentity {
                schema_version: crate::host::state::SCHEMA_VERSION,
                resource_uid: uid.clone(),
                pod_uid: PodUid::new("pod"),
                pvc_uid: PvcUid::new("pvc"),
                initialization_id: InitializationId::new("initialization"),
                local_identity: local.clone(),
                effective_policy: EffectivePolicy::fixed(1, 1).unwrap(),
            }),
            binding.clone(),
        )
        .unwrap(),
    );
    let initialization = derive_history_initialization_id(&uid, PublicProtocolGeneration::new(1));
    let history = derive_bootstrap_history(&uid, PublicProtocolGeneration::new(1), &initialization);
    store
        .initialize_public_v1_history(&initialization, &history, &storage)
        .await
        .unwrap();
    let runtime = Arc::new(PodRuntime::new_replacement(
        local.clone(),
        Arc::new(Application),
        store.clone(),
        binding,
        "127.0.0.1:0".parse().unwrap(),
    ));
    let service = AgentService::new(store.clone(), runtime.clone(), "test-token").unwrap();
    runtime
        .bind_replica_session(uid, service.local_session().clone())
        .unwrap();
    service
        .public_operation_runtime(identity.clone())
        .await
        .unwrap();
    let intent = |revision, program: PublicOperationProgram| PublicOperationIntent {
        identity: identity.clone(),
        operation_id: OperationId::new(format!("operation-{revision}")),
        revision,
        process_session_id: service.local_session().clone(),
        class: PublicOperationClass::Authority,
        input_digest: program.digest(),
        replay_disposition: program.replay_disposition(),
        lifecycle: None,
        program: Some(program),
    };
    let epoch = Epoch::new(0, 1);
    for (revision, program) in [
        (
            1,
            PublicOperationProgram::Open {
                replica: local.clone(),
                existing: true,
            },
        ),
        (
            2,
            PublicOperationProgram::Role {
                epoch,
                role: ReplicaRole::Primary,
            },
        ),
        (3, PublicOperationProgram::Epoch { epoch }),
    ] {
        service
            .execute_public_intent(intent(revision, program))
            .await
            .unwrap();
    }
    let configuration = ConfigurationDescriptor::new(
        epoch,
        local.replica_id,
        vec![ConfigurationMember {
            identity: local,
            role: ReplicaRole::Primary,
        }],
        1,
    );
    let command = intent(
        4,
        PublicOperationProgram::Configuration(PublicConfiguration {
            current: configuration,
            previous: None,
            replicas: Vec::new(),
        }),
    );
    let operation_id = command.operation_id.clone();
    let mut requester = Box::pin(service.execute_public_intent(command));
    assert!(futures::poll!(requester.as_mut()).is_pending());
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let state = store.load_state().await.unwrap();
            if state
                .public_v1_operation_state
                .unwrap()
                .operations
                .get(&operation_id)
                .is_some_and(|operation| operation.stage == PublicOperationStage::Completed)
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    drop(requester);
    let admitted = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let report = crate::control::normalize_public_v1_agent_status_report(
                service.public_report().await.unwrap(),
            )
            .unwrap();
            if report.role == ReplicaRole::Primary
                && report.read_status == AccessStatus::Granted
                && report.write_status == AccessStatus::Granted
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await;
    service.registry.get().unwrap().shutdown().await.unwrap();
    runtime.shutdown_and_drain().await.unwrap();
    assert!(
        admitted.is_ok(),
        "completed configuration remained fenced after losing only its requester"
    );
}
