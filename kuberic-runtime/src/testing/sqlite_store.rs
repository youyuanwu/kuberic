use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::authority::{BuildAuthorityStore, BuildProgressStore, LocalWriteJournal};
use crate::host::store::AgentStore;
use crate::protocol::command::EnsureReplicaBuild;
use crate::protocol::public_operations::{
    DataLossAuthorization, PreviewLifecycleBinding, PublicDataLossRecoveryStatus,
    PublicOperationPreviewIdentity, StatePersistence,
};
use crate::protocol::types::{BuildAuthority, OperationId};
use crate::protocol::types::{
    HistoryContext, HistoryInitializationId, ReplicaStorageIncarnationContext,
};

use super::authority::{DurableBuildProgress, DurableLocalWrite};
use super::effects::{RuntimeEffect, RuntimeEffectResult};
use super::state::{AgentState, StorageIdentity};

pub struct SqliteStore {
    pub(super) inner: Arc<crate::host::sqlite_store::SqliteStore>,
}

pub const PUBLIC_V1_PREVIEW_SCHEMA_VERSION: u32 =
    crate::host::state::PUBLIC_V1_PREVIEW_SCHEMA_VERSION;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicV1HistorySnapshot {
    pub initialization_id: HistoryInitializationId,
    pub history: HistoryContext,
    pub storage: ReplicaStorageIncarnationContext,
    pub authorization: Option<DataLossAuthorization>,
    pub recovery: Option<PublicDataLossRecoveryStatus>,
}

impl SqliteStore {
    pub fn metadata_database_path(root: &Path) -> PathBuf {
        crate::host::sqlite_store::SqliteStore::metadata_database_path(root)
    }

    pub fn create_authorized(
        path: impl AsRef<Path>,
        state: AgentState,
    ) -> crate::host::Result<Self> {
        Ok(Self {
            inner: Arc::new(crate::host::sqlite_store::SqliteStore::create_authorized(
                path,
                super::convert(state),
            )?),
        })
    }

    pub fn create_public_v1(
        path: impl AsRef<Path>,
        preview: PublicOperationPreviewIdentity,
        identity: StorageIdentity,
    ) -> crate::host::Result<Self> {
        if !preview.is_public_v1() {
            return Err(crate::host::HostError::IdentityMismatch(
                "public-v1 store requires protocol 11".into(),
            ));
        }
        let resource_uid = identity.resource_uid.clone();
        Ok(Self {
            inner: Arc::new(
                crate::host::sqlite_store::SqliteStore::create_preview_bound_authorized(
                    path,
                    crate::host::state::AgentState::new(super::convert(identity)),
                    PreviewLifecycleBinding {
                        preview,
                        resource_uid,
                        spec_generation: 1,
                        state_persistence: StatePersistence::Persisted,
                    },
                )?,
            ),
        })
    }

    pub fn open_public_v1(
        path: impl AsRef<Path>,
        preview: &PublicOperationPreviewIdentity,
    ) -> crate::host::Result<Self> {
        Ok(Self {
            inner: Arc::new(
                crate::host::sqlite_store::SqliteStore::open_preview_existing(path, None, preview)?,
            ),
        })
    }

    pub fn open_existing(
        path: impl AsRef<Path>,
        identity: Option<&StorageIdentity>,
    ) -> crate::host::Result<Self> {
        let identity = identity.map(super::convert);
        Ok(Self {
            inner: Arc::new(crate::host::sqlite_store::SqliteStore::open_existing(
                path,
                identity.as_ref(),
            )?),
        })
    }

    pub fn path(&self) -> &Path {
        self.inner.path()
    }

    pub async fn load_state(&self) -> crate::host::Result<AgentState> {
        self.inner.load_state().await.map(super::convert)
    }

    pub async fn initialize_public_v1_history(
        &self,
        initialization_id: &HistoryInitializationId,
        history: &HistoryContext,
        storage: &ReplicaStorageIncarnationContext,
    ) -> crate::host::Result<()> {
        self.inner
            .initialize_public_v1_history(initialization_id, history, storage)
            .await
    }

    pub async fn public_v1_history_snapshot(&self) -> crate::host::Result<PublicV1HistorySnapshot> {
        let state = self.inner.load_state().await?;
        let preview = state.public_operation_preview.ok_or_else(|| {
            crate::host::HostError::CommandRejected("public-v1 preview state required".into())
        })?;
        Ok(PublicV1HistorySnapshot {
            initialization_id: preview.history_initialization_id.ok_or_else(|| {
                crate::host::HostError::CommandRejected(
                    "public-v1 history initialization is absent".into(),
                )
            })?,
            history: preview.history.ok_or_else(|| {
                crate::host::HostError::CommandRejected("public-v1 history is absent".into())
            })?,
            storage: preview.storage_incarnation.ok_or_else(|| {
                crate::host::HostError::CommandRejected(
                    "public-v1 storage incarnation is absent".into(),
                )
            })?,
            authorization: preview.data_loss_authorization,
            recovery: preview.data_loss_recovery,
        })
    }

    pub async fn journal_build(
        &self,
        command: &EnsureReplicaBuild,
    ) -> crate::host::Result<EnsureReplicaBuild> {
        self.inner.journal_build(command).await
    }

    pub async fn load_local_writes(&self) -> crate::host::Result<Vec<DurableLocalWrite>> {
        Ok(super::convert(self.inner.load_local_writes().await?))
    }

    pub async fn load_local_write(
        &self,
        id: &OperationId,
    ) -> crate::host::Result<Option<DurableLocalWrite>> {
        Ok(super::convert(self.inner.load_local_write(id).await?))
    }

    pub async fn load_build(
        &self,
        id: &OperationId,
    ) -> crate::host::Result<Option<BuildAuthority>> {
        Ok(self.inner.load_build(id).await?)
    }

    pub async fn load_build_progress(
        &self,
        id: &OperationId,
    ) -> crate::host::Result<Option<DurableBuildProgress>> {
        Ok(super::convert(self.inner.load_build_progress(id).await?))
    }

    pub async fn begin_effect(&self, effect: &RuntimeEffect) -> crate::host::Result<()> {
        self.inner.begin_effect(&super::convert(effect)).await?;
        Ok(())
    }

    pub async fn mark_effect_applied(
        &self,
        effect: &RuntimeEffect,
        result: &super::effects::RuntimeEffectResult,
    ) -> crate::host::Result<()> {
        self.inner
            .mark_effect_applied(&super::convert(effect), &super::convert(result))
            .await
    }

    pub async fn complete_effect(&self, result: &RuntimeEffectResult) -> crate::host::Result<()> {
        self.inner.complete_effect(&super::convert(result)).await
    }
}
