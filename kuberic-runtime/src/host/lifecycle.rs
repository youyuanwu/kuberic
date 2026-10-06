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

use super::custom::{AccessCommit, AccessDecision, BuildAdmission, BuildCompletionConfirmation};

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

impl AccessClosure {
    pub(super) async fn set_access(&self, read: AccessStatus, write: AccessStatus) -> Result<()> {
        let (ready_tx, ready_rx) = oneshot::channel();
        let (accept_tx, accept_rx) = oneshot::channel();
        let (accepted_tx, accepted_rx) = oneshot::channel();
        let (decision_tx, decision_rx) = oneshot::channel();
        let access = self.inner.clone();
        let completion = tokio::spawn(async move {
            access
                .run_access_transaction(read, write, ready_tx, accept_rx, accepted_tx, decision_rx)
                .await
        });
        ready_rx
            .await
            .map_err(|_| RuntimeError::OperationCancelled)?;
        let _ = accept_tx.send(());
        accepted_rx
            .await
            .map_err(|_| RuntimeError::OperationCancelled)?;
        let _ = decision_tx.send(AccessDecision::Commit(AccessCommit::Direct));
        completion
            .await
            .map_err(|error| RuntimeError::Application(error.to_string()))?
    }
}
