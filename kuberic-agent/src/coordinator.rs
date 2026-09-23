//! Durable replica-local reconfiguration coordinator.

use std::sync::Arc;
use tokio::sync::Mutex;

use kuberic_protocol::command::EnsureConfiguration;
use kuberic_protocol::types::{AccessStatus, OperationId, ProcessSessionId, ReplicaRole};
use kuberic_runtime::application::OpenMode;
use kuberic_runtime_internal::effects::{RuntimeEffect, RuntimeEffectAction, RuntimeEffectResult};

use crate::Result;
use crate::command::admit_configuration;
use crate::runtime_adapter::{RuntimeAdapter, RuntimeEffectExecutor};
use crate::state::{CoordinatorStage, ReconfigurationRecord, RetainedCommandResult};
use crate::store::{AgentStore, BeginConfiguration};

pub struct Coordinator<S, E> {
    store: Arc<S>,
    runtime: RuntimeAdapter<S, E>,
    command_lock: Mutex<()>,
}

impl<S, E> Coordinator<S, E>
where
    S: AgentStore,
    E: RuntimeEffectExecutor,
{
    pub fn new(store: Arc<S>, executor: Arc<E>) -> Self {
        Self {
            runtime: RuntimeAdapter::new(store.clone(), executor),
            store,
            command_lock: Mutex::new(()),
        }
    }

    pub async fn open_runtime(
        &self,
        mode: OpenMode,
        process_session: &ProcessSessionId,
    ) -> Result<RuntimeEffectResult> {
        let state = self.store.load_state().await?;
        self.runtime
            .execute(RuntimeEffect {
                operation_id: OperationId::new(format!(
                    "{}:open:{}",
                    state.identity.initialization_id.as_str(),
                    process_session.as_str()
                )),
                sequence: state.next_effect_sequence,
                action: RuntimeEffectAction::Open(mode),
            })
            .await
    }

    pub async fn resume_pending(&self) -> Result<Option<RuntimeEffectResult>> {
        self.runtime.resume_pending().await
    }

    pub async fn ensure_configuration(
        &self,
        command: EnsureConfiguration,
    ) -> Result<RetainedCommandResult> {
        let _command = self.command_lock.lock().await;
        let durable = self.store.load_state().await?;
        let authority = admit_configuration(&command, &durable)?;
        match self.store.begin_configuration(&command).await? {
            BeginConfiguration::Completed(result) => return Ok(result),
            BeginConfiguration::Execute(_) | BeginConfiguration::Pending(_) => {}
        }

        loop {
            let state = self.store.load_state().await?;
            let record = if let Some(record) = state.reconfiguration.clone() {
                record
            } else if let Some(retained) = state.retained_command
                && retained.command.operation_id == command.operation_id
            {
                return Ok(retained);
            } else {
                return Err(crate::AgentError::EffectConflict(
                    "configuration ownership changed while the command was executing".into(),
                ));
            };
            match record.stage {
                CoordinatorStage::AdmitAuthority => {
                    self.execute(
                        &record,
                        "admit-authority",
                        RuntimeEffectAction::AdmitAuthority(Box::new(authority.clone())),
                    )
                    .await?;
                    self.advance(&record, CoordinatorStage::Demote, None)
                        .await?;
                }
                CoordinatorStage::Demote => {
                    self.execute(
                        &record,
                        "demote",
                        RuntimeEffectAction::SetReadStatus(AccessStatus::ReconfigurationPending),
                    )
                    .await?;
                    self.advance(&record, CoordinatorStage::GetLsn, None)
                        .await?;
                }
                CoordinatorStage::GetLsn => {
                    let result = self
                        .execute(
                            &record,
                            "get-lsn",
                            RuntimeEffectAction::RefreshApplicationProgress,
                        )
                        .await?;
                    let next = if state.role == ReplicaRole::Primary
                        && authority.local_role() != ReplicaRole::Primary
                    {
                        CoordinatorStage::Catchup
                    } else {
                        CoordinatorStage::Deactivate
                    };
                    self.advance(&record, next, Some(result.postcondition.current_progress))
                        .await?;
                }
                CoordinatorStage::Catchup => {
                    self.execute(&record, "catchup", RuntimeEffectAction::WaitForCatchup)
                        .await?;
                    let next = if authority.local_role() == ReplicaRole::Primary {
                        CoordinatorStage::Activate
                    } else {
                        CoordinatorStage::Deactivate
                    };
                    self.advance(&record, next, None).await?;
                }
                CoordinatorStage::Deactivate => {
                    self.execute(
                        &record,
                        "deactivate",
                        RuntimeEffectAction::SetWriteStatus(AccessStatus::ReconfigurationPending),
                    )
                    .await?;
                    self.advance(&record, CoordinatorStage::ReplicatorRole, None)
                        .await?;
                }
                CoordinatorStage::ReplicatorRole => {
                    self.execute(
                        &record,
                        "replicator-role",
                        RuntimeEffectAction::ChangeReplicatorRole(authority.local_role()),
                    )
                    .await?;
                    let next = if authority.local_role() == ReplicaRole::Primary {
                        CoordinatorStage::Epoch
                    } else {
                        CoordinatorStage::ApplicationRole
                    };
                    self.advance(&record, next, None).await?;
                }
                CoordinatorStage::Epoch => {
                    self.execute(&record, "epoch", RuntimeEffectAction::UpdateEpoch)
                        .await?;
                    self.advance(&record, CoordinatorStage::ApplicationRole, None)
                        .await?;
                }
                CoordinatorStage::ApplicationRole => {
                    self.execute(
                        &record,
                        "application-role",
                        RuntimeEffectAction::ChangeApplicationRole(authority.local_role()),
                    )
                    .await?;
                    let next = if authority.local_role() == ReplicaRole::Primary {
                        CoordinatorStage::Catchup
                    } else {
                        CoordinatorStage::Activate
                    };
                    self.advance(&record, next, None).await?;
                }
                CoordinatorStage::Activate => {
                    let read_status = if matches!(
                        authority.local_role(),
                        ReplicaRole::Primary | ReplicaRole::ActiveSecondary
                    ) {
                        AccessStatus::Granted
                    } else {
                        AccessStatus::NotPrimary
                    };
                    let write_status = if authority.local_role() == ReplicaRole::Primary
                        && authority.previous_configuration.is_none()
                    {
                        AccessStatus::Granted
                    } else if authority.local_role() == ReplicaRole::Primary {
                        AccessStatus::ReconfigurationPending
                    } else {
                        AccessStatus::NotPrimary
                    };
                    self.execute(
                        &record,
                        "activate",
                        RuntimeEffectAction::SetAccessStatus {
                            read: read_status,
                            write: write_status,
                        },
                    )
                    .await?;
                    self.advance(&record, CoordinatorStage::Complete, None)
                        .await?;
                }
                CoordinatorStage::Complete => {
                    return self
                        .store
                        .complete_configuration(&record.command.operation_id)
                        .await;
                }
            }
        }
    }

    async fn execute(
        &self,
        record: &ReconfigurationRecord,
        stage: &str,
        action: RuntimeEffectAction,
    ) -> Result<RuntimeEffectResult> {
        let state = self.store.load_state().await?;
        let operation_id = stage_operation_id(&record.command.operation_id, stage);
        if let Some(retained) = state.retained_result.as_ref()
            && retained.operation_id == operation_id
        {
            if retained.effect.action != action {
                return Err(crate::AgentError::EffectConflict(
                    "retained coordinator effect has different stage authority".into(),
                ));
            }
            return self.runtime.execute(retained.effect.clone()).await;
        }
        self.runtime
            .execute(RuntimeEffect {
                operation_id,
                sequence: state.next_effect_sequence,
                action,
            })
            .await
    }

    async fn advance(
        &self,
        record: &ReconfigurationRecord,
        next: CoordinatorStage,
        observed_lsn: Option<i64>,
    ) -> Result<ReconfigurationRecord> {
        self.store
            .advance_configuration(
                &record.command.operation_id,
                record.stage,
                next,
                observed_lsn,
            )
            .await
    }
}

fn stage_operation_id(command: &OperationId, stage: &str) -> OperationId {
    OperationId::new(format!("{}:{stage}", command.as_str()))
}
