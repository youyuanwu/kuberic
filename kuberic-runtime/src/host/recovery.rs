//! Caller-independent lifecycle recovery and restart-effect inspection.

use std::sync::Arc;

#[cfg(test)]
use crate::effects::{RuntimeEffect, RuntimeEffectResult};
#[cfg(test)]
use crate::protocol::types::AccessStatus;

use crate::RuntimeError;
use crate::host::Result;
use crate::host::hosting::RecoveryOwnerRuntime;
#[cfg(test)]
use crate::host::observation::RecoveryObservation;
#[cfg(test)]
use crate::host::runtime_adapter::{RuntimeAdapter, RuntimeEffectExecutor};
#[cfg(test)]
use crate::host::state::RetainedResult;
use crate::host::store::AgentStore;
use tokio::sync::watch;

pub(crate) struct RecoveryOwner<S> {
    runtime: RecoveryOwnerRuntime,
    store: Arc<S>,
    persisted_partition_revision: Option<u64>,
}

impl<S: AgentStore> RecoveryOwner<S> {
    pub(crate) fn new(runtime: RecoveryOwnerRuntime, store: Arc<S>) -> Self {
        Self {
            runtime,
            store,
            persisted_partition_revision: None,
        }
    }

    pub(crate) async fn advance(&mut self) -> Result<()> {
        let durable = self.store.load_state().await?;
        if durable.pending_effect.is_none() && durable.reconfiguration.is_none() {
            match self.runtime.observe_progress().await {
                Ok(())
                | Err(
                    RuntimeError::ReconfigurationPending
                    | RuntimeError::OperationCancelled
                    | RuntimeError::NotOpen
                    | RuntimeError::Closed,
                ) => {}
                Err(error) => return Err(error.into()),
            }
            match self.runtime.refresh_catch_up_capability().await {
                Ok(()) | Err(RuntimeError::NotOpen | RuntimeError::Closed) => {}
                Err(error) => return Err(error.into()),
            }

            let observation = self.runtime.observation().await;
            if observation.host.read_status != durable.read_status
                || observation.host.write_status != durable.write_status
            {
                match self
                    .runtime
                    .reconcile_durable_access(durable.read_status, durable.write_status)
                    .await
                {
                    Ok(())
                    | Err(
                        RuntimeError::ReconfigurationPending
                        | RuntimeError::OperationCancelled
                        | RuntimeError::NotOpen
                        | RuntimeError::Closed,
                    ) => {}
                    Err(error) => return Err(error.into()),
                }
            }
        }

        let partition = self.runtime.partition_report().await;
        if self.persisted_partition_revision != Some(partition.revision) {
            self.store
                .record_partition_reports(partition.load_metrics, partition.reported_fault)
                .await?;
            self.persisted_partition_revision = Some(partition.revision);
        }
        Ok(())
    }

    pub(crate) async fn run(mut self, mut shutdown: watch::Receiver<bool>) {
        loop {
            if *shutdown.borrow_and_update() {
                return;
            }
            if let Err(error) = self.advance().await {
                tracing::warn!(%error, "background lifecycle recovery retrying");
            }
            tokio::select! {
                _ = shutdown.changed() => {}
                _ = tokio::time::sleep(std::time::Duration::from_millis(100)) => {}
            }
        }
    }
}

#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RecoveryDecision {
    Idle,
    Reissue(Box<RuntimeEffect>),
    ReturnRetained(Box<RetainedResult>),
}

#[cfg(test)]
pub(crate) async fn inspect_recovery<S: AgentStore>(
    store: &S,
    runtime: &RecoveryObservation,
) -> Result<RecoveryDecision> {
    if runtime.write_status == AccessStatus::Granted {
        return Err(crate::host::HostError::DurableEffectConflict(
            "runtime must start write-closed before recovery".into(),
        ));
    }
    let state = store.load_state().await?;
    if let Some(pending) = state.pending_effect {
        return Ok(RecoveryDecision::Reissue(Box::new(pending.effect)));
    }
    if let Some(retained) = state.retained_result {
        return Ok(RecoveryDecision::ReturnRetained(Box::new(retained)));
    }
    Ok(RecoveryDecision::Idle)
}

#[cfg(test)]
pub(crate) async fn recover_pending<S, E>(
    adapter: &RuntimeAdapter<S, E>,
    runtime: &RecoveryObservation,
) -> Result<Option<RuntimeEffectResult>>
where
    S: AgentStore,
    E: RuntimeEffectExecutor,
{
    match inspect_recovery(adapter.store().as_ref(), runtime).await? {
        RecoveryDecision::Idle => Ok(None),
        RecoveryDecision::ReturnRetained(retained) => Ok(Some(retained.result)),
        RecoveryDecision::Reissue(effect) => adapter.execute(*effect).await.map(Some),
    }
}
