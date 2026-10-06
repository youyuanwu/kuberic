use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::oneshot;

use crate::authority::{AdmittedAuthority, BuildAuthority, RetiredAuthority};
use crate::effects::{RuntimeEffectAction, RuntimePostcondition, RuntimeSnapshot};
use crate::protocol::command::AcceptSecondaryRemovalCommit;
use crate::protocol::types::{
    AccessStatus, ConfigurationId, Epoch, OperationId, ProcessSessionId, ReplicaIdentity,
    SecondaryRemovalWitness, SecondaryScaleDownCleanup, SecondaryScaleDownIntent,
    SwitchoverRequestId,
};
use crate::receipts::{NativeProgressStatus, TopologyReceipt};
use crate::replicator::ReplicaInformation;
use crate::transport::{OutboundOperation, ReplicaEndpoint};
use crate::{Result, RuntimeError};

use super::custom::{
    AccessDecision, BuildAdmission, BuildCompletionConfirmation, ReadyAccessTransaction,
};

#[async_trait]
pub(super) trait ProcessLifecycle: Send + Sync {
    fn owns_stream_session(&self) -> bool;

    async fn complete_open(&self, address: String) -> Result<()>;
    async fn complete_close(&self) -> Result<()>;
    async fn complete_abort(&self);
    fn notify_abort(&self);
    async fn fence_writes(&self) -> Result<()>;
    #[cfg(any(all(test, kuberic_workspace_tests), feature = "testing"))]
    async fn invalidate_public_access(&self) -> Result<()>;
    async fn settle_primary_prefix(&self) -> Result<()>;
}

#[async_trait]
pub(super) trait AuthorityLifecycle: Send + Sync {
    async fn cancel_configuration_work(&self) -> Result<()>;
    async fn restore_authority(&self) -> Result<()>;
    async fn admit_authority(&self, authority: AdmittedAuthority) -> Result<()>;
    async fn register_peer_session(
        &self,
        identity: ReplicaIdentity,
        session: ProcessSessionId,
    ) -> Result<()>;
    async fn describe_peer(&self, replica: ReplicaInformation) -> Result<()>;
}

#[async_trait]
pub(super) trait AccessLifecycle: Send + Sync {
    async fn defer_restored_access(&self, read: AccessStatus, write: AccessStatus);
    async fn run_access_transaction(
        &self,
        read: AccessStatus,
        write: AccessStatus,
        ready: oneshot::Sender<()>,
        accept: oneshot::Receiver<()>,
        accepted: oneshot::Sender<Option<NativeProgressStatus>>,
        decision: oneshot::Receiver<AccessDecision>,
    ) -> Result<()>;
    async fn restored_access(&self) -> Option<(AccessStatus, AccessStatus)>;
}

#[async_trait]
pub(super) trait BuildLifecycle: Send + Sync {
    async fn admit_build_authority(&self, authority: BuildAuthority) -> Result<()>;
    async fn retire_build(&self, build_id: OperationId) -> Result<()>;
    async fn cancel_outbound_build(&self, id: &OperationId, generation: u64) -> Result<()>;
    async fn cancel_outbound_build_attempt(
        &self,
        id: &OperationId,
        generation: u64,
        public_cleanup: bool,
    ) -> Result<()>;
    async fn build_generation(&self, id: &OperationId) -> u64;
    async fn wait_for_build_completion(
        &self,
        build_id: &OperationId,
        target: &ReplicaIdentity,
    ) -> Result<()>;
    #[cfg(any(all(test, kuberic_workspace_tests), feature = "testing"))]
    async fn build_replica(&self, replica: ReplicaInformation) -> Result<()>;
    #[cfg(any(all(test, kuberic_workspace_tests), feature = "testing"))]
    async fn remove_replica(&self, replica_id: crate::protocol::types::ReplicaId) -> Result<()>;
    async fn select_build(&self, authority: &BuildAuthority) -> Result<()>;
    async fn execute_build(&self, replica: ReplicaInformation) -> Result<Option<BuildAdmission>>;
    async fn accept_build(&self, receipt: Option<BuildAdmission>) -> Result<()>;
    async fn enqueue_build(&self, endpoint: ReplicaEndpoint) -> Result<()>;
    async fn confirm_build_completion(
        &self,
        build_id: &OperationId,
        target: &ReplicaIdentity,
    ) -> Result<BuildCompletionConfirmation>;
}

