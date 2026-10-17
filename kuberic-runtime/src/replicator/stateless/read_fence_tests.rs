use super::*;
use crate::application::{DurableApplicationProgress, OperationDataStream};
use crate::capabilities::RuntimeHostToken;
use crate::engine::RetainedOperationStream;
use crate::host::store::PublicInboundBuildAdmission;
use crate::protocol::public_operations::{
    PublicV1Identity, PublicV1LifecycleBinding, StatePersistence,
};
use crate::protocol::types::{
    AccessStatus, AgentGeneration, ConfigurationId, ConfigurationMember, FaultType, LoadMetric,
    OperationId, PartitionId, PartitionInformation, ReplicaInstanceId, ResourceUid,
};
use crate::replicator::PartitionAccessView;

struct Publication {
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
    calls: AtomicU64,
    fail: bool,
}

#[async_trait]
impl PartitionAccessView for Publication {
    fn partition_information(&self) -> PartitionInformation {
        PartitionInformation {
            partition_id: PartitionId::new("read-fence"),
        }
    }
    async fn read_status(&self) -> Result<AccessStatus> {
        Ok(AccessStatus::NotPrimary)
    }
    async fn write_status(&self) -> Result<AccessStatus> {
        Ok(AccessStatus::NotPrimary)
    }
    async fn publish_replacement_progress(
        &self,
        applied: i64,
        committed: i64,
        ambiguous: bool,
    ) -> Result<()> {
        if self.calls.fetch_add(1, Ordering::AcqRel) == 0 {
            assert_eq!((applied, committed, ambiguous), (0, 0, true));
            self.entered.notify_one();
            self.release.notified().await;
        }
        if self.fail {
            Err(RuntimeError::Closed)
        } else {
            Ok(())
        }
    }
    async fn replacement_inbound_build_authority(
        &self,
        _: &OperationId,
    ) -> Result<Option<PublicInboundBuildAuthority>> {
        unreachable!()
    }
    async fn classify_replacement_inbound_build_authority(
        &self,
        _: &PublicInboundBuildAuthority,
    ) -> Result<PublicInboundBuildAdmission> {
        unreachable!()
    }
    async fn install_replacement_inbound_build_authority(
        &self,
        _: &PublicInboundBuildAuthority,
    ) -> Result<()> {
        unreachable!()
    }
    async fn replacement_terminal_inbound_build_receipt(
        &self,
        _: &OperationId,
    ) -> Result<Option<PublicInboundBuildReceipt>> {
        unreachable!()
    }
    async fn revoke_replacement_inbound_build_authority(&self) -> Result<()> {
        unreachable!()
    }
    async fn replacement_outbound_build_authority(
        &self,
        _: &OperationId,
        _: &ConfigurationId,
    ) -> Result<Option<PublicInboundBuildAuthority>> {
        unreachable!()
    }
    async fn complete_replacement_inbound_build(
        &self,
        _: &PublicInboundBuildReceipt,
    ) -> Result<()> {
        unreachable!()
    }
    async fn replacement_storage_is_retired(
        &self,
        _: &crate::protocol::types::ReplicaStorageIncarnationContext,
    ) -> Result<bool> {
        unreachable!()
    }
    fn release_provider_writer(&self) {}
    async fn report_load(&self, _: Vec<LoadMetric>) -> Result<()> {
        unreachable!()
    }
    async fn report_fault(&self, _: FaultType) -> Result<()> {
        unreachable!()
    }
}

#[derive(Default)]
struct Provider {
    apply_calls: AtomicU64,
}

#[async_trait]
impl StateProvider for Provider {
    async fn update_epoch(&self, _: Epoch, _: i64) -> Result<()> {
        unreachable!()
    }
    async fn last_committed_lsn(&self) -> Result<i64> {
        Ok(0)
    }
    async fn get_copy_context(&self) -> Result<OperationDataStream> {
        unreachable!()
    }
    async fn get_copy_state(&self, _: i64, _: OperationDataStream) -> Result<OperationDataStream> {
        unreachable!()
    }
    async fn on_data_loss(&self) -> Result<bool> {
        unreachable!()
    }
}

