use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::authority::{BuildAuthorityStore, BuildProgressStore, LocalWriteJournal};
use crate::host::store::AgentStore;
use crate::protocol::command::{
    EnsureReplicaBuild, PublicV1HistoryCommand, PublicV1SessionRenewalCommand,
};
use crate::protocol::public_operations::{
    DataLossAuthorization, PreviewLifecycleBinding, PublicDataLossRecoveryStatus,
    PublicOperationPreviewIdentity, PublicProgress, PublicV1AgentReport,
    PublicV1SessionRenewalStatus, StatePersistence,
};
use crate::protocol::types::{
    AccessStatus, BuildAuthority, OperationId, ProcessSessionId, ReplicaRole,
};
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
    pub session_renewal: Option<PublicV1SessionRenewalStatus>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicV1ReportProjection {
    pub process_session_id: ProcessSessionId,
    pub report_sequence: u64,
    pub role: ReplicaRole,
    pub read_status: AccessStatus,
    pub write_status: AccessStatus,
    pub progress: PublicProgress,
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
            session_renewal: preview.session_renewal,
        })
    }

    pub async fn apply_public_v1_history_command(
        &self,
        command: &PublicV1HistoryCommand,
    ) -> crate::host::Result<()> {
        command
            .validate()
            .map_err(|message| crate::host::HostError::CommandRejected(message.into()))?;
        match command {
            PublicV1HistoryCommand::Initialize {
                accepted, storage, ..
            } => {
                self.inner
                    .initialize_public_v1_history(
                        &accepted.history_initialization_id,
                        &accepted.history,
                        storage,
                    )
                    .await
            }
            PublicV1HistoryCommand::Recover { intent, .. } => {
                let renewal = self
                    .inner
                    .load_state()
                    .await?
                    .public_operation_preview
                    .and_then(|preview| preview.data_loss_recovery)
                    .is_some_and(|recovery| {
                        intent
                            .lifecycle
                            .as_ref()
                            .and_then(|input| input.data_loss_recovery.as_ref())
                            .is_some_and(|candidate| candidate.attempt != recovery.attempt)
                    });
                if renewal {
                    self.inner.renew_public_v1_data_loss_attempt(intent).await?;
                } else {
                    self.inner.begin_public_operation(intent, &[], &[]).await?;
                }
                Ok(())
            }
            PublicV1HistoryCommand::SessionRenewal(command) => self
                .apply_public_v1_session_renewal_command(command)
                .await
                .map(|_| ()),
        }
    }

    pub async fn persist_public_v1_session_renewal(
        &self,
        expected: Option<&PublicV1SessionRenewalStatus>,
        next: &PublicV1SessionRenewalStatus,
    ) -> crate::host::Result<()> {
        self.inner
            .persist_public_v1_session_renewal(expected, next)
            .await
    }

    pub async fn begin_public_v1_operation(
        &self,
        intent: &crate::protocol::public_operations::PublicOperationIntent,
    ) -> crate::host::Result<()> {
        self.inner.begin_public_operation(intent, &[], &[]).await?;
        Ok(())
    }

    pub async fn public_v1_operation_intents(
        &self,
    ) -> crate::host::Result<Vec<crate::protocol::public_operations::PublicOperationIntent>> {
        Ok(self
            .inner
            .public_operation_records()
            .await?
            .into_iter()
            .map(|record| record.intent)
            .collect())
    }

    pub async fn apply_public_v1_session_renewal_command(
        &self,
        command: &PublicV1SessionRenewalCommand,
    ) -> crate::host::Result<PublicV1SessionRenewalStatus> {
        command
            .validate()
            .map_err(|message| crate::host::HostError::CommandRejected(message.into()))?;
        let accepted = command.accepted();
        let current = self
            .inner
            .load_state()
            .await?
            .public_operation_preview
            .and_then(|preview| preview.session_renewal)
            .ok_or_else(|| {
                crate::host::HostError::CommandRejected(
                    "public-v1 session renewal is not durable".into(),
                )
            })?;
        if &current != accepted {
            return Err(crate::host::HostError::StaleEffectCompletion(
                "public-v1 session renewal command changed accepted state".into(),
            ));
        }
        let next = match command {
            PublicV1SessionRenewalCommand::CloseAccessAndIngress { .. } => accepted.close_access(),
            PublicV1SessionRenewalCommand::UpdateEpochBarrier {
                survivor, epoch, ..
            } => accepted.record_epoch_updated(survivor, *epoch),
            PublicV1SessionRenewalCommand::ReplayReplacementRole {
                process_session_id,
                epoch,
                role,
                ..
            } => accepted.replay_replacement_role(process_session_id, *epoch, *role),
            PublicV1SessionRenewalCommand::InstallFreshConfiguration { configuration, .. } => {
                accepted.install_fresh_configuration(configuration)
            }
        }
        .map_err(|message| crate::host::HostError::CommandRejected(message.into()))?;
        self.inner
            .persist_public_v1_session_renewal(Some(accepted), &next)
            .await?;
        Ok(next)
    }

    pub async fn contain_public_v1_data_loss_attempt(
        &self,
        intent: &crate::protocol::public_operations::PublicOperationIntent,
    ) -> crate::host::Result<()> {
        let record = self
            .inner
            .public_operation_records()
            .await?
            .into_iter()
            .find(|record| record.intent == *intent)
            .ok_or_else(|| {
                crate::host::HostError::CommandRejected(
                    "public-v1 data-loss attempt is not journaled".into(),
                )
            })?;
        self.inner
            .advance_public_operation(
                &intent.operation_id,
                intent.revision,
                &intent.process_session_id,
                record.stage,
                crate::host::state::PublicOperationStage::Running,
                None,
            )
            .await?;
        for (index, instruction) in crate::host::public_lifecycle::operation_instructions(intent)
            .into_iter()
            .enumerate()
        {
            self.inner
                .public_instruction(intent, index, instruction, None)
                .await?;
            if instruction == crate::host::state::PublicInstruction::DataLoss {
                self.inner
                    .advance_public_operation(
                        &intent.operation_id,
                        intent.revision,
                        &intent.process_session_id,
                        crate::host::state::PublicOperationStage::Running,
                        crate::host::state::PublicOperationStage::ContainmentPending,
                        Some(crate::host::state::PublicOperationDisposition::Ambiguous(
                            "predecessor process exited".into(),
                        )),
                    )
                    .await?;
                return Ok(());
            }
            let outcome = match instruction {
                crate::host::state::PublicInstruction::ApplicationPrimary => {
                    crate::host::state::PublicInstructionOutcome::ApplicationRole(None)
                }
                _ => crate::host::state::PublicInstructionOutcome::Done,
            };
            self.inner
                .public_instruction(intent, index, instruction, Some(outcome))
                .await?;
        }
        Err(crate::host::HostError::CommandRejected(
            "public-v1 recovery has no data-loss instruction".into(),
        ))
    }

    pub async fn public_v1_agent_report(
        &self,
        identity: &PublicOperationPreviewIdentity,
        projection: PublicV1ReportProjection,
    ) -> crate::host::Result<PublicV1AgentReport> {
        crate::host::public_lifecycle::public_v1_report(
            self.inner.as_ref(),
            identity,
            crate::host::public_lifecycle::PublicV1ReportProjection {
                process_session_id: projection.process_session_id,
                report_sequence: projection.report_sequence,
                role: projection.role,
                read_status: projection.read_status,
                write_status: projection.write_status,
                progress: projection.progress,
            },
        )
        .await
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