#[async_trait]
pub(super) trait TopologyLifecycle: Send + Sync {
    async fn wait_for_catch_up(&self) -> Result<()>;
    async fn authorize_failover_prefix(&self, boundary: i64) -> Result<()>;
    async fn prepare_switchover(
        &self,
        preparation_generation: u64,
        request_id: SwitchoverRequestId,
        source: ReplicaIdentity,
        target: ReplicaIdentity,
        starting_configuration_id: ConfigurationId,
        starting_epoch: Epoch,
    ) -> Result<()>;
    async fn prepare_secondary_removal(
        &self,
        intent: SecondaryScaleDownIntent,
        process_session_id: ProcessSessionId,
        report_sequence: u64,
    ) -> Result<()>;
    async fn observe_secondary_removal(&self, witness: SecondaryRemovalWitness) -> Result<()>;
    async fn observe_secondary_removal_progress(
        &self,
        witness: SecondaryRemovalWitness,
        committed: SecondaryScaleDownCleanup,
    ) -> Result<()>;
    async fn accept_secondary_removal(&self, committed: SecondaryScaleDownCleanup) -> Result<()>;
    async fn accept_historical_secondary_removal(
        &self,
        command: AcceptSecondaryRemovalCommit,
    ) -> Result<()>;
    async fn fence_retirement(&self, retired: RetiredAuthority) -> Result<()>;
    async fn complete_retirement(&self, retired: RetiredAuthority) -> Result<()>;
    async fn topology_receipt(&self, action: &RuntimeEffectAction) -> Option<TopologyReceipt>;
}

#[async_trait]
pub(super) trait LifecycleObservation: Send + Sync {
    async fn refresh_progress(&self) -> Result<()>;
    async fn observe_progress(&self) -> Result<()>;
    async fn snapshot(&self) -> RuntimeSnapshot;
    async fn postcondition(&self, progress: Option<&NativeProgressStatus>) -> RuntimePostcondition;
}

#[async_trait]
pub(super) trait OutboundLifecycle: Send + Sync {
    async fn next_outbound(&self) -> Option<OutboundOperation>;
}

#[derive(Clone)]
pub(super) struct LifecycleWiring {
    pub(super) process: Arc<dyn ProcessLifecycle>,
    pub(super) authority: Arc<dyn AuthorityLifecycle>,
    pub(super) access: Arc<dyn AccessLifecycle>,
    pub(super) build: Arc<dyn BuildLifecycle>,
    pub(super) topology: Arc<dyn TopologyLifecycle>,
    pub(super) observation: Arc<dyn LifecycleObservation>,
    pub(super) outbound: Arc<dyn OutboundLifecycle>,
}

impl LifecycleWiring {
    pub(super) fn new<T>(backend: Arc<T>) -> Self
    where
        T: ProcessLifecycle
            + AuthorityLifecycle
            + AccessLifecycle
            + BuildLifecycle
            + TopologyLifecycle
            + LifecycleObservation
            + OutboundLifecycle
            + 'static,
    {
        Self {
            process: backend.clone(),
            authority: backend.clone(),
            access: backend.clone(),
            build: backend.clone(),
            topology: backend.clone(),
            observation: backend.clone(),
            outbound: backend,
        }
    }

    pub(super) fn process_runtime(&self) -> ProcessRuntime {
        ProcessRuntime {
            inner: self.process.clone(),
        }
    }

    pub(super) fn authority_runtime(&self) -> AuthorityRuntime {
        AuthorityRuntime {
            inner: self.authority.clone(),
        }
    }

    pub(super) fn peer_runtime(&self) -> PeerRuntime {
        PeerRuntime {
            inner: self.authority.clone(),
        }
    }

    pub(super) fn access_closure(&self) -> AccessClosure {
        AccessClosure {
            inner: self.access.clone(),
        }
    }