#[async_trait]
impl DurableState for Provider {
    async fn get_replication_operations(&self, _: i64, _: i64) -> Result<RetainedOperationStream> {
        unreachable!()
    }
    async fn apply_copy_chunk(&self, _: &OperationId, _: u64, _: CopyChunk) -> Result<()> {
        unreachable!()
    }
    async fn verify_copy_chunk(&self, _: &OperationId, _: u64, _: &CopyChunk) -> Result<bool> {
        unreachable!()
    }
    async fn finish_copy(
        &self,
        _: &OperationId,
        _: i64,
        _: i64,
    ) -> Result<DurableApplicationProgress> {
        unreachable!()
    }
    async fn apply(&self, _: Operation) -> Result<DurableApplicationProgress> {
        self.apply_calls.fetch_add(1, Ordering::AcqRel);
        Err(RuntimeError::Application(
            "provider should not be entered".into(),
        ))
    }
    async fn durable_progress(&self) -> Result<DurableApplicationProgress> {
        Ok(DurableApplicationProgress::default())
    }
    async fn verify_applied(&self, _: &Operation) -> Result<bool> {
        unreachable!()
    }
    async fn commit(&self, _: i64) -> Result<DurableApplicationProgress> {
        unreachable!()
    }
}

async fn publication_boundary(fail: bool, revoke: bool, cancel: bool) {
    let access = Arc::new(Publication {
        entered: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
        calls: AtomicU64::new(0),
        fail,
    });
    let provider = Arc::new(Provider::default());
    let identity = ReplicaIdentity {
        replica_id: ReplicaId::new(1),
        instance_id: ReplicaInstanceId::new("read-fence"),
        agent_generation: AgentGeneration::new("read-fence"),
    };
    let session = ProcessSessionId::new("read-fence");
    let address = "127.0.0.1:0".parse().unwrap();
    let context = ReplicatorFactoryContext::new(
        RuntimeHostToken::new(),
        identity.clone(),
        session.clone(),
        access.clone(),
        PublicV1LifecycleBinding {
            identity: PublicV1Identity::new(51),
            resource_uid: ResourceUid::new("read-fence"),
            spec_generation: 1,
            state_persistence: StatePersistence::Persisted,
        },
        address,
    );
    let engine = Arc::new(StatelessReplicator::new(
        identity.clone(),
        session,
        context,
        provider.clone(),
        provider.clone(),
        ReplicatorSettings::default(),
        address,
    ));
    engine.self_ref.set(Arc::downgrade(&engine)).unwrap();
    let epoch = Epoch::new(0, 1);
    {
        let mut state = engine.state.lock().await;
        state.opened = true;
        state.role = ReplicaRole::Primary;
        state.role_epoch = Some(epoch);
        state.updated_epoch = Some(epoch);
        state.authority = Some(ConfigurationAuthority {
            epoch,
            current: ConfigurationDescriptor::new(
                epoch,
                identity.replica_id,
                vec![ConfigurationMember {
                    identity,
                    role: ReplicaRole::Primary,
                }],
                1,
            ),
            previous: None,
            peers: BTreeMap::new(),
        });
    }
    let mut writes = tokio::task::JoinSet::new();
    let writer = engine.clone();
    writes.spawn(async move { writer.replicate(bytes::Bytes::from_static(b"value")).await });
    tokio::time::timeout(Duration::from_secs(2), access.entered.notified())
        .await
        .unwrap();
    assert_eq!(provider.apply_calls.load(Ordering::Acquire), 0);
    assert!(engine.state.lock().await.ambiguous_suffix);
    if revoke {
        engine.revoke_generation();
    }
    if cancel {
        writes.abort_all();
        assert!(
            writes
                .join_next()
                .await
                .unwrap()
                .unwrap_err()
                .is_cancelled()
        );
    } else {
        access.release.notify_one();
        let result = tokio::time::timeout(Duration::from_secs(2), writes.join_next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        if fail {
            assert!(matches!(result, Err(RuntimeError::Closed)));
        } else {
            assert!(matches!(result, Err(RuntimeError::OperationCancelled)));
        }
    }
    engine.descendants.drain().await.unwrap();
    assert_eq!(provider.apply_calls.load(Ordering::Acquire), 0);
    let state = engine.state.lock().await;
    assert!(state.ambiguous_suffix);
    assert_eq!((state.applied_lsn, state.committed_lsn), (0, 0));
    assert_eq!(engine.abandoned_applied_floor.load(Ordering::Acquire), 0);
}

#[tokio::test]
async fn sf01_failed_preapply_publication_never_enters_provider() {
    publication_boundary(true, false, false).await;
}

#[tokio::test]
async fn sf01_revocation_during_preapply_publication_never_enters_provider() {
    publication_boundary(false, true, false).await;
}

#[tokio::test]
async fn sf01_drop_during_preapply_publication_never_enters_provider() {
    publication_boundary(false, false, true).await;
}