    pub(super) fn access_runtime(&self) -> AccessRuntime {
        AccessRuntime {
            inner: self.access.clone(),
        }
    }

    pub(super) fn report_lifecycle(&self) -> ReportLifecycle {
        ReportLifecycle {
            access: self.access.clone(),
            observation: self.observation.clone(),
        }
    }

    pub(super) fn evidence_runtime(&self) -> EvidenceRuntime {
        EvidenceRuntime {
            inner: self.observation.clone(),
        }
    }

    pub(super) fn effect_evidence_runtime(&self) -> EffectEvidenceRuntime {
        EffectEvidenceRuntime {
            inner: self.observation.clone(),
        }
    }
}

#[derive(Clone)]
pub(super) struct ProcessRuntime {
    inner: Arc<dyn ProcessLifecycle>,
}

impl ProcessRuntime {
    pub(super) fn owns_stream_session(&self) -> bool {
        self.inner.owns_stream_session()
    }

    pub(super) async fn complete_open(&self, address: String) -> Result<()> {
        self.inner.complete_open(address).await
    }

    pub(super) async fn complete_close(&self) -> Result<()> {
        self.inner.complete_close().await
    }

    pub(super) async fn complete_abort(&self) {
        self.inner.complete_abort().await;
    }

    pub(super) fn notify_abort(&self) {
        self.inner.notify_abort();
    }

    pub(super) async fn fence_writes(&self) -> Result<()> {
        self.inner.fence_writes().await
    }

    #[cfg(any(all(test, kuberic_workspace_tests), feature = "testing"))]
    pub(super) async fn invalidate_public_access(&self) -> Result<()> {
        self.inner.invalidate_public_access().await
    }

    pub(super) async fn settle_primary_prefix(&self) -> Result<()> {
        self.inner.settle_primary_prefix().await
    }
}

#[derive(Clone)]
pub(super) struct AuthorityRuntime {
    inner: Arc<dyn AuthorityLifecycle>,
}

impl AuthorityRuntime {
    pub(super) async fn cancel_configuration_work(&self) -> Result<()> {
        self.inner.cancel_configuration_work().await
    }

    pub(super) async fn restore_authority(&self) -> Result<()> {
        self.inner.restore_authority().await
    }

    pub(super) async fn admit_authority(&self, authority: AdmittedAuthority) -> Result<()> {
        self.inner.admit_authority(authority).await
    }
}

#[derive(Clone)]
pub(super) struct PeerRuntime {
    inner: Arc<dyn AuthorityLifecycle>,
}

impl PeerRuntime {
    pub(super) async fn register_peer_session(
        &self,
        identity: ReplicaIdentity,
        session: ProcessSessionId,
    ) -> Result<()> {
        self.inner.register_peer_session(identity, session).await
    }

    pub(super) async fn describe_peer(&self, replica: ReplicaInformation) -> Result<()> {
        self.inner.describe_peer(replica).await
    }
}

#[derive(Clone)]
pub(super) struct AccessClosure {
    inner: Arc<dyn AccessLifecycle>,
}

async fn begin_access_effect(
    access: Arc<dyn AccessLifecycle>,
    read: AccessStatus,
    write: AccessStatus,
) -> Result<ReadyAccessTransaction> {
    let (ready_tx, ready_rx) = oneshot::channel();
    let (accept_tx, accept_rx) = oneshot::channel();
    let (accepted_tx, accepted_rx) = oneshot::channel();
    let (decision_tx, decision_rx) = oneshot::channel();
    let completion = tokio::spawn(async move {
        access
            .run_access_transaction(read, write, ready_tx, accept_rx, accepted_tx, decision_rx)
            .await
    });
    match ready_rx.await {
        Ok(()) => Ok(ReadyAccessTransaction {
            accept: accept_tx,
            accepted: accepted_rx,
            decision: decision_tx,
            completion,
        }),
        Err(_) => completion
            .await
            .map_err(|error| RuntimeError::Application(error.to_string()))?
            .and(Err(RuntimeError::OperationCancelled)),
    }
}

async fn commit_access(
    access: Arc<dyn AccessLifecycle>,
    read: AccessStatus,
    write: AccessStatus,
) -> Result<()> {
    let transaction = begin_access_effect(access, read, write).await?;
    let (_, transaction) = transaction.accept().await?;
    transaction.commit().await
}

async fn restore_access(
    access: Arc<dyn AccessLifecycle>,
    read: AccessStatus,
    write: AccessStatus,
) -> Result<()> {
    match commit_access(access.clone(), read, write).await {
        Err(RuntimeError::ReconfigurationPending) => {
            access.defer_restored_access(read, write).await;
            Err(RuntimeError::ReconfigurationPending)
        }
        result => result,
    }
}

impl AccessClosure {
    pub(super) async fn set_access(&self, read: AccessStatus, write: AccessStatus) -> Result<()> {
        commit_access(self.inner.clone(), read, write).await
    }
}

#[derive(Clone)]
pub(super) struct AccessRuntime {
    inner: Arc<dyn AccessLifecycle>,
}

impl AccessRuntime {
    pub(super) async fn begin_effect(
        &self,
        read: AccessStatus,
        write: AccessStatus,
    ) -> Result<ReadyAccessTransaction> {
        begin_access_effect(self.inner.clone(), read, write).await
    }

    pub(super) async fn restore(&self, read: AccessStatus, write: AccessStatus) -> Result<()> {
        restore_access(self.inner.clone(), read, write).await
    }
}

#[derive(Clone)]
pub(super) struct ReportLifecycle {
    access: Arc<dyn AccessLifecycle>,
    observation: Arc<dyn LifecycleObservation>,
}

impl ReportLifecycle {
    pub(super) async fn observe_progress(&self) -> Result<()> {
        self.observation.observe_progress().await?;
        if let Some((read, write)) = self.access.restored_access().await {
            commit_access(self.access.clone(), read, write).await?;
        }
        Ok(())
    }

    pub(super) async fn reconcile_access(
        &self,
        read: AccessStatus,
        write: AccessStatus,
    ) -> Result<()> {
        restore_access(self.access.clone(), read, write).await
    }

    pub(super) async fn snapshot(&self) -> RuntimeSnapshot {
        self.observation.snapshot().await
    }
}

#[derive(Clone)]
pub(super) struct EvidenceRuntime {
    inner: Arc<dyn LifecycleObservation>,
}

impl EvidenceRuntime {
    pub(super) async fn snapshot(&self) -> RuntimeSnapshot {
        self.inner.snapshot().await
    }
}

#[derive(Clone)]
pub(super) struct EffectEvidenceRuntime {
    inner: Arc<dyn LifecycleObservation>,
}

impl EffectEvidenceRuntime {
    pub(super) async fn refresh_progress(&self) -> Result<()> {
        self.inner.refresh_progress().await
    }

    pub(super) async fn postcondition(
        &self,
        progress: Option<&NativeProgressStatus>,
    ) -> RuntimePostcondition {
        self.inner.postcondition(progress).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FailingAccess;

    #[async_trait]
    impl AccessLifecycle for FailingAccess {
        async fn defer_restored_access(&self, _read: AccessStatus, _write: AccessStatus) {}

        async fn run_access_transaction(
            &self,
            _read: AccessStatus,
            _write: AccessStatus,
            _ready: oneshot::Sender<()>,
            _accept: oneshot::Receiver<()>,
            _accepted: oneshot::Sender<Option<NativeProgressStatus>>,
            _decision: oneshot::Receiver<AccessDecision>,
        ) -> Result<()> {
            Err(RuntimeError::Application("pre-ready failure".into()))
        }

        async fn restored_access(&self) -> Option<(AccessStatus, AccessStatus)> {
            None
        }
    }

    #[tokio::test]
    async fn access_closure_preserves_pre_ready_failure() {
        let closure = AccessClosure {
            inner: Arc::new(FailingAccess),
        };
        assert!(matches!(
            closure
                .set_access(
                    AccessStatus::ReconfigurationPending,
                    AccessStatus::ReconfigurationPending,
                )
                .await,
            Err(RuntimeError::Application(message)) if message == "pre-ready failure"
        ));
    }
}
