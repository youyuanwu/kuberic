//! Private SF lifecycle/effect hosting, independent of operation/copy capability.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex, Weak};

use async_trait::async_trait;
use kuberic_protocol::types::{
    AccessStatus, ConfigurationDescriptor, OperationId, ProcessSessionId, ReplicaIdentity,
};
use kuberic_runtime::replicator::{
    ManagedReplicatorLifecycle, PrimaryReplicator, ReplicaInformation, ReplicaSetConfiguration,
    ReplicaSetQuorumMode, Replicator,
};
use kuberic_runtime::{Result, RuntimeError};
use kuberic_runtime_internal::authority::{
    AdmittedAuthority, BuildAuthority, BuildSelection, DurableBuildProgress,
};
use kuberic_runtime_internal::effects::{
    BuildPostcondition, RuntimeEffectAction, RuntimeOperationEvidence, RuntimePostcondition,
    RuntimeSnapshot, TopologyEvidenceKind, TopologyOperationEvidence,
};
use kuberic_runtime_internal::receipts::{
    AccessReceipt, BuildReceipt as NativeBuildReceipt, CatchUpReceipt, CertifiedPrefixReceipt,
    NativeOperationToken, RemovalReceipt, RetirementReceipt, SecondaryRemovalReceipt,
    SwitchoverReceipt,
};
use kuberic_runtime_internal::transport::{OutboundOperation, ReplicaEndpoint};
use tokio::sync::{Mutex, Notify, RwLock, mpsc};

use super::{RuntimeHost, empty_snapshot};
#[path = "custom_removal.rs"]
mod removal;

#[derive(Clone, PartialEq, Eq)]
pub(super) struct BuildAdmission {
    selection: BuildSelection,
    source_session: ProcessSessionId,
    target_session: ProcessSessionId,
    attempt_generation: u64,
    configuration_generation: u64,
    native_token: Option<NativeOperationToken>,
}

impl BuildAdmission {
    fn matches_durable_selection(&self, other: &Self) -> bool {
        self.selection == other.selection
            && self.source_session == other.source_session
            && self.target_session == other.target_session
            && self.attempt_generation == other.attempt_generation
    }

    fn matches_host_admission(&self, other: &Self) -> bool {
        self.matches_durable_selection(other)
            && self.configuration_generation == other.configuration_generation
    }
}

#[derive(Clone, PartialEq, Eq)]
struct AcceptedBuildReceipt {
    admission: BuildAdmission,
    native: NativeBuildReceipt,
}

#[async_trait]
trait AccessRollbackFence: Send + Sync {
    async fn fence_writes(&self);
    fn abort(&self);
}

struct ManagedAccessRollbackFence(Arc<dyn ManagedReplicatorLifecycle>);
struct CommonAccessRollbackFence;

#[async_trait]
impl AccessRollbackFence for ManagedAccessRollbackFence {
    async fn fence_writes(&self) {
        let _ = self.0.fence_writes().await;
    }

    fn abort(&self) {
        self.0.abort();
    }
}

#[async_trait]
impl AccessRollbackFence for CommonAccessRollbackFence {
    async fn fence_writes(&self) {}

    fn abort(&self) {}
}

struct AccessPublicationRollback {
    fence: Arc<dyn AccessRollbackFence>,
    host: Weak<RuntimeHost>,
    common_state: Arc<RwLock<RuntimeSnapshot>>,
    access_generation: Arc<AtomicU64>,
    published_access_generation: Arc<AtomicU64>,
    access_commit: Arc<Mutex<()>>,
    generation: u64,
    armed: bool,
}

pub(super) struct AccessEffectCommit {
    rollback: Option<AccessPublicationRollback>,
}

impl AccessEffectCommit {
    fn managed(rollback: AccessPublicationRollback) -> Self {
        Self {
            rollback: Some(rollback),
        }
    }

    pub(super) fn commit(mut self) {
        if let Some(rollback) = self.rollback.as_mut() {
            rollback.disarm();
        }
        self.rollback = None;
    }

    pub(super) async fn lock_acceptance(&self) -> Result<Option<tokio::sync::OwnedMutexGuard<()>>> {
        let Some(rollback) = self.rollback.as_ref() else {
            return Ok(None);
        };
        let guard = rollback.access_commit.clone().lock_owned().await;
        if rollback.access_generation.load(Ordering::Acquire) != rollback.generation
            || rollback.published_access_generation.load(Ordering::Acquire) != rollback.generation
        {
            return Err(RuntimeError::OperationCancelled);
        }
        Ok(Some(guard))
    }
}

impl AccessPublicationRollback {
    fn with_fence(
        fence: Arc<dyn AccessRollbackFence>,
        common: &CustomReplicatorHost,
        projection: &AccessProjection,
    ) -> Self {
        Self {
            fence,
            host: common.host.clone(),
            common_state: common.state.clone(),
            access_generation: common.access_generation.clone(),
            published_access_generation: common.published_access_generation.clone(),
            access_commit: common.access_commit.clone(),
            generation: projection.access_generation,
            armed: true,
        }
    }

    fn managed(
        lifecycle: Arc<dyn ManagedReplicatorLifecycle>,
        common: &CustomReplicatorHost,
        projection: &AccessProjection,
    ) -> Self {
        Self::with_fence(
            Arc::new(ManagedAccessRollbackFence(lifecycle)),
            common,
            projection,
        )
    }

    fn common(common: &CustomReplicatorHost, projection: &AccessProjection) -> Self {
        Self::with_fence(Arc::new(CommonAccessRollbackFence), common, projection)
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for AccessPublicationRollback {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let fence = self.fence.clone();
        let host = self.host.clone();
        let common_state = self.common_state.clone();
        let access_generation = self.access_generation.clone();
        let published_access_generation = self.published_access_generation.clone();
        let access_commit = self.access_commit.clone();
        let generation = self.generation;
        let rollback_generation = self.generation.saturating_add(1);
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                let _commit = access_commit.lock().await;
                if published_access_generation.load(Ordering::Acquire) > generation {
                    return;
                }
                if access_generation.load(Ordering::Acquire) == generation {
                    access_generation.store(rollback_generation, Ordering::Release);
                }
                fence.fence_writes().await;
                let mut state = common_state.write().await;
                state.read_status = AccessStatus::ReconfigurationPending;
                state.write_status = AccessStatus::ReconfigurationPending;
                drop(state);
                if let Some(host) = host.upgrade() {
                    let mut state = host.state.write().await;
                    state.fallback_snapshot.read_status = AccessStatus::ReconfigurationPending;
                    state.fallback_snapshot.write_status = AccessStatus::ReconfigurationPending;
                }
            });
        } else {
            fence.abort();
        }
    }
}

#[derive(Clone)]
struct AccessProjection {
    read: AccessStatus,
    write: AccessStatus,
    authority: Option<AdmittedAuthority>,
    configuration: Option<ReplicaSetConfiguration>,
    sessions: BTreeMap<ReplicaIdentity, ProcessSessionId>,
    role: kuberic_protocol::types::ReplicaRole,
    configuration_generation: u64,
    access_generation: u64,
    faulted_grant: bool,
}

fn validate_access_receipt(
    preparation: &AccessReceipt,
    receipt: &AccessReceipt,
    projection: &AccessProjection,
    published: bool,
) -> Result<()> {
    let expected = AccessReceipt {
        published,
        ..preparation.clone()
    };
    if receipt != &expected
        || receipt.authority != projection.authority
        || receipt.read != projection.read
        || receipt.write != projection.write
        || receipt.engine_session_id.is_empty()
    {
        return Err(RuntimeError::OperationCancelled);
    }
    Ok(())
}

fn validate_catch_up_receipt(
    receipt: &CatchUpReceipt,
    token: &NativeOperationToken,
    authority: Option<&AdmittedAuthority>,
    observed_progress: i64,
) -> Result<()> {
    if authority != Some(&receipt.authority)
        || token.authority.as_ref() != authority
        || receipt.engine_session_id != token.engine_session_id
        || receipt.engine_generation != token.engine_generation
        || receipt.boundary_lsn > receipt.current_progress
        || receipt.current_progress < observed_progress
        || receipt.committed_lsn > receipt.current_progress
    {
        return Err(RuntimeError::OperationCancelled);
    }
    Ok(())
}

fn validate_build_receipt(native: &NativeBuildReceipt, admission: &BuildAdmission) -> Result<()> {
    if native.selection != admission.selection
        || native.progress.authority != admission.selection.authority
        || !native.progress.completed
        || native.progress.durable_lsn < native.progress.authority.replication_boundary_lsn
        || admission.native_token.as_ref().is_none_or(|token| {
            token.authority.as_ref().is_none_or(|authority| {
                authority.current_configuration != native.progress.authority.current_configuration
            }) || native.engine_session_id != token.engine_session_id
                || native.engine_generation != token.engine_generation
        })
    {
        return Err(RuntimeError::OperationCancelled);
    }
    Ok(())
}

fn validate_removal_receipt(
    receipt: &RemovalReceipt,
    token: &NativeOperationToken,
    authority: Option<&AdmittedAuthority>,
    replica_id: kuberic_protocol::types::ReplicaId,
) -> Result<()> {
    if authority != Some(&receipt.authority)
        || token.authority.as_ref() != authority
        || receipt.replica_id != replica_id
        || receipt.engine_session_id != token.engine_session_id
        || receipt.engine_generation != token.engine_generation
        || receipt
            .authority
            .current_configuration
            .members
            .iter()
            .chain(
                receipt
                    .authority
                    .previous_configuration
                    .iter()
                    .flat_map(|configuration| &configuration.members),
            )
            .any(|member| member.identity.replica_id == replica_id)
    {
        return Err(RuntimeError::OperationCancelled);
    }
    Ok(())
}

fn validate_native_token(
    receipt: &NativeOperationToken,
    expected: &NativeOperationToken,
) -> Result<()> {
    if receipt != expected || receipt.engine_session_id.is_empty() {
        return Err(RuntimeError::OperationCancelled);
    }
    Ok(())
}

fn validate_certified_prefix_receipt(
    receipt: &CertifiedPrefixReceipt,
    token: &NativeOperationToken,
) -> Result<()> {
    validate_native_token(&receipt.token, token)?;
    if receipt.settled_lsn < 0 || receipt.settled_lsn > receipt.verified_lsn {
        return Err(RuntimeError::OperationCancelled);
    }
    Ok(())
}

fn validate_secondary_removal_receipt(
    receipt: &SecondaryRemovalReceipt,
    token: &NativeOperationToken,
) -> Result<()> {
    validate_native_token(&receipt.token, token)
}

fn validate_retirement_receipt(
    receipt: &RetirementReceipt,
    retired: &kuberic_runtime_internal::authority::RetiredAuthority,
    completed: bool,
) -> Result<()> {
    if &receipt.retired != retired
        || receipt.completed != completed
        || receipt.engine_session_id.is_empty()
    {
        return Err(RuntimeError::OperationCancelled);
    }
    Ok(())
}

fn topology_evidence(metadata: TopologyOperationEvidence) -> RuntimeOperationEvidence {
    RuntimeOperationEvidence::Topology(
        serde_json::to_string(&metadata).expect("topology receipt metadata is serializable"),
    )
}

type DeferredConfiguration = tokio::task::JoinHandle<Result<(u64, ReplicaSetConfiguration)>>;

#[async_trait]
trait ReplicatorLifecycleBackend: Send + Sync {
    fn owns_stream_session(&self) -> bool;

    async fn complete_open(&self, address: String) -> Result<()>;
    async fn complete_close(&self) -> Result<()>;
    async fn complete_abort(&self);
    fn notify_abort(&self);
    async fn fence_writes(&self) -> Result<()>;
    async fn settle_primary_prefix(&self) -> Result<()>;
    async fn cancel_configuration_work(&self) -> Result<()>;
    async fn restore_authority(&self) -> Result<()>;
    async fn restore_access(&self, read: AccessStatus, write: AccessStatus) -> Result<()>;
    async fn admit_authority(&self, authority: AdmittedAuthority) -> Result<()>;
    async fn admit_build_authority(&self, authority: BuildAuthority) -> Result<()>;
    async fn register_peer_session(
        &self,
        identity: ReplicaIdentity,
        session: ProcessSessionId,
    ) -> Result<()>;
    async fn retire_build(&self, build_id: OperationId) -> Result<()>;
    async fn set_access(&self, read: AccessStatus, write: AccessStatus) -> Result<()>;
    async fn begin_access_effect(
        &self,
        read: AccessStatus,
        write: AccessStatus,
    ) -> Result<AccessEffectCommit>;
    async fn wait_for_catch_up(&self) -> Result<()>;
    async fn authorize_failover_prefix(&self, boundary: i64) -> Result<()>;
    async fn prepare_switchover(
        &self,
        preparation_generation: u64,
        request_id: kuberic_protocol::types::SwitchoverRequestId,
        source: ReplicaIdentity,
        target: ReplicaIdentity,
        starting_configuration_id: kuberic_protocol::types::ConfigurationId,
        starting_epoch: kuberic_protocol::types::Epoch,
    ) -> Result<()>;
    async fn refresh_progress(&self) -> Result<()>;
    async fn observe_progress(&self) -> Result<()>;
    async fn prepare_secondary_removal(
        &self,
        intent: kuberic_protocol::types::SecondaryScaleDownIntent,
        process_session_id: ProcessSessionId,
        report_sequence: u64,
    ) -> Result<()>;
    async fn observe_secondary_removal(
        &self,
        witness: kuberic_protocol::types::SecondaryRemovalWitness,
    ) -> Result<()>;
    async fn observe_secondary_removal_progress(
        &self,
        witness: kuberic_protocol::types::SecondaryRemovalWitness,
        committed: kuberic_protocol::types::SecondaryScaleDownCleanup,
    ) -> Result<()>;
    async fn accept_secondary_removal(
        &self,
        committed: kuberic_protocol::types::SecondaryScaleDownCleanup,
    ) -> Result<()>;
    async fn accept_historical_secondary_removal(
        &self,
        command: kuberic_protocol::command::AcceptSecondaryRemovalCommit,
    ) -> Result<()>;
    async fn fence_retirement(
        &self,
        retired: kuberic_runtime_internal::authority::RetiredAuthority,
    ) -> Result<()>;
    async fn complete_retirement(
        &self,
        retired: kuberic_runtime_internal::authority::RetiredAuthority,
    ) -> Result<()>;
    async fn snapshot(&self) -> RuntimeSnapshot;
    async fn cancel_outbound_build(&self, id: &OperationId) -> Result<()>;
    async fn cancel_outbound_build_attempt(&self, id: &OperationId, generation: u64) -> Result<()>;
    async fn build_generation(&self, id: &OperationId) -> u64;
    async fn next_outbound(&self) -> Option<OutboundOperation>;
    async fn wait_for_build_completion(
        &self,
        build_id: &OperationId,
        target: &ReplicaIdentity,
    ) -> Result<()>;
    async fn build_replica(&self, replica: ReplicaInformation) -> Result<()>;
    async fn remove_replica(&self, replica_id: kuberic_protocol::types::ReplicaId) -> Result<()>;
    async fn select_build(&self, authority: &BuildAuthority) -> Result<()>;
    async fn describe_peer(&self, replica: ReplicaInformation) -> Result<()>;
    async fn execute_build(&self, replica: ReplicaInformation) -> Result<Option<BuildAdmission>>;
    async fn accept_build(&self, receipt: Option<BuildAdmission>) -> Result<()>;
    async fn enqueue_build(&self, endpoint: ReplicaEndpoint) -> Result<()>;
    async fn effect_evidence(
        &self,
        action: &RuntimeEffectAction,
    ) -> Option<RuntimeOperationEvidence>;
    async fn confirm_build_completion(
        &self,
        build_id: &OperationId,
        target: &ReplicaIdentity,
    ) -> Result<Option<RuntimeOperationEvidence>>;
    async fn postcondition(&self) -> RuntimePostcondition;
}

struct ManagedLifecycleBackend {
    legacy: Arc<dyn ManagedReplicatorLifecycle>,
    common: CustomReplicatorHost,
    accepted_builds: RwLock<BTreeMap<OperationId, AcceptedBuildReceipt>>,
    catch_up_receipt: RwLock<Option<CatchUpReceipt>>,
    access_receipt: RwLock<Option<AccessReceipt>>,
    certified_prefix_receipt: RwLock<Option<CertifiedPrefixReceipt>>,
    switchover_receipt: RwLock<Option<SwitchoverReceipt>>,
    secondary_removal_receipt: RwLock<Option<SecondaryRemovalReceipt>>,
    retirement_receipt: RwLock<Option<RetirementReceipt>>,
}

#[async_trait]
impl ReplicatorLifecycleBackend for ManagedLifecycleBackend {
    fn owns_stream_session(&self) -> bool {
        true
    }

    async fn complete_open(&self, address: String) -> Result<()> {
        self.common.complete_open_common(address.clone()).await;
        self.legacy.complete_open(address).await?;
        self.sync_engine_proof().await
    }

    async fn complete_close(&self) -> Result<()> {
        self.common.terminate().await
    }

    async fn complete_abort(&self) {
        self.common.notify_abort();
        self.common.terminate().await.ok();
    }

    fn notify_abort(&self) {
        self.common.notify_abort();
    }

    async fn fence_writes(&self) -> Result<()> {
        self.common.fence_managed_access().await?;
        self.legacy.fence_writes().await?;
        self.sync_engine_proof().await
    }

    async fn settle_primary_prefix(&self) -> Result<()> {
        let token = self.legacy.operation_token().await?;
        let receipt = self.legacy.settle_primary_prefix().await?;
        validate_certified_prefix_receipt(&receipt, &token)?;
        if receipt.committed_lsn != receipt.settled_lsn {
            return Err(RuntimeError::OperationCancelled);
        }
        self.common.settle_primary_prefix().await?;
        self.common.accept_certified_prefix(&receipt).await?;
        *self.certified_prefix_receipt.write().await = Some(receipt);
        Ok(())
    }

    async fn cancel_configuration_work(&self) -> Result<()> {
        self.common.cancel_configuration_work().await?;
        self.legacy.cancel_configuration_work().await?;
        self.legacy.fence_writes().await?;
        self.sync_engine_proof().await
    }

    async fn restore_authority(&self) -> Result<()> {
        self.legacy.restore_engine_proof().await?;
        self.common.restore_authority().await?;
        self.sync_engine_proof().await
    }

    async fn restore_access(&self, read: AccessStatus, write: AccessStatus) -> Result<()> {
        let preparation = self.legacy.prepare_access(read, write).await?;
        let projection = match self.common.reserve_access_projection(read, write).await {
            Err(RuntimeError::ReconfigurationPending) => {
                *self.common.restored_access.write().await = Some((read, write));
                return Err(RuntimeError::ReconfigurationPending);
            }
            result => result?,
        };
        let mut rollback =
            AccessPublicationRollback::managed(self.legacy.clone(), &self.common, &projection);
        self.common.complete_access_projection(&projection).await?;
        validate_access_receipt(&preparation, &preparation, &projection, false)?;
        let publication = match self.legacy.publish_access(preparation.clone()).await {
            Ok(publication) => publication,
            Err(error) => return Err(error),
        };
        validate_access_receipt(&preparation, &publication, &projection, true)?;
        self.common.publish_access_projection(projection).await?;
        rollback.disarm();
        Ok(())
    }

    async fn admit_authority(&self, authority: AdmittedAuthority) -> Result<()> {
        self.common.prepare_authority_admission(&authority).await?;
        if let Err(error) = self.legacy.admit_authority_proof(authority.clone()).await {
            self.sync_engine_proof().await?;
            return Err(error);
        }
        self.common.install_managed_authority(&authority).await?;
        self.sync_engine_proof().await
    }

    async fn admit_build_authority(&self, authority: BuildAuthority) -> Result<()> {
        self.legacy
            .admit_build_authority_proof(authority.clone())
            .await?;
        self.common.install_managed_build(&authority).await?;
        self.sync_engine_proof().await
    }

    async fn register_peer_session(
        &self,
        identity: ReplicaIdentity,
        session: ProcessSessionId,
    ) -> Result<()> {
        let registered_engine_peer =
            self.legacy
                .snapshot()
                .await
                .authority
                .as_ref()
                .is_some_and(|authority| {
                    authority
                        .current_configuration
                        .members
                        .iter()
                        .chain(
                            authority
                                .previous_configuration
                                .iter()
                                .flat_map(|configuration| &configuration.members),
                        )
                        .any(|member| member.identity == identity)
                });
        if registered_engine_peer {
            self.legacy
                .register_peer_session_proof(identity.clone(), session.clone())
                .await?;
        }
        self.common
            .execute_common_build_action(RuntimeEffectAction::RegisterPeerSession {
                identity,
                session,
            })
            .await
    }

    async fn retire_build(&self, build_id: OperationId) -> Result<()> {
        self.accepted_builds.write().await.remove(&build_id);
        self.legacy.retire_build_proof(build_id.clone()).await?;
        self.common.retire_managed_build(build_id).await?;
        self.sync_engine_proof().await
    }

    async fn set_access(&self, read: AccessStatus, write: AccessStatus) -> Result<()> {
        self.execute_access(read, write).await?.commit();
        Ok(())
    }

    async fn begin_access_effect(
        &self,
        read: AccessStatus,
        write: AccessStatus,
    ) -> Result<AccessEffectCommit> {
        self.execute_access(read, write).await
    }

    async fn wait_for_catch_up(&self) -> Result<()> {
        let native_token = self.legacy.operation_token().await?;
        let (authority_before, sessions_before, configuration_generation) = {
            let _gate = self.common.gate.lock().await;
            (
                self.common.state.read().await.authority.clone(),
                self.common.sessions.read().await.clone(),
                self.common.configuration_generation.load(Ordering::Acquire),
            )
        };
        self.common
            .primary
            .wait_for_catch_up_quorum(ReplicaSetQuorumMode::WriteQuorum)
            .await?;
        let receipt = self.legacy.catch_up_receipt().await?;
        self.common.active_host()?;
        let _gate = self.common.gate.lock().await;
        if authority_before != self.common.state.read().await.authority
            || sessions_before != *self.common.sessions.read().await
            || configuration_generation
                != self.common.configuration_generation.load(Ordering::Acquire)
        {
            return Err(RuntimeError::OperationCancelled);
        }
        validate_catch_up_receipt(
            &receipt,
            &native_token,
            authority_before.as_ref(),
            self.common.state.read().await.current_progress,
        )?;
        self.common.accept_catch_up_receipt(&receipt).await?;
        *self.catch_up_receipt.write().await = Some(receipt);
        Ok(())
    }

    async fn authorize_failover_prefix(&self, boundary: i64) -> Result<()> {
        let token = self.legacy.operation_token().await?;
        let receipt = self
            .legacy
            .authorize_failover_prefix_proof(boundary)
            .await?;
        validate_certified_prefix_receipt(&receipt, &token)?;
        if receipt.settled_lsn != boundary {
            return Err(RuntimeError::OperationCancelled);
        }
        self.common.accept_certified_prefix(&receipt).await?;
        *self.certified_prefix_receipt.write().await = Some(receipt);
        Ok(())
    }

    async fn prepare_switchover(
        &self,
        preparation_generation: u64,
        request_id: kuberic_protocol::types::SwitchoverRequestId,
        source: ReplicaIdentity,
        target: ReplicaIdentity,
        starting_configuration_id: kuberic_protocol::types::ConfigurationId,
        starting_epoch: kuberic_protocol::types::Epoch,
    ) -> Result<()> {
        let expected_authority = self.common.state.read().await.authority.clone();
        let expected_request = request_id.clone();
        let expected_source = source.clone();
        let expected_target = target.clone();
        let expected_configuration = starting_configuration_id.clone();
        self.common.fence_managed_access().await?;
        let receipt = self
            .legacy
            .prepare_switchover_proof(
                preparation_generation,
                request_id,
                source,
                target,
                starting_configuration_id,
                starting_epoch,
            )
            .await?;
        let current_token = self.legacy.operation_token().await?;
        validate_native_token(&receipt.token, &current_token)?;
        if receipt.token.authority != expected_authority {
            return Err(RuntimeError::OperationCancelled);
        }
        if receipt.preparation_generation != preparation_generation
            || receipt.request_id != expected_request
            || receipt.source != expected_source
            || receipt.target != expected_target
            || receipt.starting_configuration_id != expected_configuration
            || receipt.starting_epoch != starting_epoch
        {
            return Err(RuntimeError::OperationCancelled);
        }
        self.common.accept_switchover_receipt(&receipt).await?;
        *self.switchover_receipt.write().await = Some(receipt);
        Ok(())
    }

    async fn refresh_progress(&self) -> Result<()> {
        self.legacy.refresh_progress_proof().await?;
        self.common
            .apply_common_action(RuntimeEffectAction::RefreshApplicationProgress)
            .await?;
        self.sync_engine_proof().await
    }

    async fn observe_progress(&self) -> Result<()> {
        self.common.retry_restored_access().await
    }

    async fn prepare_secondary_removal(
        &self,
        intent: kuberic_protocol::types::SecondaryScaleDownIntent,
        process_session_id: ProcessSessionId,
        report_sequence: u64,
    ) -> Result<()> {
        self.execute_removal_action(RuntimeEffectAction::PrepareSecondaryRemoval {
            intent: Box::new(intent),
            process_session_id,
            report_sequence,
        })
        .await
    }

    async fn observe_secondary_removal(
        &self,
        witness: kuberic_protocol::types::SecondaryRemovalWitness,
    ) -> Result<()> {
        self.execute_removal_action(RuntimeEffectAction::ObserveSecondaryRemovalWitness(
            Box::new(witness),
        ))
        .await
    }

    async fn observe_secondary_removal_progress(
        &self,
        witness: kuberic_protocol::types::SecondaryRemovalWitness,
        committed: kuberic_protocol::types::SecondaryScaleDownCleanup,
    ) -> Result<()> {
        self.execute_removal_action(RuntimeEffectAction::ObserveSecondaryRemovalProgress {
            witness: Box::new(witness),
            committed: Box::new(committed),
        })
        .await
    }

    async fn accept_secondary_removal(
        &self,
        committed: kuberic_protocol::types::SecondaryScaleDownCleanup,
    ) -> Result<()> {
        self.execute_removal_action(RuntimeEffectAction::AcceptSecondaryRemovalCommit(Box::new(
            committed,
        )))
        .await
    }

    async fn accept_historical_secondary_removal(
        &self,
        command: kuberic_protocol::command::AcceptSecondaryRemovalCommit,
    ) -> Result<()> {
        self.execute_removal_action(RuntimeEffectAction::AcceptHistoricalSecondaryRemovalCommit(
            Box::new(command),
        ))
        .await
    }

    async fn fence_retirement(
        &self,
        retired: kuberic_runtime_internal::authority::RetiredAuthority,
    ) -> Result<()> {
        self.execute_removal_action(RuntimeEffectAction::FenceRetirement(Box::new(retired)))
            .await
    }

    async fn complete_retirement(
        &self,
        retired: kuberic_runtime_internal::authority::RetiredAuthority,
    ) -> Result<()> {
        self.execute_removal_action(RuntimeEffectAction::CompleteRetirement(Box::new(retired)))
            .await
    }

    async fn snapshot(&self) -> RuntimeSnapshot {
        let mut snapshot = self.common.snapshot().await;
        let engine = self.engine_snapshot_for_host().await;
        snapshot.current_progress = engine.current_progress;
        snapshot.committed_lsn = engine.committed_lsn;
        snapshot.verified_replication_lsn = engine.verified_replication_lsn;
        snapshot.current_configuration_quorum_progress =
            engine.current_configuration_quorum_progress;
        snapshot.catch_up_boundary = if engine.catch_up_complete {
            snapshot.catch_up_boundary.or(engine.catch_up_boundary)
        } else {
            engine.catch_up_boundary
        };
        snapshot.catch_up_complete = engine.catch_up_complete;
        merge_builds(&mut snapshot.builds, engine.builds);
        snapshot.live_builds_only = false;
        snapshot.prepared_secondary_removal = engine.prepared_secondary_removal;
        snapshot.accepted_secondary_removal = engine.accepted_secondary_removal;
        snapshot.retired_authority = engine.retired_authority;
        snapshot
    }

    async fn cancel_outbound_build(&self, id: &OperationId) -> Result<()> {
        self.accepted_builds.write().await.remove(id);
        self.common.cancel_common_build(id).await?;
        self.legacy.cancel_outbound_build(id).await
    }

    async fn cancel_outbound_build_attempt(&self, id: &OperationId, generation: u64) -> Result<()> {
        if !self
            .common
            .cancel_common_build_attempt(id, generation)
            .await?
        {
            return Ok(());
        }
        self.accepted_builds.write().await.remove(id);
        self.legacy.cancel_outbound_build(id).await?;
        self.common
            .complete_common_build_cancellation(id, generation)
            .await;
        Ok(())
    }

    async fn build_generation(&self, id: &OperationId) -> u64 {
        self.common.build_generation(id).await
    }

    async fn next_outbound(&self) -> Option<OutboundOperation> {
        self.common.next_outbound().await
    }

    async fn wait_for_build_completion(
        &self,
        build_id: &OperationId,
        target: &ReplicaIdentity,
    ) -> Result<()> {
        let generation = self.common.build_generation(build_id).await;
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(75);
        loop {
            let changed = self.common.changed.notified();
            let snapshot = self.snapshot().await;
            if snapshot.builds.iter().any(|build| {
                &build.authority.build_id == build_id
                    && &build.authority.target == target
                    && build.completed
            }) {
                return Ok(());
            }

            self.common
                .ensure_build_generation(build_id, generation)
                .await?;
            if tokio::time::Instant::now() >= deadline {
                return Err(RuntimeError::OperationCancelled);
            }
            tokio::select! {
                _ = changed => {}
                _ = tokio::time::sleep(std::time::Duration::from_millis(10)) => {}
            }
        }
    }

    async fn build_replica(&self, replica: ReplicaInformation) -> Result<()> {
        let build_id = replica.build_id.clone();
        let target = replica.identity.clone();
        if self.snapshot().await.builds.iter().any(|build| {
            build.authority.build_id == build_id
                && build.authority.target == target
                && build.completed
        }) {
            return Ok(());
        }
        let endpoint = ReplicaEndpoint {
            build_id: build_id.clone(),
            identity: replica.identity.clone(),
            replication_address: replica.replication_address.clone(),
        };
        self.common.enqueue_build_wait(endpoint).await?;
        self.wait_for_build_completion(&build_id, &target).await
    }

    async fn remove_replica(&self, replica_id: kuberic_protocol::types::ReplicaId) -> Result<()> {
        let native_token = self.legacy.operation_token().await?;
        let authority_before = self.common.state.read().await.authority.clone();
        let sessions_before = self.common.sessions.read().await.clone();
        let configuration_generation = self.common.configuration_generation.load(Ordering::Acquire);
        self.common.primary.remove_replica(replica_id).await?;
        let receipt = self.legacy.removal_receipt(replica_id).await?;
        let _gate = self.common.gate.lock().await;
        self.common.active_host()?;
        if authority_before != self.common.state.read().await.authority
            || sessions_before != *self.common.sessions.read().await
            || configuration_generation
                != self.common.configuration_generation.load(Ordering::Acquire)
        {
            return Err(RuntimeError::OperationCancelled);
        }
        validate_removal_receipt(
            &receipt,
            &native_token,
            authority_before.as_ref(),
            replica_id,
        )?;
        self.accepted_builds.write().await.retain(|_, receipt| {
            receipt.admission.selection.authority.target.replica_id != replica_id
        });
        self.common.remove_managed_replica(replica_id).await
    }

    async fn select_build(&self, authority: &BuildAuthority) -> Result<()> {
        let _gate = self.common.gate.lock().await;
        self.common.select_build_inner(authority).await
    }

    async fn describe_peer(&self, replica: ReplicaInformation) -> Result<()> {
        self.common.describe_peer(replica).await
    }

    async fn execute_build(&self, replica: ReplicaInformation) -> Result<Option<BuildAdmission>> {
        let mut replica = replica;
        let mut receipt = self.common.prepare_build(&mut replica).await?;
        receipt.native_token = Some(self.legacy.operation_token().await?);
        self.common.primary.build_replica(replica).await?;
        Ok(Some(receipt))
    }

    async fn accept_build(&self, receipt: Option<BuildAdmission>) -> Result<()> {
        let receipt = receipt.ok_or_else(|| {
            RuntimeError::Application("managed build acceptance omitted its receipt".into())
        })?;
        self.common.active_host()?;
        let _gate = self.common.gate.lock().await;
        if !self
            .common
            .receipt(&receipt.selection.authority)
            .await?
            .matches_host_admission(&receipt)
        {
            return Err(RuntimeError::OperationCancelled);
        }
        let native = self
            .legacy
            .build_receipt(
                &receipt.selection.authority.build_id,
                &receipt.selection.authority.target,
            )
            .await?;
        validate_build_receipt(&native, &receipt)?;
        self.legacy
            .detach_outbound_build_stream(&receipt.selection.authority.build_id)
            .await?;
        self.common.accept_managed_build(&native).await?;
        self.accepted_builds.write().await.insert(
            receipt.selection.authority.build_id.clone(),
            AcceptedBuildReceipt {
                admission: receipt.clone(),
                native,
            },
        );
        self.common
            .pending_builds
            .write()
            .await
            .remove(&receipt.selection.authority.build_id);
        self.common.changed.notify_waiters();
        Ok(())
    }

    async fn enqueue_build(&self, endpoint: ReplicaEndpoint) -> Result<()> {
        self.common.enqueue_build(endpoint).await
    }

    async fn effect_evidence(
        &self,
        action: &RuntimeEffectAction,
    ) -> Option<RuntimeOperationEvidence> {
        match action {
            RuntimeEffectAction::WaitForCatchup => self
                .catch_up_receipt
                .read()
                .await
                .clone()
                .map(|receipt| RuntimeOperationEvidence::CatchUp(Box::new(receipt))),
            RuntimeEffectAction::BuildReplica {
                build_id, target, ..
            } => self
                .confirm_build_completion(build_id, target)
                .await
                .ok()
                .flatten(),
            RuntimeEffectAction::SetAccessStatus { .. }
            | RuntimeEffectAction::SetReadStatus(_)
            | RuntimeEffectAction::SetWriteStatus(_) => self
                .access_receipt
                .read()
                .await
                .clone()
                .map(|receipt| RuntimeOperationEvidence::Access(Box::new(receipt))),
            RuntimeEffectAction::AuthorizeFailoverPrefix(_)
            | RuntimeEffectAction::ChangeApplicationRole(
                kuberic_protocol::types::ReplicaRole::Primary,
            ) => self
                .certified_prefix_receipt
                .read()
                .await
                .clone()
                .map(|receipt| {
                    topology_evidence(TopologyOperationEvidence {
                        kind: TopologyEvidenceKind::CertifiedPrefix,
                        engine_session_id: receipt.token.engine_session_id,
                        engine_generation: receipt.token.engine_generation,
                        operation_id: None,
                        request_id: None,
                        preparation_generation: None,
                        source: None,
                        target: None,
                        configuration_id: receipt.token.authority.as_ref().map(|authority| {
                            authority.current_configuration.configuration_id.clone()
                        }),
                        epoch: receipt
                            .token
                            .authority
                            .as_ref()
                            .map(|authority| authority.current_configuration.epoch),
                        boundary_lsn: Some(receipt.settled_lsn),
                        completed: true,
                    })
                }),
            RuntimeEffectAction::PrepareSwitchover { .. } => {
                self.switchover_receipt.read().await.clone().map(|receipt| {
                    topology_evidence(TopologyOperationEvidence {
                        kind: TopologyEvidenceKind::Switchover,
                        engine_session_id: receipt.token.engine_session_id,
                        engine_generation: receipt.token.engine_generation,
                        operation_id: None,
                        request_id: Some(receipt.request_id),
                        preparation_generation: Some(receipt.preparation_generation),
                        source: Some(receipt.source),
                        target: Some(receipt.target),
                        configuration_id: Some(receipt.starting_configuration_id),
                        epoch: Some(receipt.starting_epoch),
                        boundary_lsn: Some(receipt.handoff_lsn),
                        completed: true,
                    })
                })
            }
            RuntimeEffectAction::PrepareSecondaryRemoval { .. }
            | RuntimeEffectAction::ObserveSecondaryRemovalWitness(_)
            | RuntimeEffectAction::ObserveSecondaryRemovalProgress { .. }
            | RuntimeEffectAction::AcceptSecondaryRemovalCommit(_)
            | RuntimeEffectAction::AcceptHistoricalSecondaryRemovalCommit(_) => self
                .secondary_removal_receipt
                .read()
                .await
                .clone()
                .map(|receipt| {
                    let completed = matches!(
                        action,
                        RuntimeEffectAction::ObserveSecondaryRemovalProgress { .. }
                            | RuntimeEffectAction::AcceptSecondaryRemovalCommit(_)
                            | RuntimeEffectAction::AcceptHistoricalSecondaryRemovalCommit(_)
                    );
                    let preparation = receipt.preparation.as_ref().or_else(|| {
                        receipt
                            .accepted
                            .as_ref()
                            .map(|accepted| &accepted.evidence.preparation)
                    });
                    topology_evidence(TopologyOperationEvidence {
                        kind: TopologyEvidenceKind::SecondaryRemoval,
                        engine_session_id: receipt.token.engine_session_id,
                        engine_generation: receipt.token.engine_generation,
                        operation_id: preparation.map(|value| value.operation_id.clone()),
                        request_id: None,
                        preparation_generation: None,
                        source: preparation.map(|value| value.intent.primary.clone()),
                        target: preparation.map(|value| value.intent.target.clone()),
                        configuration_id: preparation.map(|value| {
                            value.intent.current_configuration.configuration_id.clone()
                        }),
                        epoch: preparation.map(|value| value.intent.current_configuration.epoch),
                        boundary_lsn: preparation.map(|value| value.boundary_lsn),
                        completed,
                    })
                }),
            RuntimeEffectAction::RetireReplica(_)
            | RuntimeEffectAction::FenceRetirement(_)
            | RuntimeEffectAction::CompleteRetirement(_) => {
                self.retirement_receipt.read().await.clone().map(|receipt| {
                    let intent = &receipt.retired.report.intent;
                    topology_evidence(TopologyOperationEvidence {
                        kind: TopologyEvidenceKind::Retirement,
                        engine_session_id: receipt.engine_session_id,
                        engine_generation: receipt.engine_generation,
                        operation_id: Some(receipt.retired.report.operation_id.clone()),
                        request_id: None,
                        preparation_generation: None,
                        source: Some(intent.primary.clone()),
                        target: Some(intent.target.clone()),
                        configuration_id: Some(
                            intent.current_configuration.configuration_id.clone(),
                        ),
                        epoch: Some(intent.current_configuration.epoch),
                        boundary_lsn: Some(
                            receipt.retired.committed.evidence.preparation.boundary_lsn,
                        ),
                        completed: receipt.completed,
                    })
                })
            }
            _ => None,
        }
    }

    async fn confirm_build_completion(
        &self,
        build_id: &OperationId,
        target: &ReplicaIdentity,
    ) -> Result<Option<RuntimeOperationEvidence>> {
        let accepted = self
            .accepted_builds
            .read()
            .await
            .get(build_id)
            .cloned()
            .ok_or(RuntimeError::ReconfigurationPending)?;
        if &accepted.admission.selection.authority.target != target
            || !self
                .common
                .receipt(&accepted.admission.selection.authority)
                .await?
                .matches_host_admission(&accepted.admission)
            || accepted.native.selection != accepted.admission.selection
            || !accepted.native.progress.completed
        {
            return Err(RuntimeError::ReconfigurationPending);
        }
        Ok(Some(RuntimeOperationEvidence::Build(Box::new(
            accepted.native,
        ))))
    }

    async fn postcondition(&self) -> RuntimePostcondition {
        self.common.narrow_postcondition().await
    }
}

impl ManagedLifecycleBackend {
    async fn engine_snapshot_for_host(&self) -> RuntimeSnapshot {
        let mut snapshot = self.legacy.snapshot().await;
        let accepted = self.accepted_builds.read().await.clone();
        for build in &mut snapshot.builds {
            if !build.completed
                || self
                    .common
                    .build_generation(&build.authority.build_id)
                    .await
                    == 0
            {
                continue;
            }
            build.completed = match accepted.get(&build.authority.build_id) {
                Some(receipt)
                    if receipt.admission.selection.authority == build.authority
                        && receipt.native.progress.authority == build.authority =>
                {
                    self.common
                        .receipt(&build.authority)
                        .await
                        .is_ok_and(|current| current.matches_host_admission(&receipt.admission))
                }
                _ => false,
            };
        }
        snapshot
    }

    async fn execute_removal_action(&self, action: RuntimeEffectAction) -> Result<()> {
        match action {
            RuntimeEffectAction::PrepareSecondaryRemoval {
                intent,
                process_session_id,
                report_sequence,
            } => {
                let token = self.legacy.operation_token().await?;
                let expected_intent = (*intent).clone();
                let expected_session = process_session_id.clone();
                self.common.fence_managed_access().await?;
                let receipt = self
                    .legacy
                    .prepare_secondary_removal_proof(*intent, process_session_id, report_sequence)
                    .await?;
                validate_secondary_removal_receipt(&receipt, &token)?;
                if receipt.preparation.as_ref().is_none_or(|preparation| {
                    preparation.intent != expected_intent
                        || preparation.process_session_id != expected_session
                        || preparation.report_sequence != report_sequence
                }) {
                    return Err(RuntimeError::OperationCancelled);
                }
                self.common
                    .accept_secondary_removal_receipt(&receipt)
                    .await?;
                *self.secondary_removal_receipt.write().await = Some(receipt);
                Ok(())
            }
            RuntimeEffectAction::ObserveSecondaryRemovalWitness(witness) => {
                let token = self.legacy.operation_token().await?;
                let expected = (*witness).clone();
                let receipt = self
                    .legacy
                    .observe_secondary_removal_proof(*witness)
                    .await?;
                validate_secondary_removal_receipt(&receipt, &token)?;
                if receipt.witness.as_ref() != Some(&expected) {
                    return Err(RuntimeError::OperationCancelled);
                }
                self.common
                    .accept_secondary_removal_receipt(&receipt)
                    .await?;
                *self.secondary_removal_receipt.write().await = Some(receipt);
                Ok(())
            }
            RuntimeEffectAction::ObserveSecondaryRemovalProgress { witness, committed } => {
                let token = self.legacy.operation_token().await?;
                let expected_witness = (*witness).clone();
                let expected_committed = (*committed).clone();
                let receipt = self
                    .legacy
                    .observe_secondary_removal_progress_proof(*witness, *committed)
                    .await?;
                validate_secondary_removal_receipt(&receipt, &token)?;
                if receipt.witness.as_ref() != Some(&expected_witness)
                    || receipt.accepted.as_ref() != Some(&expected_committed)
                {
                    return Err(RuntimeError::OperationCancelled);
                }
                self.common
                    .accept_secondary_removal_receipt(&receipt)
                    .await?;
                *self.secondary_removal_receipt.write().await = Some(receipt);
                Ok(())
            }
            RuntimeEffectAction::AcceptSecondaryRemovalCommit(committed) => {
                let token = self.legacy.operation_token().await?;
                let expected = (*committed).clone();
                let receipt = self
                    .legacy
                    .accept_secondary_removal_proof(*committed)
                    .await?;
                validate_secondary_removal_receipt(&receipt, &token)?;
                if receipt.accepted.as_ref() != Some(&expected) {
                    return Err(RuntimeError::OperationCancelled);
                }
                self.common
                    .accept_secondary_removal_receipt(&receipt)
                    .await?;
                *self.secondary_removal_receipt.write().await = Some(receipt);
                Ok(())
            }
            RuntimeEffectAction::AcceptHistoricalSecondaryRemovalCommit(command) => {
                let token = self.legacy.operation_token().await?;
                let receipt = self
                    .legacy
                    .accept_historical_secondary_removal_proof(*command)
                    .await?;
                validate_secondary_removal_receipt(&receipt, &token)?;
                self.common
                    .accept_secondary_removal_receipt(&receipt)
                    .await?;
                *self.secondary_removal_receipt.write().await = Some(receipt);
                Ok(())
            }
            RuntimeEffectAction::FenceRetirement(retired) => {
                let receipt = self
                    .legacy
                    .fence_retirement_proof((*retired).clone())
                    .await?;
                validate_retirement_receipt(&receipt, &retired, false)?;
                self.common.fence_retirement_state(&retired).await?;
                *self.retirement_receipt.write().await = Some(receipt);
                Ok(())
            }
            RuntimeEffectAction::CompleteRetirement(retired) => {
                let receipt = self
                    .legacy
                    .complete_retirement_proof((*retired).clone())
                    .await?;
                validate_retirement_receipt(&receipt, &retired, true)?;
                self.common.complete_retirement_state(&retired).await?;
                *self.retirement_receipt.write().await = Some(receipt);
                Ok(())
            }
            _ => Err(RuntimeError::Application(
                "managed removal proof requires a removal action".into(),
            )),
        }
    }

    async fn execute_access(
        &self,
        read: AccessStatus,
        write: AccessStatus,
    ) -> Result<AccessEffectCommit> {
        let preparation = self.legacy.prepare_access(read, write).await?;
        let projection = self.common.reserve_access_projection(read, write).await?;
        let rollback =
            AccessPublicationRollback::managed(self.legacy.clone(), &self.common, &projection);
        self.common.complete_access_projection(&projection).await?;
        validate_access_receipt(&preparation, &preparation, &projection, false)?;
        let publication = match self.legacy.publish_access(preparation.clone()).await {
            Ok(publication) => publication,
            Err(error) => return Err(error),
        };
        validate_access_receipt(&preparation, &publication, &projection, true)?;
        self.common.publish_access_projection(projection).await?;
        *self.access_receipt.write().await = Some(publication);
        Ok(AccessEffectCommit::managed(rollback))
    }

    async fn sync_engine_proof(&self) -> Result<()> {
        self.common
            .restore_engine_proof(self.engine_snapshot_for_host().await)
            .await
    }
}

pub(super) struct ReplicatorLifecycleHost {
    backend: Arc<dyn ReplicatorLifecycleBackend>,
}

impl ReplicatorLifecycleHost {
    pub(super) fn managed(
        host: Weak<RuntimeHost>,
        control: Arc<dyn Replicator>,
        primary: Arc<dyn PrimaryReplicator>,
        lifecycle: Arc<dyn ManagedReplicatorLifecycle>,
    ) -> Self {
        Self {
            backend: Arc::new(ManagedLifecycleBackend {
                legacy: lifecycle,
                common: CustomReplicatorHost::new(host, control, primary, false),
                accepted_builds: RwLock::default(),
                catch_up_receipt: RwLock::default(),
                access_receipt: RwLock::default(),
                certified_prefix_receipt: RwLock::default(),
                switchover_receipt: RwLock::default(),
                secondary_removal_receipt: RwLock::default(),
                retirement_receipt: RwLock::default(),
            }),
        }
    }

    pub(super) fn service(
        host: Weak<RuntimeHost>,
        control: Arc<dyn Replicator>,
        primary: Arc<dyn PrimaryReplicator>,
    ) -> Self {
        Self {
            backend: Arc::new(CustomReplicatorHost::new(host, control, primary, true)),
        }
    }

    pub(super) fn is_managed(&self) -> bool {
        self.backend.owns_stream_session()
    }

    pub(super) async fn complete_open(&self, address: String) -> Result<()> {
        self.backend.complete_open(address).await
    }

    pub(super) async fn complete_close(&self) -> Result<()> {
        self.backend.complete_close().await
    }

    pub(super) async fn complete_abort(&self) {
        self.backend.complete_abort().await;
    }

    pub(super) fn notify_abort(&self) {
        self.backend.notify_abort();
    }

    pub(super) async fn fence_writes(&self) -> Result<()> {
        self.backend.fence_writes().await
    }

    pub(super) async fn settle_primary_prefix(&self) -> Result<()> {
        self.backend.settle_primary_prefix().await
    }

    pub(super) async fn cancel_configuration_work(&self) -> Result<()> {
        self.backend.cancel_configuration_work().await
    }

    pub(super) async fn restore_authority(&self) -> Result<()> {
        self.backend.restore_authority().await
    }

    pub(super) async fn restore_access(
        &self,
        read: AccessStatus,
        write: AccessStatus,
    ) -> Result<()> {
        self.backend.restore_access(read, write).await
    }

    pub(super) async fn admit_authority(&self, authority: AdmittedAuthority) -> Result<()> {
        self.backend.admit_authority(authority).await
    }

    pub(super) async fn admit_build_authority(&self, authority: BuildAuthority) -> Result<()> {
        self.backend.admit_build_authority(authority).await
    }

    pub(super) async fn register_peer_session(
        &self,
        identity: ReplicaIdentity,
        session: ProcessSessionId,
    ) -> Result<()> {
        self.backend.register_peer_session(identity, session).await
    }

    pub(super) async fn retire_build(&self, build_id: OperationId) -> Result<()> {
        self.backend.retire_build(build_id).await
    }

    pub(super) async fn set_access(&self, read: AccessStatus, write: AccessStatus) -> Result<()> {
        self.backend.set_access(read, write).await
    }

    pub(super) async fn begin_access_effect(
        &self,
        read: AccessStatus,
        write: AccessStatus,
    ) -> Result<AccessEffectCommit> {
        self.backend.begin_access_effect(read, write).await
    }

    pub(super) async fn wait_for_catch_up(&self) -> Result<()> {
        self.backend.wait_for_catch_up().await
    }

    pub(super) async fn authorize_failover_prefix(&self, boundary: i64) -> Result<()> {
        self.backend.authorize_failover_prefix(boundary).await
    }

    pub(super) async fn prepare_switchover(
        &self,
        preparation_generation: u64,
        request_id: kuberic_protocol::types::SwitchoverRequestId,
        source: ReplicaIdentity,
        target: ReplicaIdentity,
        starting_configuration_id: kuberic_protocol::types::ConfigurationId,
        starting_epoch: kuberic_protocol::types::Epoch,
    ) -> Result<()> {
        self.backend
            .prepare_switchover(
                preparation_generation,
                request_id,
                source,
                target,
                starting_configuration_id,
                starting_epoch,
            )
            .await
    }

    pub(super) async fn refresh_progress(&self) -> Result<()> {
        self.backend.refresh_progress().await
    }

    pub(super) async fn observe_progress(&self) -> Result<()> {
        self.backend.observe_progress().await
    }

    pub(super) async fn prepare_secondary_removal(
        &self,
        intent: kuberic_protocol::types::SecondaryScaleDownIntent,
        process_session_id: ProcessSessionId,
        report_sequence: u64,
    ) -> Result<()> {
        self.backend
            .prepare_secondary_removal(intent, process_session_id, report_sequence)
            .await
    }

    pub(super) async fn observe_secondary_removal(
        &self,
        witness: kuberic_protocol::types::SecondaryRemovalWitness,
    ) -> Result<()> {
        self.backend.observe_secondary_removal(witness).await
    }

    pub(super) async fn observe_secondary_removal_progress(
        &self,
        witness: kuberic_protocol::types::SecondaryRemovalWitness,
        committed: kuberic_protocol::types::SecondaryScaleDownCleanup,
    ) -> Result<()> {
        self.backend
            .observe_secondary_removal_progress(witness, committed)
            .await
    }

    pub(super) async fn accept_secondary_removal(
        &self,
        committed: kuberic_protocol::types::SecondaryScaleDownCleanup,
    ) -> Result<()> {
        self.backend.accept_secondary_removal(committed).await
    }

    pub(super) async fn accept_historical_secondary_removal(
        &self,
        command: kuberic_protocol::command::AcceptSecondaryRemovalCommit,
    ) -> Result<()> {
        self.backend
            .accept_historical_secondary_removal(command)
            .await
    }

    pub(super) async fn fence_retirement(
        &self,
        retired: kuberic_runtime_internal::authority::RetiredAuthority,
    ) -> Result<()> {
        self.backend.fence_retirement(retired).await
    }

    pub(super) async fn complete_retirement(
        &self,
        retired: kuberic_runtime_internal::authority::RetiredAuthority,
    ) -> Result<()> {
        self.backend.complete_retirement(retired).await
    }

    pub(super) async fn snapshot(&self) -> RuntimeSnapshot {
        self.backend.snapshot().await
    }

    pub(super) async fn cancel_outbound_build(&self, id: &OperationId) -> Result<()> {
        self.backend.cancel_outbound_build(id).await
    }

    pub(super) async fn cancel_outbound_build_attempt(
        &self,
        id: &OperationId,
        generation: u64,
    ) -> Result<()> {
        self.backend
            .cancel_outbound_build_attempt(id, generation)
            .await
    }

    pub(super) async fn build_generation(&self, id: &OperationId) -> u64 {
        self.backend.build_generation(id).await
    }

    pub(super) async fn next_outbound(&self) -> Option<OutboundOperation> {
        self.backend.next_outbound().await
    }

    pub(super) async fn wait_for_build_completion(
        &self,
        build_id: &OperationId,
        target: &ReplicaIdentity,
    ) -> Result<()> {
        self.backend
            .wait_for_build_completion(build_id, target)
            .await
    }

    pub(super) async fn build_replica(&self, replica: ReplicaInformation) -> Result<()> {
        self.backend.build_replica(replica).await
    }

    pub(super) async fn remove_replica(
        &self,
        replica_id: kuberic_protocol::types::ReplicaId,
    ) -> Result<()> {
        self.backend.remove_replica(replica_id).await
    }

    pub(super) async fn select_build(&self, authority: &BuildAuthority) -> Result<()> {
        self.backend.select_build(authority).await
    }

    pub(super) async fn describe_peer(&self, replica: ReplicaInformation) -> Result<()> {
        self.backend.describe_peer(replica).await
    }

    pub(super) async fn execute_build(
        &self,
        replica: ReplicaInformation,
    ) -> Result<Option<BuildAdmission>> {
        self.backend.execute_build(replica).await
    }

    pub(super) async fn accept_build(&self, receipt: Option<BuildAdmission>) -> Result<()> {
        self.backend.accept_build(receipt).await
    }

    pub(super) async fn enqueue_build(&self, endpoint: ReplicaEndpoint) -> Result<()> {
        self.backend.enqueue_build(endpoint).await
    }

    pub(super) async fn effect_evidence(
        &self,
        action: &RuntimeEffectAction,
    ) -> Option<RuntimeOperationEvidence> {
        self.backend.effect_evidence(action).await
    }

    pub(super) async fn confirm_build_completion(
        &self,
        build_id: &OperationId,
        target: &ReplicaIdentity,
    ) -> Result<Option<RuntimeOperationEvidence>> {
        self.backend
            .confirm_build_completion(build_id, target)
            .await
    }

    pub(super) async fn postcondition(&self) -> RuntimePostcondition {
        self.backend.postcondition().await
    }
}

pub(super) struct CustomReplicatorHost {
    host: Weak<RuntimeHost>,
    control: Arc<dyn Replicator>,
    primary: Arc<dyn PrimaryReplicator>,
    native_receipts: bool,
    abort_notified: std::sync::atomic::AtomicBool,
    gate: Mutex<()>,
    state: Arc<RwLock<RuntimeSnapshot>>,
    sessions: RwLock<BTreeMap<ReplicaIdentity, ProcessSessionId>>,
    addresses: RwLock<BTreeMap<ReplicaIdentity, (ProcessSessionId, String)>>,
    retired_sessions: RwLock<BTreeSet<(ReplicaIdentity, ProcessSessionId)>>,
    retired_builds: RwLock<BTreeSet<OperationId>>,
    build_generations: RwLock<BTreeMap<OperationId, u64>>,
    cancelling_builds: RwLock<BTreeMap<OperationId, u64>>,
    pending_builds: RwLock<BTreeMap<OperationId, ReplicaEndpoint>>,
    receipts: RwLock<BTreeMap<OperationId, BuildAdmission>>,
    configuration: Arc<RwLock<Option<ReplicaSetConfiguration>>>,
    deferred_configuration: Mutex<Option<DeferredConfiguration>>,
    deferred_configuration_abort: StdMutex<Option<tokio::task::AbortHandle>>,
    configuration_generation: Arc<AtomicU64>,
    configuration_commit: Arc<Mutex<()>>,
    access_generation: Arc<AtomicU64>,
    published_access_generation: Arc<AtomicU64>,
    access_commit: Arc<Mutex<()>>,
    restored_access: RwLock<Option<(AccessStatus, AccessStatus)>>,
    removal_witnesses:
        RwLock<BTreeMap<ReplicaIdentity, kuberic_protocol::types::SecondaryRemovalWitness>>,
    outbound: mpsc::Sender<OutboundOperation>,
    receiver: Mutex<mpsc::Receiver<OutboundOperation>>,
    changed: Notify,
}

impl CustomReplicatorHost {
    pub(super) fn new(
        host: Weak<RuntimeHost>,
        control: Arc<dyn Replicator>,
        primary: Arc<dyn PrimaryReplicator>,
        native_receipts: bool,
    ) -> Self {
        let identity = host
            .upgrade()
            .expect("registering host exists")
            .identity
            .clone();
        let (outbound, receiver) = mpsc::channel(16);
        let mut snapshot = empty_snapshot(identity);
        snapshot.live_builds_only = true;
        Self {
            host,
            control,
            primary,
            native_receipts,
            abort_notified: std::sync::atomic::AtomicBool::new(false),
            gate: Mutex::new(()),
            state: Arc::new(RwLock::new(snapshot)),
            sessions: RwLock::default(),
            addresses: RwLock::default(),
            retired_sessions: RwLock::default(),
            retired_builds: RwLock::default(),
            build_generations: RwLock::default(),
            cancelling_builds: RwLock::default(),
            pending_builds: RwLock::default(),
            receipts: RwLock::default(),
            configuration: Arc::new(RwLock::default()),
            deferred_configuration: Mutex::new(None),
            deferred_configuration_abort: StdMutex::new(None),
            configuration_generation: Arc::new(AtomicU64::new(0)),
            configuration_commit: Arc::new(Mutex::new(())),
            access_generation: Arc::new(AtomicU64::new(0)),
            published_access_generation: Arc::new(AtomicU64::new(0)),
            access_commit: Arc::new(Mutex::new(())),
            restored_access: RwLock::default(),
            removal_witnesses: RwLock::default(),
            outbound,
            receiver: Mutex::new(receiver),
            changed: Notify::new(),
        }
    }

    fn host(&self) -> Result<Arc<RuntimeHost>> {
        let host = self.host.upgrade().ok_or(RuntimeError::Closed)?;
        if self
            .abort_notified
            .load(std::sync::atomic::Ordering::Acquire)
            || host.aborted.load(std::sync::atomic::Ordering::Acquire)
        {
            return Err(RuntimeError::Closed);
        }
        Ok(host)
    }

    fn active_host(&self) -> Result<Arc<RuntimeHost>> {
        let host = self.host()?;
        if host.closed.load(std::sync::atomic::Ordering::Acquire) {
            return Err(RuntimeError::Closed);
        }
        Ok(host)
    }

    pub(super) async fn describe_peer(&self, replica: ReplicaInformation) -> Result<()> {
        self.apply_common_action(RuntimeEffectAction::RegisterPeerSession {
            identity: replica.identity.clone(),
            session: replica.process_session_id.clone(),
        })
        .await?;
        {
            let _gate = self.gate.lock().await;
            if self.sessions.read().await.get(&replica.identity)
                != Some(&replica.process_session_id)
            {
                return Err(RuntimeError::AuthorityNotAdmitted);
            }
            if replica.replication_address.is_empty() || replica.replication_address.len() > 512 {
                return Err(RuntimeError::InvalidReplication(
                    "invalid replica address".into(),
                ));
            }
            let value = (replica.process_session_id, replica.replication_address);
            if self.addresses.read().await.get(&replica.identity) == Some(&value) {
                return Ok(());
            }
            self.addresses.write().await.insert(replica.identity, value);
        }
        self.configure().await
    }

    pub(super) async fn select_build(&self, authority: &BuildAuthority) -> Result<()> {
        {
            let _gate = self.gate.lock().await;
            self.select_build_inner(authority).await?;
        }
        self.configure().await
    }

    async fn select_build_inner(&self, authority: &BuildAuthority) -> Result<()> {
        self.host()?
            .default_dependencies
            .build_authority_store
            .select_build(authority)
            .await?;
        self.state.write().await.builds.retain(|build| {
            build.authority.target.replica_id != authority.target.replica_id
                || build.authority == *authority
        });
        self.receipts.write().await.retain(|_, receipt| {
            receipt.selection.authority.target.replica_id != authority.target.replica_id
                || receipt.selection.authority == *authority
        });
        self.changed.notify_waiters();
        Ok(())
    }

    async fn receipt(&self, authority: &BuildAuthority) -> Result<BuildAdmission> {
        let host = self.host()?;
        let selection = host
            .default_dependencies
            .build_authority_store
            .load_build_selection(&authority.target)
            .await?
            .filter(|selection| selection.authority == *authority)
            .ok_or(RuntimeError::AuthorityNotAdmitted)?;
        let sessions = self.sessions.read().await;
        let local_session = &host
            .replica_session
            .get()
            .ok_or(RuntimeError::AuthorityNotAdmitted)?
            .1;
        let session = |identity: &ReplicaIdentity| {
            if identity == &host.identity {
                Some(local_session.clone())
            } else {
                sessions.get(identity).cloned()
            }
            .ok_or(RuntimeError::AuthorityNotAdmitted)
        };
        Ok(BuildAdmission {
            selection,
            source_session: session(&authority.source)?,
            target_session: session(&authority.target)?,
            attempt_generation: self
                .build_generations
                .read()
                .await
                .get(&authority.build_id)
                .copied()
                .unwrap_or_default(),
            configuration_generation: self.configuration_generation.load(Ordering::Acquire),
            native_token: None,
        })
    }

    async fn descriptions(&self) -> Result<Option<ReplicaSetConfiguration>> {
        let host = self.host()?;
        let builds = host
            .default_dependencies
            .build_authority_store
            .load_builds()
            .await?;
        let authority = self.state.read().await.authority.clone();
        let previous = authority
            .as_ref()
            .and_then(|a| a.previous_configuration.clone());
        let admitted = authority.map(|a| a.current_configuration);
        let incoming = builds
            .iter()
            .filter(|b| b.target == host.identity)
            .max_by_key(|b| b.current_configuration.epoch)
            .map(|b| b.current_configuration.clone());
        let configuration = match (admitted, incoming) {
            (Some(admitted), Some(incoming)) if incoming.epoch > admitted.epoch => Some(incoming),
            (Some(admitted), _) => Some(admitted),
            (None, incoming) => incoming,
        };
        let previous_configuration = self
            .published_configuration()
            .await
            .as_ref()
            .map(|c| c.configuration.clone());
        let configuration = configuration.or(previous_configuration);
        let Some(configuration) = configuration else {
            return Ok(None);
        };
        let mut sessions = self.sessions.read().await.clone();
        let local_session = match host.replica_session.get() {
            Some((_, session)) => session.clone(),
            None if !self.native_receipts => ProcessSessionId::default(),
            None => return Err(RuntimeError::AuthorityNotAdmitted),
        };
        sessions.insert(host.identity.clone(), local_session.clone());
        let address = self
            .state
            .read()
            .await
            .replication_address
            .clone()
            .unwrap_or_default();
        let addresses = self.addresses.read().await.clone();
        let mut members = configuration.members.clone();
        if let Some(previous) = previous {
            for member in previous.members {
                if !members
                    .iter()
                    .any(|current| current.identity == member.identity)
                {
                    members.push(member);
                }
            }
        }
        let mut replicas = members
            .iter()
            .map(|member| {
                let mut replica = ReplicaInformation::new(
                    OperationId::default(),
                    member.identity.clone(),
                    String::new(),
                );
                replica.role = member.role;
                replica.process_session_id =
                    sessions.get(&member.identity).cloned().unwrap_or_default();
                if member.identity == host.identity {
                    replica.replication_address = address.clone();
                } else if let Some((session, address)) = addresses.get(&member.identity)
                    && session == &replica.process_session_id
                {
                    replica.replication_address = address.clone();
                }
                replica
            })
            .collect::<Vec<_>>();
        for build in builds {
            if self.retired_builds.read().await.contains(&build.build_id)
                || build.current_configuration != configuration
            {
                continue;
            }
            if host
                .default_dependencies
                .build_authority_store
                .load_build_selection(&build.target)
                .await?
                .is_none_or(|selection| selection.authority != build)
            {
                continue;
            }
            if !replicas.iter().any(|r| r.identity == build.source) {
                continue;
            }
            let mut target =
                ReplicaInformation::new(build.build_id, build.target.clone(), String::new());
            target.process_session_id = sessions.get(&build.target).cloned().unwrap_or_default();
            target.current_progress = build.replication_boundary_lsn;
            target.catch_up_capability = build.replication_boundary_lsn;
            if build.target == host.identity {
                target.replication_address = address.clone();
            }
            replicas.push(target);
        }
        if !replicas.iter().any(|r| r.identity == host.identity) {
            let mut local =
                ReplicaInformation::new(OperationId::default(), host.identity.clone(), address);
            local.process_session_id = local_session;
            replicas.push(local);
        }
        Ok(Some(ReplicaSetConfiguration {
            configuration,
            replicas,
        }))
    }

    async fn published_configuration(&self) -> Option<ReplicaSetConfiguration> {
        let _commit = self.configuration_commit.lock().await;
        self.configuration.read().await.clone()
    }

    async fn configuration_update(
        &self,
    ) -> Result<Option<(ReplicaSetConfiguration, Option<ConfigurationDescriptor>)>> {
        let Some(current) = self.descriptions().await? else {
            return Ok(None);
        };
        let previous = self
            .state
            .read()
            .await
            .authority
            .as_ref()
            .and_then(|a| a.previous_configuration.clone());
        Ok(Some((current, previous)))
    }

    async fn apply_configuration_update(
        primary: Arc<dyn PrimaryReplicator>,
        current: ReplicaSetConfiguration,
        previous: Option<ConfigurationDescriptor>,
    ) -> Result<ReplicaSetConfiguration> {
        if let Some(previous) = previous {
            primary
                .update_catch_up_replica_set_configuration(
                    current.clone(),
                    ReplicaSetConfiguration {
                        replicas: current
                            .replicas
                            .iter()
                            .filter_map(|replica| {
                                let member = previous
                                    .members
                                    .iter()
                                    .find(|m| m.identity == replica.identity)?;
                                let mut replica = replica.clone();
                                replica.role = member.role;
                                Some(replica)
                            })
                            .collect(),
                        configuration: previous,
                    },
                )
                .await?;
        } else {
            primary
                .update_current_replica_set_configuration(current.clone())
                .await?;
        }
        Ok(current)
    }

    fn advance_configuration_generation(&self) -> Result<u64> {
        self.configuration_generation
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |generation| {
                generation.checked_add(1)
            })
            .map(|generation| generation + 1)
            .map_err(|_| RuntimeError::OperationCancelled)
    }

    fn advance_access_generation(&self) -> Result<u64> {
        self.access_generation
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |generation| {
                generation.checked_add(1)
            })
            .map(|generation| generation + 1)
            .map_err(|_| RuntimeError::OperationCancelled)
    }

    async fn invalidate_access_projections(&self) -> Result<()> {
        let _commit = self.access_commit.lock().await;
        self.advance_access_generation()?;
        Ok(())
    }

    fn ensure_configuration_generation(&self, generation: u64) -> Result<()> {
        self.active_host()?;
        if self.configuration_generation.load(Ordering::Acquire) != generation {
            return Err(RuntimeError::OperationCancelled);
        }
        Ok(())
    }

    async fn finish_deferred_configuration(&self) -> Result<()> {
        let Some(mut handle) = self.deferred_configuration.lock().await.take() else {
            return Ok(());
        };
        let (generation, _) =
            match tokio::time::timeout(std::time::Duration::from_secs(75), &mut handle).await {
                Ok(result) => result.map_err(|error| {
                    if error.is_cancelled() {
                        RuntimeError::OperationCancelled
                    } else {
                        RuntimeError::Application(error.to_string())
                    }
                })??,
                Err(_) => {
                    handle.abort();
                    return Err(RuntimeError::OperationCancelled);
                }
            };
        self.deferred_configuration_abort.lock().unwrap().take();
        self.ensure_configuration_generation(generation)?;
        Ok(())
    }

    async fn defer_configuration(&self) -> Result<()> {
        {
            let _commit = self.configuration_commit.lock().await;
            self.deferred_configuration_abort.lock().unwrap().take();
            if let Some(handle) = self.deferred_configuration.lock().await.take() {
                handle.abort();
                let _ = handle.await;
            }
        }
        let authority_before = self.state.read().await.authority.clone();
        let sessions_before = self.sessions.read().await.clone();
        let Some((current, previous)) = self.configuration_update().await? else {
            return Ok(());
        };
        let _commit = self.configuration_commit.lock().await;
        self.active_host()?;
        if authority_before != self.state.read().await.authority
            || sessions_before != *self.sessions.read().await
        {
            return Err(RuntimeError::OperationCancelled);
        }
        let generation = self.advance_configuration_generation()?;
        let configuration_generation = self.configuration_generation.clone();
        let configuration = self.configuration.clone();
        let configuration_commit = self.configuration_commit.clone();
        let host = self.host.clone();
        let primary = self.primary.clone();
        let handle = tokio::spawn(async move {
            let current = tokio::time::timeout(
                std::time::Duration::from_secs(75),
                Self::apply_configuration_update(primary, current, previous),
            )
            .await
            .map_err(|_| RuntimeError::OperationCancelled)??;
            if configuration_generation.load(Ordering::Acquire) != generation {
                return Err(RuntimeError::OperationCancelled);
            }
            let _commit = configuration_commit.lock().await;
            let active = host.upgrade().is_some_and(|host| {
                !host.aborted.load(std::sync::atomic::Ordering::Acquire)
                    && !host.closed.load(std::sync::atomic::Ordering::Acquire)
            });
            if !active || configuration_generation.load(Ordering::Acquire) != generation {
                return Err(RuntimeError::OperationCancelled);
            }
            *configuration.write().await = Some(current.clone());
            let still_active = host.upgrade().is_some_and(|host| {
                !host.aborted.load(std::sync::atomic::Ordering::Acquire)
                    && !host.closed.load(std::sync::atomic::Ordering::Acquire)
            });
            if !still_active || configuration_generation.load(Ordering::Acquire) != generation {
                *configuration.write().await = None;
                return Err(RuntimeError::OperationCancelled);
            }
            Ok((generation, current))
        });
        *self.deferred_configuration_abort.lock().unwrap() = Some(handle.abort_handle());
        *self.deferred_configuration.lock().await = Some(handle);
        Ok(())
    }

    async fn configure(&self) -> Result<()> {
        self.finish_deferred_configuration().await?;
        let Some((current, previous)) = self.configuration_update().await? else {
            return Ok(());
        };
        let generation = {
            let _commit = self.configuration_commit.lock().await;
            if self.configuration.read().await.as_ref() == Some(&current) {
                self.configuration_generation.load(Ordering::Acquire)
            } else {
                self.advance_configuration_generation()?
            }
        };
        let current =
            Self::apply_configuration_update(self.primary.clone(), current, previous).await?;
        let _commit = self.configuration_commit.lock().await;
        self.ensure_configuration_generation(generation)?;
        *self.configuration.write().await = Some(current);
        if let Err(error) = self.ensure_configuration_generation(generation) {
            *self.configuration.write().await = None;
            return Err(error);
        }
        Ok(())
    }

    async fn reserve_access_projection(
        &self,
        read: AccessStatus,
        write: AccessStatus,
    ) -> Result<AccessProjection> {
        *self.restored_access.write().await = None;
        let host = self.host()?;
        let faulted_grant = (read == AccessStatus::Granted || write == AccessStatus::Granted)
            && host.state.read().await.reported_fault.is_some();
        let (read, write) = if faulted_grant {
            (
                AccessStatus::ReconfigurationPending,
                AccessStatus::ReconfigurationPending,
            )
        } else {
            (read, write)
        };
        let authority_before = self.state.read().await.authority.clone();
        let configuration_before = self.published_configuration().await;
        let sessions_before = self.sessions.read().await.clone();
        let role_before = host.state.read().await.fallback_snapshot.role;
        let configuration_generation = self.configuration_generation.load(Ordering::Acquire);
        if write == AccessStatus::Granted {
            if role_before != kuberic_protocol::types::ReplicaRole::Primary {
                return Err(RuntimeError::NotPrimary);
            }
            let authority = authority_before
                .as_ref()
                .ok_or(RuntimeError::AuthorityNotAdmitted)?;
            if authority.primary_identity() != &host.identity {
                return Err(RuntimeError::NotPrimary);
            }
            let state = self.state.read().await;
            if authority
                .secondary_removal
                .as_ref()
                .is_some_and(|evidence| {
                    authority.previous_configuration.is_some()
                        || state
                            .accepted_secondary_removal
                            .as_ref()
                            .is_none_or(|committed| &committed.evidence != evidence)
                })
            {
                return Err(RuntimeError::ReconfigurationPending);
            }
        }
        let _commit = self.access_commit.lock().await;
        self.active_host()?;
        if authority_before != self.state.read().await.authority
            || configuration_before != self.published_configuration().await
            || sessions_before != *self.sessions.read().await
            || role_before != host.state.read().await.fallback_snapshot.role
            || configuration_generation != self.configuration_generation.load(Ordering::Acquire)
        {
            return Err(RuntimeError::OperationCancelled);
        }
        // Advancing the generation is the final operation before returning the
        // reservation. The caller installs rollback ownership immediately,
        // without an intervening await.
        let access_generation = self.advance_access_generation()?;
        Ok(AccessProjection {
            read,
            write,
            authority: authority_before,
            configuration: configuration_before,
            sessions: sessions_before,
            role: role_before,
            configuration_generation,
            access_generation,
            faulted_grant,
        })
    }

    async fn complete_access_projection(&self, projection: &AccessProjection) -> Result<()> {
        let progress = super::with_access_proof_view(
            projection.read,
            projection.write,
            self.control.current_progress(),
        )
        .await;
        if let Err(error) = progress {
            if !matches!(
                error,
                RuntimeError::ReconfigurationPending | RuntimeError::OperationCancelled
            ) {
                self.control.abort();
            }
            return Err(error);
        }
        self.active_host()?;
        self.validate_access_projection(projection).await?;
        Ok(())
    }

    async fn validate_access_projection(&self, projection: &AccessProjection) -> Result<()> {
        let host = self.active_host()?;
        if projection.authority != self.state.read().await.authority
            || projection.configuration != self.published_configuration().await
            || projection.sessions != *self.sessions.read().await
            || projection.role != host.state.read().await.fallback_snapshot.role
            || projection.configuration_generation
                != self.configuration_generation.load(Ordering::Acquire)
            || projection.access_generation != self.access_generation.load(Ordering::Acquire)
        {
            return Err(RuntimeError::OperationCancelled);
        }
        Ok(())
    }

    async fn publish_access_projection(&self, projection: AccessProjection) -> Result<()> {
        let _commit = self.access_commit.lock().await;
        self.validate_access_projection(&projection).await?;
        let host = self.active_host()?;
        let configuration = self.published_configuration().await;
        let sessions = self.sessions.read().await.clone();
        let mut state = self.state.write().await;
        let mut host_state = host.state.write().await;
        if projection.authority != state.authority
            || projection.configuration != configuration
            || projection.sessions != sessions
            || projection.role != host_state.fallback_snapshot.role
            || projection.configuration_generation
                != self.configuration_generation.load(Ordering::Acquire)
            || projection.access_generation != self.access_generation.load(Ordering::Acquire)
            || self
                .abort_notified
                .load(std::sync::atomic::Ordering::Acquire)
            || host.aborted.load(std::sync::atomic::Ordering::Acquire)
            || host.closed.load(std::sync::atomic::Ordering::Acquire)
        {
            return Err(RuntimeError::OperationCancelled);
        }
        // Both projection locks are held before either view changes. From this
        // point to commit there is no await, so cancellation cannot leave a
        // partially granted common/external projection.
        state.read_status = projection.read;
        state.write_status = projection.write;
        host_state.fallback_snapshot.read_status = projection.read;
        host_state.fallback_snapshot.write_status = projection.write;
        self.published_access_generation
            .store(projection.access_generation, Ordering::Release);
        if projection.faulted_grant {
            Err(RuntimeError::ReconfigurationPending)
        } else {
            Ok(())
        }
    }

    async fn set_access(&self, read: AccessStatus, write: AccessStatus) -> Result<()> {
        self.begin_common_access_effect(read, write).await?.commit();
        Ok(())
    }

    async fn begin_common_access_effect(
        &self,
        read: AccessStatus,
        write: AccessStatus,
    ) -> Result<AccessEffectCommit> {
        let projection = self.reserve_access_projection(read, write).await?;
        let mut rollback = AccessPublicationRollback::common(self, &projection);
        if let Err(error) = self.complete_access_projection(&projection).await {
            self.rollback_common_access_projection(&projection).await;
            rollback.disarm();
            return Err(error);
        }
        if let Err(error) = self.publish_access_projection(projection.clone()).await {
            self.rollback_common_access_projection(&projection).await;
            rollback.disarm();
            return Err(error);
        }
        Ok(AccessEffectCommit::managed(rollback))
    }

    async fn rollback_common_access_projection(&self, projection: &AccessProjection) {
        let _commit = self.access_commit.lock().await;
        if self.access_generation.load(Ordering::Acquire) != projection.access_generation {
            return;
        }
        self.access_generation.store(
            projection.access_generation.saturating_add(1),
            Ordering::Release,
        );
        let mut state = self.state.write().await;
        state.read_status = AccessStatus::ReconfigurationPending;
        state.write_status = AccessStatus::ReconfigurationPending;
        drop(state);
        if let Some(host) = self.host.upgrade() {
            let mut state = host.state.write().await;
            state.fallback_snapshot.read_status = AccessStatus::ReconfigurationPending;
            state.fallback_snapshot.write_status = AccessStatus::ReconfigurationPending;
        }
    }

    async fn accept_catch_up_receipt(&self, receipt: &CatchUpReceipt) -> Result<()> {
        let mut state = self.state.write().await;
        if state.authority.as_ref() != Some(&receipt.authority) {
            return Err(RuntimeError::OperationCancelled);
        }
        state.current_progress = state.current_progress.max(receipt.current_progress);
        state.committed_lsn = state.committed_lsn.max(receipt.committed_lsn);
        state.current_configuration_quorum_progress = receipt.current_configuration_quorum_progress;
        state.catch_up_boundary = Some(receipt.boundary_lsn);
        state.catch_up_complete = true;
        Ok(())
    }

    async fn accept_certified_prefix(&self, receipt: &CertifiedPrefixReceipt) -> Result<()> {
        let mut state = self.state.write().await;
        if state.authority != receipt.token.authority {
            return Err(RuntimeError::OperationCancelled);
        }
        state.current_progress = state.current_progress.max(receipt.verified_lsn);
        state.verified_replication_lsn = Some(
            state
                .verified_replication_lsn
                .map_or(receipt.verified_lsn, |verified| {
                    verified.max(receipt.verified_lsn)
                }),
        );
        state.committed_lsn = state.committed_lsn.max(receipt.committed_lsn);
        Ok(())
    }

    async fn accept_switchover_receipt(&self, receipt: &SwitchoverReceipt) -> Result<()> {
        let mut state = self.state.write().await;
        if state.authority != receipt.token.authority {
            return Err(RuntimeError::OperationCancelled);
        }
        state.current_progress = state.current_progress.max(receipt.handoff_lsn);
        state.committed_lsn = state.committed_lsn.max(receipt.committed_lsn);
        Ok(())
    }

    async fn accept_secondary_removal_receipt(
        &self,
        receipt: &SecondaryRemovalReceipt,
    ) -> Result<()> {
        let mut state = self.state.write().await;
        if state.authority != receipt.token.authority {
            return Err(RuntimeError::OperationCancelled);
        }
        if let Some(preparation) = &receipt.preparation {
            state.prepared_secondary_removal = Some(preparation.clone());
            state.current_progress = state.current_progress.max(preparation.boundary_lsn);
        }
        if let Some(accepted) = &receipt.accepted {
            state.accepted_secondary_removal = Some(accepted.clone());
            state.current_progress = state
                .current_progress
                .max(accepted.evidence.preparation.boundary_lsn);
        }
        if let Some(verified_lsn) = receipt.verified_lsn {
            state.verified_replication_lsn = Some(
                state
                    .verified_replication_lsn
                    .map_or(verified_lsn, |verified| verified.max(verified_lsn)),
            );
            state.current_progress = state.current_progress.max(verified_lsn);
        }
        state.committed_lsn = state.committed_lsn.max(receipt.committed_lsn);
        Ok(())
    }

    async fn accept_managed_build(&self, receipt: &NativeBuildReceipt) -> Result<()> {
        let host = self.active_host()?;
        if host
            .default_dependencies
            .build_authority_store
            .load_build_selection(&receipt.selection.authority.target)
            .await?
            .as_ref()
            != Some(&receipt.selection)
        {
            return Err(RuntimeError::OperationCancelled);
        }
        let mut state = self.state.write().await;
        state
            .builds
            .retain(|build| build.authority.build_id != receipt.progress.authority.build_id);
        state.builds.push(BuildPostcondition {
            authority: receipt.progress.authority.clone(),
            last_sequence: receipt.progress.last_sequence,
            durable_lsn: receipt.progress.durable_lsn,
            completed: true,
            catch_up_boundary_lsn: receipt.progress.catch_up_boundary_lsn,
        });
        Ok(())
    }

    async fn record_completion(&self, receipt: BuildAdmission) -> Result<()> {
        let authority = receipt.selection.authority.clone();
        let host = self.host()?;
        if self.receipt(&authority).await? != receipt {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        if self
            .retired_builds
            .read()
            .await
            .contains(&authority.build_id)
            || host
                .default_dependencies
                .build_authority_store
                .load_build(&authority.build_id)
                .await?
                .as_ref()
                != Some(&authority)
        {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        if authority.source == host.identity
            && self
                .state
                .read()
                .await
                .authority
                .as_ref()
                .is_none_or(|a| a.current_configuration != authority.current_configuration)
        {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        let existing = host
            .default_dependencies
            .build_progress_store
            .load_build_progress(&authority.build_id)
            .await?;
        let progress = DurableBuildProgress {
            last_sequence: existing
                .as_ref()
                .map_or(Some(1), |p| p.last_sequence.checked_add(1))
                .ok_or_else(|| {
                    RuntimeError::InvalidReplication("build sequence exhausted".into())
                })?,
            durable_lsn: authority.replication_boundary_lsn,
            catch_up_boundary_lsn: Some(authority.replication_boundary_lsn),
            completed: true,
            authority,
        };
        host.default_dependencies
            .build_progress_store
            .record_selected_build_progress(&receipt.selection, &progress)
            .await?;
        self.receipts
            .write()
            .await
            .insert(progress.authority.build_id.clone(), receipt);
        let mut state = self.state.write().await;
        state
            .builds
            .retain(|b| b.authority.build_id != progress.authority.build_id);
        state.builds.push(BuildPostcondition {
            authority: progress.authority,
            last_sequence: progress.last_sequence,
            durable_lsn: progress.durable_lsn,
            completed: true,
            catch_up_boundary_lsn: progress.catch_up_boundary_lsn,
        });
        Ok(())
    }

    async fn record_build_start(&self, receipt: &BuildAdmission) -> Result<()> {
        let authority = receipt.selection.authority.clone();
        let host = self.host()?;
        if self.receipt(&authority).await? != *receipt {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        let existing = host
            .default_dependencies
            .build_progress_store
            .load_build_progress(&authority.build_id)
            .await?;
        let progress = match existing {
            Some(progress) if progress.authority == authority => progress,
            Some(_) => {
                return Err(RuntimeError::AuthorityMismatch(
                    "build progress belongs to different authority".into(),
                ));
            }
            None => {
                let progress = DurableBuildProgress {
                    authority,
                    last_sequence: 0,
                    durable_lsn: receipt.selection.authority.replication_boundary_lsn,
                    completed: false,
                    catch_up_boundary_lsn: None,
                };
                host.default_dependencies
                    .build_progress_store
                    .record_selected_build_progress(&receipt.selection, &progress)
                    .await?;
                progress
            }
        };
        let mut state = self.state.write().await;
        state
            .builds
            .retain(|build| build.authority.build_id != progress.authority.build_id);
        state.builds.push(BuildPostcondition {
            authority: progress.authority,
            last_sequence: progress.last_sequence,
            durable_lsn: progress.durable_lsn,
            completed: progress.completed,
            catch_up_boundary_lsn: progress.catch_up_boundary_lsn,
        });
        drop(state);
        self.changed.notify_waiters();
        Ok(())
    }

    async fn refresh(&self) -> Result<()> {
        let host = self.host()?;
        if self.native_receipts
            && host
                .default_dependencies
                .build_authority_store
                .load_build_selection(&host.identity)
                .await?
                .is_some()
        {
            self.configure().await?;
        }
        let restored = *self.restored_access.read().await;
        if let Some((read, write)) = restored {
            self.try_restore_access(read, write).await?;
        }
        let incoming = match host
            .default_dependencies
            .build_authority_store
            .load_build_selection(&host.identity)
            .await?
        {
            Some(selection) => self.receipt(&selection.authority).await.ok(),
            None => None,
        };
        let authority_before = self.state.read().await.authority.clone();
        let sessions_before = self.sessions.read().await.clone();
        let configuration_generation = self.configuration_generation.load(Ordering::Acquire);
        let progress = match self.control.current_progress().await {
            Ok(progress) => progress,
            Err(RuntimeError::ReconfigurationPending) => {
                let mut state = self.state.write().await;
                state.read_status = AccessStatus::ReconfigurationPending;
                state.write_status = AccessStatus::ReconfigurationPending;
                drop(state);
                let mut state = host.state.write().await;
                state.fallback_snapshot.read_status = AccessStatus::ReconfigurationPending;
                state.fallback_snapshot.write_status = AccessStatus::ReconfigurationPending;
                return Err(RuntimeError::ReconfigurationPending);
            }
            Err(error) => return Err(error),
        };
        self.active_host()?;
        if authority_before != self.state.read().await.authority
            || sessions_before != *self.sessions.read().await
            || configuration_generation != self.configuration_generation.load(Ordering::Acquire)
        {
            return Err(RuntimeError::OperationCancelled);
        }
        if progress < 0 {
            return Err(RuntimeError::InvalidReplication(
                "negative custom replicator progress".into(),
            ));
        }
        let described = self.published_configuration().await;
        if let Some(receipt) = incoming {
            let build = &receipt.selection.authority;
            let exact_description = described.as_ref().is_some_and(|c| {
                c.configuration == build.current_configuration
                    && c.replicas
                        .iter()
                        .filter(|r| r.identity == host.identity && !r.build_id.is_empty())
                        .count()
                        == 1
                    && c.replicas.iter().any(|r| {
                        r.build_id == build.build_id
                            && r.identity == build.target
                            && r.process_session_id == receipt.target_session
                            && r.current_progress == build.replication_boundary_lsn
                    })
            });
            if exact_description
                && !self.retired_builds.read().await.contains(&build.build_id)
                && progress > 0
                && progress >= build.replication_boundary_lsn
                && self
                    .receipts
                    .read()
                    .await
                    .get(&build.build_id)
                    .is_none_or(|stored| !stored.matches_durable_selection(&receipt))
            {
                self.record_completion(receipt).await?;
            }
        }
        let mut state = self.state.write().await;
        state.current_progress = progress;
        state.committed_lsn = progress;
        state.current_configuration_quorum_progress = progress;
        state.verified_replication_lsn = state.authority.as_ref().map(|_| progress);
        Ok(())
    }

    pub(super) async fn enqueue_build(&self, endpoint: ReplicaEndpoint) -> Result<()> {
        if !self.begin_build_attempt(&endpoint).await? {
            return Ok(());
        }
        let build_id = endpoint.build_id.clone();
        if let Err(error) = self.enqueue_outbound(OutboundOperation::Build(endpoint)) {
            self.pending_builds.write().await.remove(&build_id);
            return Err(error);
        }
        Ok(())
    }

    async fn enqueue_build_wait(&self, endpoint: ReplicaEndpoint) -> Result<()> {
        if !self.begin_build_attempt(&endpoint).await? {
            return Ok(());
        }
        let build_id = endpoint.build_id.clone();
        let endpoint_for_cleanup = endpoint.clone();
        let result = self.enqueue_build_wait_inner(endpoint).await;
        if result.is_err()
            && self.pending_builds.read().await.get(&build_id) == Some(&endpoint_for_cleanup)
        {
            self.pending_builds.write().await.remove(&build_id);
        }
        result
    }

    async fn begin_build_attempt(&self, endpoint: &ReplicaEndpoint) -> Result<bool> {
        let mut pending = self.pending_builds.write().await;
        if let Some(existing) = pending.get(&endpoint.build_id) {
            if existing == endpoint {
                return Ok(false);
            }
            return Err(RuntimeError::AuthorityMismatch(
                "build ID is already queued for another exact target".into(),
            ));
        }
        let mut generations = self.build_generations.write().await;
        let generation = generations.entry(endpoint.build_id.clone()).or_default();
        *generation = generation
            .checked_add(1)
            .ok_or(RuntimeError::OperationCancelled)?;
        pending.insert(endpoint.build_id.clone(), endpoint.clone());
        Ok(true)
    }

    async fn enqueue_build_wait_inner(&self, endpoint: ReplicaEndpoint) -> Result<()> {
        let mut operation = OutboundOperation::Build(endpoint);
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(75);
        loop {
            match self.outbound.try_send(operation) {
                Ok(()) => return Ok(()),
                Err(mpsc::error::TrySendError::Full(returned)) => {
                    operation = returned;
                }
                Err(mpsc::error::TrySendError::Closed(_)) => {
                    return Err(RuntimeError::Closed);
                }
            }
            self.host()?;
            if tokio::time::Instant::now() >= deadline {
                return Err(RuntimeError::QueueFull);
            }
            let changed = self.changed.notified();
            tokio::select! {
                _ = changed => {}
                _ = tokio::time::sleep(std::time::Duration::from_millis(10)) => {}
            }
        }
    }

    fn enqueue_outbound(&self, operation: OutboundOperation) -> Result<()> {
        self.outbound
            .try_send(operation)
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => RuntimeError::QueueFull,
                mpsc::error::TrySendError::Closed(_) => RuntimeError::Closed,
            })
    }

    async fn prepare_build_description(
        &self,
        replica: &mut ReplicaInformation,
    ) -> Result<BuildAuthority> {
        let host = self.host()?;
        let _gate = self.gate.lock().await;
        let authority = host
            .default_dependencies
            .build_authority_store
            .load_build(&replica.build_id)
            .await?
            .ok_or(RuntimeError::AuthorityNotAdmitted)?;
        if authority.source != host.identity || authority.target != replica.identity {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        if self
            .retired_builds
            .read()
            .await
            .contains(&authority.build_id)
            || self
                .state
                .read()
                .await
                .authority
                .as_ref()
                .is_none_or(|a| a.current_configuration != authority.current_configuration)
        {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        let sessions = self.sessions.read().await.clone();
        replica.process_session_id = sessions
            .get(&replica.identity)
            .cloned()
            .ok_or(RuntimeError::AuthorityNotAdmitted)?;
        replica.current_progress = authority.replication_boundary_lsn;
        replica.catch_up_capability = authority.replication_boundary_lsn;
        self.configure().await?;
        Ok(authority)
    }

    async fn prepare_build(&self, replica: &mut ReplicaInformation) -> Result<BuildAdmission> {
        let authority = self.prepare_build_description(replica).await?;
        self.receipt(&authority).await
    }

    pub(super) async fn execute_build(&self, mut replica: ReplicaInformation) -> Result<()> {
        let receipt = self.prepare_build(&mut replica).await?;
        self.record_build_start(&receipt).await?;
        self.primary.build_replica(replica).await?;
        self.active_host()?;
        let _gate = self.gate.lock().await;
        self.record_completion(receipt.clone()).await?;
        self.pending_builds
            .write()
            .await
            .remove(&receipt.selection.authority.build_id);
        self.refresh().await
    }

    async fn complete_open_common(&self, address: String) {
        let mut state = self.state.write().await;
        state.open = true;
        state.replication_address = Some(address);
    }

    async fn terminate(&self) -> Result<()> {
        *self.restored_access.write().await = None;
        self.removal_witnesses.write().await.clear();
        self.invalidate_build_attempts().await?;
        self.receiver.lock().await.close();
        let mut state = self.state.write().await;
        state.open = false;
        state.role = kuberic_protocol::types::ReplicaRole::None;
        state.role_transition = None;
        state.read_status = AccessStatus::NotPrimary;
        state.write_status = AccessStatus::NotPrimary;
        state.builds.clear();
        drop(state);
        self.changed.notify_waiters();
        Ok(())
    }

    async fn mark_not_open(&self) {
        self.state.write().await.open = false;
        if let Some(host) = self.host.upgrade() {
            host.state.write().await.fallback_snapshot.open = false;
        }
    }

    async fn invalidate_configuration_attempts(&self) -> Result<()> {
        self.invalidate_access_projections().await?;
        let _commit = self.configuration_commit.lock().await;
        self.advance_configuration_generation()?;
        self.deferred_configuration_abort.lock().unwrap().take();
        if let Some(handle) = self.deferred_configuration.lock().await.take() {
            handle.abort();
            let _ = handle.await;
        }
        Ok(())
    }

    async fn invalidate_build_attempts(&self) -> Result<()> {
        self.invalidate_configuration_attempts().await?;
        for generation in self.build_generations.write().await.values_mut() {
            *generation = generation
                .checked_add(1)
                .ok_or(RuntimeError::OperationCancelled)?;
        }
        self.pending_builds.write().await.clear();
        self.changed.notify_waiters();
        Ok(())
    }

    async fn prepare_authority_admission(&self, authority: &AdmittedAuthority) -> Result<()> {
        let previous = self.state.read().await.authority.clone();
        if previous.as_ref() != Some(authority) {
            self.invalidate_configuration_attempts().await?;
        }
        let preserve_access = previous.as_ref().is_some_and(|existing| {
            existing == authority || preserves_same_primary_scale_up_access(existing, authority)
        });
        if preserve_access {
            return Ok(());
        }
        if self.native_receipts {
            return self.fence_writes().await;
        }
        self.fence_managed_access().await
    }

    async fn fence_managed_access(&self) -> Result<()> {
        *self.restored_access.write().await = None;
        self.removal_witnesses.write().await.clear();
        self.invalidate_build_attempts().await?;
        self.set_access(
            AccessStatus::ReconfigurationPending,
            AccessStatus::ReconfigurationPending,
        )
        .await
    }

    async fn restore_engine_proof(&self, snapshot: RuntimeSnapshot) -> Result<()> {
        let mut state = self.state.write().await;
        state.committed_lsn = snapshot.committed_lsn;
        state.current_progress = snapshot.current_progress;
        state.verified_replication_lsn = snapshot.verified_replication_lsn;
        state.current_configuration_quorum_progress =
            snapshot.current_configuration_quorum_progress;
        state.catch_up_boundary = snapshot.catch_up_boundary;
        state.catch_up_complete = snapshot.catch_up_complete;
        merge_builds(&mut state.builds, snapshot.builds);
        state.prepared_secondary_removal = snapshot.prepared_secondary_removal;
        state.accepted_secondary_removal = snapshot.accepted_secondary_removal;
        state.retired_authority = snapshot.retired_authority;
        Ok(())
    }

    async fn restore_builds(&self) -> Result<()> {
        let host = self.host()?;
        let authorities = host
            .default_dependencies
            .build_authority_store
            .load_builds()
            .await?;
        let mut builds = Vec::with_capacity(authorities.len());
        for authority in authorities
            .into_iter()
            .filter(|authority| authority.target == host.identity)
        {
            let progress = host
                .default_dependencies
                .build_progress_store
                .load_build_progress(&authority.build_id)
                .await?;
            let build = match progress {
                Some(progress) if progress.authority == authority => BuildPostcondition {
                    authority,
                    last_sequence: progress.last_sequence,
                    durable_lsn: progress.durable_lsn,
                    completed: progress.completed,
                    catch_up_boundary_lsn: progress.catch_up_boundary_lsn,
                },
                Some(_) => {
                    return Err(RuntimeError::AuthorityMismatch(
                        "build progress belongs to different authority".into(),
                    ));
                }
                None => BuildPostcondition {
                    authority,
                    last_sequence: 0,
                    durable_lsn: 0,
                    completed: false,
                    catch_up_boundary_lsn: None,
                },
            };
            builds.push(build);
        }
        self.state.write().await.builds = builds;
        Ok(())
    }

    async fn install_managed_authority(&self, authority: &AdmittedAuthority) -> Result<()> {
        let _gate = self.gate.lock().await;
        authority.validate()?;
        let previous = self.state.read().await.authority.clone();
        self.state.write().await.authority = Some(authority.clone());
        if let Some(previous) = previous {
            let retained = authority
                .current_configuration
                .members
                .iter()
                .chain(
                    authority
                        .previous_configuration
                        .iter()
                        .flat_map(|configuration| &configuration.members),
                )
                .map(|member| member.identity.clone())
                .collect::<BTreeSet<_>>();
            for identity in previous
                .current_configuration
                .members
                .iter()
                .chain(
                    previous
                        .previous_configuration
                        .iter()
                        .flat_map(|configuration| &configuration.members),
                )
                .map(|member| member.identity.clone())
                .filter(|identity| !retained.contains(identity))
            {
                self.enqueue_outbound(OutboundOperation::Evict(identity))?;
            }
        }
        self.configure().await
    }

    async fn retire_managed_build(&self, build_id: OperationId) -> Result<()> {
        let host = self.host()?;
        let build = host
            .default_dependencies
            .build_authority_store
            .load_build(&build_id)
            .await?;
        self.execute_common_build_action(RuntimeEffectAction::RetireBuild(build_id))
            .await?;
        if let Some(build) = build {
            self.enqueue_outbound(OutboundOperation::Remove(build.target.replica_id))?;
        }
        Ok(())
    }

    async fn remove_managed_replica(
        &self,
        replica_id: kuberic_protocol::types::ReplicaId,
    ) -> Result<()> {
        let retired = self
            .state
            .read()
            .await
            .builds
            .iter()
            .filter(|build| build.authority.target.replica_id == replica_id)
            .map(|build| build.authority.build_id.clone())
            .collect::<Vec<_>>();
        {
            let mut generations = self.build_generations.write().await;
            for build_id in &retired {
                let generation = generations.entry(build_id.clone()).or_default();
                *generation = generation
                    .checked_add(1)
                    .ok_or(RuntimeError::OperationCancelled)?;
            }
        }
        self.state
            .write()
            .await
            .builds
            .retain(|build| build.authority.target.replica_id != replica_id);
        self.receipts
            .write()
            .await
            .retain(|_, receipt| receipt.selection.authority.target.replica_id != replica_id);
        self.pending_builds
            .write()
            .await
            .retain(|_, endpoint| endpoint.identity.replica_id != replica_id);
        self.retired_builds.write().await.extend(retired);
        self.changed.notify_waiters();
        self.enqueue_outbound(OutboundOperation::Remove(replica_id))
    }

    async fn install_managed_build(&self, authority: &BuildAuthority) -> Result<()> {
        let _gate = self.gate.lock().await;
        authority.validate()?;
        let host = self.host()?;
        if self
            .retired_builds
            .read()
            .await
            .contains(&authority.build_id)
        {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        self.select_build_inner(authority).await?;
        self.restore_builds().await?;
        let completed = self.state.read().await.builds.iter().any(|build| {
            build.authority == *authority
                && build.completed
                && build.durable_lsn >= authority.replication_boundary_lsn
        });
        if authority.target == host.identity && !completed {
            let read = self.state.read().await.read_status;
            self.set_access(read, AccessStatus::ReconfigurationPending)
                .await?;
        }
        if self.state.read().await.authority.is_some() {
            self.configure().await?;
        }
        Ok(())
    }

    async fn admit_common_build(&self, authority: BuildAuthority) -> Result<()> {
        let (configure, defer) = {
            let _gate = self.gate.lock().await;
            authority.validate()?;
            let host = self.host()?;
            if self
                .retired_builds
                .read()
                .await
                .contains(&authority.build_id)
            {
                return Err(RuntimeError::AuthorityNotAdmitted);
            }
            let superseding = host
                .default_dependencies
                .build_authority_store
                .load_build_selection(&authority.target)
                .await?
                .is_some_and(|selection| selection.authority != authority);
            host.default_dependencies
                .build_authority_store
                .admit_build(&authority)
                .await?;
            self.select_build_inner(&authority).await?;
            let completed = self.state.read().await.builds.iter().any(|build| {
                build.authority == authority
                    && build.completed
                    && build.durable_lsn >= authority.replication_boundary_lsn
            });
            if authority.target == host.identity && !completed {
                let read = self.state.read().await.read_status;
                if self.native_receipts && superseding {
                    let mut state = self.state.write().await;
                    state.read_status = read;
                    state.write_status = AccessStatus::ReconfigurationPending;
                    drop(state);
                    let mut state = host.state.write().await;
                    state.fallback_snapshot.read_status = read;
                    state.fallback_snapshot.write_status = AccessStatus::ReconfigurationPending;
                } else {
                    self.set_access(read, AccessStatus::ReconfigurationPending)
                        .await?;
                }
            }
            let configure = self.native_receipts || self.state.read().await.authority.is_some();
            (
                configure && !(self.native_receipts && superseding),
                configure && self.native_receipts && superseding,
            )
        };
        if defer {
            self.defer_configuration().await?;
        } else if configure {
            self.configure().await?;
        }
        Ok(())
    }

    async fn fence_retirement_state(
        &self,
        retired: &kuberic_runtime_internal::authority::RetiredAuthority,
    ) -> Result<()> {
        let host = self.host()?;
        retired.validate(&host.identity)?;
        self.invalidate_configuration_attempts().await?;
        *self.restored_access.write().await = None;
        self.removal_witnesses.write().await.clear();
        {
            let mut state = self.state.write().await;
            state.authority = None;
            state.verified_replication_lsn = None;
            state.read_status = AccessStatus::NotPrimary;
            state.write_status = AccessStatus::NotPrimary;
            state.builds.clear();
        }
        let mut state = host.state.write().await;
        state.fallback_snapshot.authority = None;
        state.fallback_snapshot.read_status = AccessStatus::NotPrimary;
        state.fallback_snapshot.write_status = AccessStatus::NotPrimary;
        Ok(())
    }

    async fn complete_retirement_state(
        &self,
        retired: &kuberic_runtime_internal::authority::RetiredAuthority,
    ) -> Result<()> {
        let host = self.host()?;
        retired.validate(&host.identity)?;
        let closed = host.state.read().await.fallback_snapshot.clone();
        if closed.open || closed.role != kuberic_protocol::types::ReplicaRole::None {
            return Err(RuntimeError::ReconfigurationPending);
        }
        self.terminate().await?;
        let mut state = self.state.write().await;
        state.retired_authority = Some(retired.clone());
        state.authority = None;
        drop(state);
        host.state.write().await.fallback_snapshot.authority = None;
        Ok(())
    }

    async fn execute_common_build_action(&self, action: RuntimeEffectAction) -> Result<()> {
        let _gate = self.gate.lock().await;
        let host = self.host()?;
        match action {
            RuntimeEffectAction::AdmitBuildAuthority(authority) => {
                authority.validate()?;
                if self
                    .retired_builds
                    .read()
                    .await
                    .contains(&authority.build_id)
                {
                    return Err(RuntimeError::AuthorityNotAdmitted);
                }
                host.default_dependencies
                    .build_authority_store
                    .admit_build(&authority)
                    .await?;
                self.select_build_inner(&authority).await?;
                Ok(())
            }
            RuntimeEffectAction::RegisterPeerSession { identity, session } => {
                if session.is_empty()
                    || self
                        .retired_sessions
                        .read()
                        .await
                        .contains(&(identity.clone(), session.clone()))
                {
                    return Err(RuntimeError::AuthorityNotAdmitted);
                }
                let old = self
                    .sessions
                    .write()
                    .await
                    .insert(identity.clone(), session.clone());
                if let Some(old) = old.filter(|old| old != &session) {
                    self.retired_sessions
                        .write()
                        .await
                        .insert((identity.clone(), old));
                    self.state.write().await.builds.retain(|b| {
                        b.authority.source != identity && b.authority.target != identity
                    });
                    self.invalidate_build_attempts().await?;
                }
                if self.native_receipts {
                    self.configure().await?;
                }
                Ok(())
            }
            RuntimeEffectAction::RetireBuild(id) => {
                self.retired_builds.write().await.insert(id.clone());
                self.pending_builds.write().await.remove(&id);
                self.state
                    .write()
                    .await
                    .builds
                    .retain(|b| b.authority.build_id != id);
                self.changed.notify_waiters();
                Ok(())
            }
            _ => Err(RuntimeError::Application(
                "action is not common build/session work".into(),
            )),
        }
    }

    async fn cancel_common_build(&self, id: &OperationId) -> Result<()> {
        let mut generations = self.build_generations.write().await;
        let generation = generations.entry(id.clone()).or_default();
        *generation = generation
            .checked_add(1)
            .ok_or(RuntimeError::OperationCancelled)?;
        drop(generations);
        self.pending_builds.write().await.remove(id);
        self.state
            .write()
            .await
            .builds
            .retain(|b| &b.authority.build_id != id);
        self.changed.notify_waiters();
        self.cancelling_builds.write().await.remove(id);
        Ok(())
    }

    async fn cancel_common_build_attempt(
        &self,
        id: &OperationId,
        expected_generation: u64,
    ) -> Result<bool> {
        if self.cancelling_builds.read().await.get(id) == Some(&expected_generation) {
            return Ok(true);
        }
        let mut generations = self.build_generations.write().await;
        let generation = generations.entry(id.clone()).or_default();
        if *generation != expected_generation {
            return Ok(false);
        }
        *generation = generation
            .checked_add(1)
            .ok_or(RuntimeError::OperationCancelled)?;
        self.cancelling_builds
            .write()
            .await
            .insert(id.clone(), expected_generation);
        drop(generations);
        self.pending_builds.write().await.remove(id);
        self.state
            .write()
            .await
            .builds
            .retain(|build| &build.authority.build_id != id);
        self.changed.notify_waiters();
        Ok(true)
    }

    async fn complete_common_build_cancellation(&self, id: &OperationId, expected_generation: u64) {
        let mut cancelling = self.cancelling_builds.write().await;
        if cancelling.get(id) == Some(&expected_generation) {
            cancelling.remove(id);
        }
    }

    async fn build_generation(&self, id: &OperationId) -> u64 {
        self.build_generations
            .read()
            .await
            .get(id)
            .copied()
            .unwrap_or_default()
    }

    async fn ensure_build_generation(&self, id: &OperationId, generation: u64) -> Result<()> {
        self.active_host()?;
        if self.build_generation(id).await != generation
            || self.retired_builds.read().await.contains(id)
        {
            return Err(RuntimeError::OperationCancelled);
        }
        Ok(())
    }
}

impl CustomReplicatorHost {
    pub(super) async fn restore_access(
        &self,
        read: AccessStatus,
        write: AccessStatus,
    ) -> Result<()> {
        let _gate = self.gate.lock().await;
        self.try_restore_access(read, write).await
    }

    async fn try_restore_access(&self, read: AccessStatus, write: AccessStatus) -> Result<()> {
        let result = self.set_access(read, write).await;
        if matches!(result, Err(RuntimeError::ReconfigurationPending)) {
            // Retain the durable intent, not a successful effect receipt. Progress
            // reconciliation retries it until discovery/application readiness converge.
            *self.restored_access.write().await = Some((read, write));
        }
        result
    }

    async fn retry_restored_access(&self) -> Result<()> {
        let restored = *self.restored_access.read().await;
        if let Some((read, write)) = restored {
            self.try_restore_access(read, write).await?;
        }
        Ok(())
    }

    pub(super) async fn complete_open(&self, address: String) -> Result<()> {
        self.complete_open_common(address).await;
        Ok(())
    }
    pub(super) async fn fence_writes(&self) -> Result<()> {
        *self.restored_access.write().await = None;
        self.removal_witnesses.write().await.clear();
        let configuration = self.published_configuration().await;
        if let Some(configuration) = configuration {
            self.invalidate_build_attempts().await?;
            self.control
                .update_epoch(configuration.configuration.epoch)
                .await?;
        }
        self.set_access(
            AccessStatus::ReconfigurationPending,
            AccessStatus::ReconfigurationPending,
        )
        .await
    }
    pub(super) async fn settle_primary_prefix(&self) -> Result<()> {
        self.refresh().await
    }
    pub(super) async fn cancel_configuration_work(&self) -> Result<()> {
        let _gate = self.gate.lock().await;
        *self.restored_access.write().await = None;
        self.removal_witnesses.write().await.clear();
        self.invalidate_build_attempts().await
    }
    pub(super) async fn restore_authority(&self) -> Result<()> {
        let _gate = self.gate.lock().await;
        self.state.write().await.authority = self
            .host()?
            .default_dependencies
            .replica_authority_store
            .load()
            .await?;
        self.state.write().await.prepared_secondary_removal = self
            .host()?
            .default_dependencies
            .replica_authority_store
            .load_secondary_removal()
            .await?;
        self.restore_builds().await?;
        if self.native_receipts || self.state.read().await.authority.is_some() {
            self.configure().await?;
        }
        self.refresh().await
    }

    async fn wait_for_common_catchup(&self) -> Result<()> {
        let (authority_before, sessions_before, configuration_generation) = {
            let _gate = self.gate.lock().await;
            (
                self.state.read().await.authority.clone(),
                self.sessions.read().await.clone(),
                self.configuration_generation.load(Ordering::Acquire),
            )
        };
        self.primary
            .wait_for_catch_up_quorum(ReplicaSetQuorumMode::WriteQuorum)
            .await?;
        let boundary = self.control.current_progress().await?;
        let _gate = self.gate.lock().await;
        self.active_host()?;
        if authority_before != self.state.read().await.authority
            || sessions_before != *self.sessions.read().await
            || configuration_generation != self.configuration_generation.load(Ordering::Acquire)
        {
            return Err(RuntimeError::OperationCancelled);
        }
        let mut state = self.state.write().await;
        state.catch_up_boundary = Some(boundary);
        state.catch_up_complete = true;
        Ok(())
    }

    async fn authorize_common_failover_prefix(&self, boundary: i64) -> Result<()> {
        let (authority_before, sessions_before, configuration_generation) = {
            let _gate = self.gate.lock().await;
            (
                self.state.read().await.authority.clone(),
                self.sessions.read().await.clone(),
                self.configuration_generation.load(Ordering::Acquire),
            )
        };
        let progress = self.control.current_progress().await?;
        let _gate = self.gate.lock().await;
        self.active_host()?;
        if authority_before != self.state.read().await.authority
            || sessions_before != *self.sessions.read().await
            || configuration_generation != self.configuration_generation.load(Ordering::Acquire)
        {
            return Err(RuntimeError::OperationCancelled);
        }
        if progress < boundary {
            return Err(RuntimeError::ReconfigurationPending);
        }
        self.state.write().await.verified_replication_lsn = Some(boundary);
        Ok(())
    }

    async fn register_common_peer(
        &self,
        identity: ReplicaIdentity,
        session: ProcessSessionId,
    ) -> Result<()> {
        let replaced = {
            let _gate = self.gate.lock().await;
            if session.is_empty()
                || self
                    .retired_sessions
                    .read()
                    .await
                    .contains(&(identity.clone(), session.clone()))
            {
                return Err(RuntimeError::AuthorityNotAdmitted);
            }
            let old = self
                .sessions
                .write()
                .await
                .insert(identity.clone(), session.clone());
            if let Some(old) = old.filter(|old| old != &session) {
                self.retired_sessions
                    .write()
                    .await
                    .insert((identity.clone(), old));
                self.state.write().await.builds.retain(|build| {
                    build.authority.source != identity && build.authority.target != identity
                });
                true
            } else {
                false
            }
        };
        if replaced {
            self.invalidate_build_attempts().await?;
            self.fence_writes().await?;
        }
        self.configure().await
    }

    async fn apply_common_action(&self, action: RuntimeEffectAction) -> Result<()> {
        let action = match action {
            RuntimeEffectAction::AdmitBuildAuthority(authority) => {
                return self.admit_common_build(*authority).await;
            }
            RuntimeEffectAction::SetAccessStatus { read, write } => {
                return self.set_access(read, write).await;
            }
            RuntimeEffectAction::SetReadStatus(read) => {
                let write = self.state.read().await.write_status;
                return self.set_access(read, write).await;
            }
            RuntimeEffectAction::SetWriteStatus(write) => {
                let read = self.state.read().await.read_status;
                return self.set_access(read, write).await;
            }
            RuntimeEffectAction::WaitForCatchup => {
                return self.wait_for_common_catchup().await;
            }
            RuntimeEffectAction::AuthorizeFailoverPrefix(boundary) => {
                return self.authorize_common_failover_prefix(boundary).await;
            }
            RuntimeEffectAction::RegisterPeerSession { identity, session } => {
                return self.register_common_peer(identity, session).await;
            }
            action => action,
        };
        let _gate = self.gate.lock().await;
        let host = self.host()?;
        match action {
            RuntimeEffectAction::AdmitAuthority(authority) => {
                authority.validate()?;
                if self.native_receipts
                    && let Some(evidence) = &authority.scale_up
                {
                    let intent = evidence.intent();
                    if intent.target == host.identity
                        && self
                            .state
                            .read()
                            .await
                            .authority
                            .as_ref()
                            .is_none_or(|old| {
                                old.current_configuration != authority.current_configuration
                            })
                    {
                        let build = host
                            .default_dependencies
                            .build_authority_store
                            .load_build(&intent.build_id)
                            .await?
                            .ok_or(RuntimeError::AuthorityNotAdmitted)?;
                        let current_receipt = self.receipt(&build).await?;
                        let retained_receipt =
                            self.receipts.read().await.get(&intent.build_id).cloned();
                        if build.source != intent.primary
                            || build.target != intent.target
                            || build.current_configuration != intent.previous_configuration
                            || build.replication_boundary_lsn != intent.snapshot_boundary_lsn
                            || retained_receipt.as_ref().is_none_or(|retained| {
                                !retained.matches_durable_selection(&current_receipt)
                            })
                            || !self.state.read().await.builds.iter().any(|progress| {
                                progress.authority == build
                                    && progress.completed
                                    && progress.catch_up_boundary_lsn
                                        == Some(intent.catch_up_boundary_lsn)
                                    && progress.durable_lsn >= intent.catch_up_boundary_lsn
                            })
                        {
                            return Err(RuntimeError::AuthorityNotAdmitted);
                        }
                    }
                }
                self.prepare_authority_admission(&authority).await?;
                host.default_dependencies
                    .replica_authority_store
                    .admit(&authority)
                    .await?;
                self.state.write().await.authority = Some(*authority);
                self.configure().await?;
            }
            RuntimeEffectAction::AdmitBuildAuthority(_) => unreachable!(),
            RuntimeEffectAction::RegisterPeerSession { .. } => unreachable!(),
            RuntimeEffectAction::RetireBuild(id) => {
                self.retired_builds.write().await.insert(id.clone());
                self.pending_builds.write().await.remove(&id);
                if let Some(build) = host
                    .default_dependencies
                    .build_authority_store
                    .load_build(&id)
                    .await?
                {
                    self.primary.remove_replica(build.target.replica_id).await?;
                }
                self.state
                    .write()
                    .await
                    .builds
                    .retain(|b| b.authority.build_id != id);
                self.configure().await?;
                self.refresh().await?;
            }
            RuntimeEffectAction::SetAccessStatus { .. }
            | RuntimeEffectAction::SetReadStatus(_)
            | RuntimeEffectAction::SetWriteStatus(_)
            | RuntimeEffectAction::WaitForCatchup
            | RuntimeEffectAction::AuthorizeFailoverPrefix(_) => unreachable!(),
            RuntimeEffectAction::PrepareSwitchover {
                source,
                target,
                starting_configuration_id,
                starting_epoch,
                ..
            } => {
                let authority = self
                    .state
                    .read()
                    .await
                    .authority
                    .clone()
                    .ok_or(RuntimeError::AuthorityNotAdmitted)?;
                if source != host.identity
                    || authority.current_configuration.configuration_id != starting_configuration_id
                    || authority.current_configuration.epoch != starting_epoch
                    || !authority
                        .current_configuration
                        .members
                        .iter()
                        .any(|m| m.identity == target)
                {
                    return Err(RuntimeError::AuthorityNotAdmitted);
                }
                self.primary
                    .wait_for_catch_up_quorum(ReplicaSetQuorumMode::All)
                    .await?;
                self.set_access(
                    AccessStatus::ReconfigurationPending,
                    AccessStatus::ReconfigurationPending,
                )
                .await?;
                self.primary
                    .wait_for_catch_up_quorum(ReplicaSetQuorumMode::All)
                    .await?;
                self.refresh().await?;
            }
            RuntimeEffectAction::RefreshApplicationProgress => self.refresh().await?,
            RuntimeEffectAction::PrepareSecondaryRemoval {
                intent,
                process_session_id,
                report_sequence,
            } => {
                self.prepare_removal(*intent, process_session_id, report_sequence)
                    .await?;
            }
            RuntimeEffectAction::ObserveSecondaryRemovalWitness(witness) => {
                self.observe_removal(*witness).await?
            }
            RuntimeEffectAction::ObserveSecondaryRemovalProgress { witness, committed } => {
                self.observe_removal_progress(*witness, *committed).await?;
            }
            RuntimeEffectAction::AcceptSecondaryRemovalCommit(committed) => {
                self.accept_removal(*committed, false).await?
            }
            RuntimeEffectAction::AcceptHistoricalSecondaryRemovalCommit(command) => {
                kuberic_protocol::validation::validate_accept_secondary_removal_commit(&command)?;
                if !command.local_recovery || command.target != host.identity {
                    return Err(RuntimeError::AuthorityNotAdmitted);
                }
                self.accept_removal(command.committed, true).await?;
            }
            RuntimeEffectAction::FenceRetirement(retired) => {
                retired.validate(&host.identity)?;
                self.invalidate_configuration_attempts().await?;
                host.default_dependencies
                    .replica_authority_store
                    .record_retirement_started(&retired)
                    .await?;
                self.set_access(AccessStatus::NotPrimary, AccessStatus::NotPrimary)
                    .await?;
            }
            RuntimeEffectAction::CompleteRetirement(retired) => {
                retired.validate(&host.identity)?;
                let closed = host.state.read().await.fallback_snapshot.clone();
                if closed.open || closed.role != kuberic_protocol::types::ReplicaRole::None {
                    return Err(RuntimeError::ReconfigurationPending);
                }
                host.default_dependencies
                    .replica_authority_store
                    .retire(&retired)
                    .await?;
                let mut state = self.state.write().await;
                state.retired_authority = Some(*retired);
                state.open = false;
                state.authority = None;
                state.builds.clear();
                state.read_status = AccessStatus::NotPrimary;
                state.write_status = AccessStatus::NotPrimary;
                drop(state);
                host.state.write().await.fallback_snapshot.authority = None;
            }
            _ => return unavailable(),
        }
        Ok(())
    }
    pub(super) async fn snapshot(&self) -> RuntimeSnapshot {
        let mut snapshot = self.state.read().await.clone();
        if !self.native_receipts {
            return snapshot;
        }
        let receipts = self.receipts.read().await.clone();
        let mut current = Vec::new();
        for build in snapshot.builds {
            if let Some(receipt) = receipts.get(&build.authority.build_id)
                && let Ok(current_receipt) = self.receipt(&build.authority).await
                && receipt.matches_durable_selection(&current_receipt)
                && !self
                    .retired_builds
                    .read()
                    .await
                    .contains(&build.authority.build_id)
            {
                current.push(build);
            }
        }
        snapshot.builds = current;
        snapshot
    }

    async fn narrow_postcondition(&self) -> RuntimePostcondition {
        let mut snapshot = self.snapshot().await;
        if let Some(host) = self.host.upgrade() {
            let fallback = host.state.read().await.fallback_snapshot.clone();
            snapshot.open = fallback.open;
            snapshot.role = fallback.role;
            snapshot.role_transition = fallback.role_transition;
        }
        snapshot.into()
    }

    pub(super) async fn cancel_outbound_build(&self, id: &OperationId) -> Result<()> {
        let _gate = self.gate.lock().await;
        let mut generations = self.build_generations.write().await;
        let generation = generations.entry(id.clone()).or_default();
        *generation = generation
            .checked_add(1)
            .ok_or(RuntimeError::OperationCancelled)?;
        drop(generations);
        self.state
            .write()
            .await
            .builds
            .retain(|b| &b.authority.build_id != id);
        self.changed.notify_waiters();
        if let Some(build) = self
            .host()?
            .default_dependencies
            .build_authority_store
            .load_build(id)
            .await?
        {
            self.primary.remove_replica(build.target.replica_id).await?;
        }
        self.configure().await?;
        self.cancelling_builds.write().await.remove(id);
        Ok(())
    }
    pub(super) async fn next_outbound(&self) -> Option<OutboundOperation> {
        loop {
            if self
                .abort_notified
                .load(std::sync::atomic::Ordering::Acquire)
                || self.host.upgrade().is_none_or(|host| {
                    host.aborted.load(std::sync::atomic::Ordering::Acquire)
                        || host.closed.load(std::sync::atomic::Ordering::Acquire)
                })
            {
                return None;
            }
            let changed = self.changed.notified();
            let mut receiver = self.receiver.lock().await;
            tokio::select! {
                item = receiver.recv() => return item,
                _ = changed => {}
                _ = tokio::time::sleep(std::time::Duration::from_millis(10)) => {}
            }
        }
    }

    fn notify_abort(&self) {
        self.abort_notified
            .store(true, std::sync::atomic::Ordering::Release);
        let _ = self.configuration_generation.fetch_update(
            Ordering::AcqRel,
            Ordering::Acquire,
            |generation| generation.checked_add(1),
        );
        let _ = self.access_generation.fetch_update(
            Ordering::AcqRel,
            Ordering::Acquire,
            |generation| generation.checked_add(1),
        );
        if let Some(handle) = self.deferred_configuration_abort.lock().unwrap().take() {
            handle.abort();
        }
        self.changed.notify_waiters();
    }
}

#[async_trait]
impl ReplicatorLifecycleBackend for CustomReplicatorHost {
    fn owns_stream_session(&self) -> bool {
        false
    }

    async fn complete_open(&self, address: String) -> Result<()> {
        CustomReplicatorHost::complete_open(self, address).await
    }

    async fn complete_close(&self) -> Result<()> {
        self.terminate().await
    }

    async fn complete_abort(&self) {
        self.notify_abort();
        self.terminate().await.ok();
    }

    fn notify_abort(&self) {
        CustomReplicatorHost::notify_abort(self);
    }

    async fn fence_writes(&self) -> Result<()> {
        CustomReplicatorHost::fence_writes(self).await
    }

    async fn settle_primary_prefix(&self) -> Result<()> {
        CustomReplicatorHost::settle_primary_prefix(self).await
    }

    async fn cancel_configuration_work(&self) -> Result<()> {
        CustomReplicatorHost::cancel_configuration_work(self).await
    }

    async fn restore_authority(&self) -> Result<()> {
        CustomReplicatorHost::restore_authority(self).await
    }

    async fn restore_access(&self, read: AccessStatus, write: AccessStatus) -> Result<()> {
        CustomReplicatorHost::restore_access(self, read, write).await
    }

    async fn admit_authority(&self, authority: AdmittedAuthority) -> Result<()> {
        CustomReplicatorHost::apply_common_action(
            self,
            RuntimeEffectAction::AdmitAuthority(Box::new(authority)),
        )
        .await
    }

    async fn admit_build_authority(&self, authority: BuildAuthority) -> Result<()> {
        CustomReplicatorHost::apply_common_action(
            self,
            RuntimeEffectAction::AdmitBuildAuthority(Box::new(authority)),
        )
        .await
    }

    async fn register_peer_session(
        &self,
        identity: ReplicaIdentity,
        session: ProcessSessionId,
    ) -> Result<()> {
        CustomReplicatorHost::apply_common_action(
            self,
            RuntimeEffectAction::RegisterPeerSession { identity, session },
        )
        .await
    }

    async fn retire_build(&self, build_id: OperationId) -> Result<()> {
        CustomReplicatorHost::apply_common_action(self, RuntimeEffectAction::RetireBuild(build_id))
            .await
    }

    async fn set_access(&self, read: AccessStatus, write: AccessStatus) -> Result<()> {
        CustomReplicatorHost::set_access(self, read, write).await
    }

    async fn begin_access_effect(
        &self,
        read: AccessStatus,
        write: AccessStatus,
    ) -> Result<AccessEffectCommit> {
        self.begin_common_access_effect(read, write).await
    }

    async fn wait_for_catch_up(&self) -> Result<()> {
        CustomReplicatorHost::apply_common_action(self, RuntimeEffectAction::WaitForCatchup).await
    }

    async fn authorize_failover_prefix(&self, boundary: i64) -> Result<()> {
        CustomReplicatorHost::apply_common_action(
            self,
            RuntimeEffectAction::AuthorizeFailoverPrefix(boundary),
        )
        .await
    }

    async fn prepare_switchover(
        &self,
        preparation_generation: u64,
        request_id: kuberic_protocol::types::SwitchoverRequestId,
        source: ReplicaIdentity,
        target: ReplicaIdentity,
        starting_configuration_id: kuberic_protocol::types::ConfigurationId,
        starting_epoch: kuberic_protocol::types::Epoch,
    ) -> Result<()> {
        CustomReplicatorHost::apply_common_action(
            self,
            RuntimeEffectAction::PrepareSwitchover {
                preparation_generation,
                request_id,
                source,
                target,
                starting_configuration_id,
                starting_epoch,
            },
        )
        .await
    }

    async fn refresh_progress(&self) -> Result<()> {
        CustomReplicatorHost::apply_common_action(
            self,
            RuntimeEffectAction::RefreshApplicationProgress,
        )
        .await
    }

    async fn observe_progress(&self) -> Result<()> {
        if !CustomReplicatorHost::snapshot(self).await.open {
            return Ok(());
        }
        let result = CustomReplicatorHost::apply_common_action(
            self,
            RuntimeEffectAction::RefreshApplicationProgress,
        )
        .await;
        if matches!(result, Err(RuntimeError::NotOpen | RuntimeError::Closed)) {
            self.fence_managed_access().await?;
            self.mark_not_open().await;
            return Ok(());
        }
        result
    }

    async fn prepare_secondary_removal(
        &self,
        intent: kuberic_protocol::types::SecondaryScaleDownIntent,
        process_session_id: ProcessSessionId,
        report_sequence: u64,
    ) -> Result<()> {
        CustomReplicatorHost::apply_common_action(
            self,
            RuntimeEffectAction::PrepareSecondaryRemoval {
                intent: Box::new(intent),
                process_session_id,
                report_sequence,
            },
        )
        .await
    }

    async fn observe_secondary_removal(
        &self,
        witness: kuberic_protocol::types::SecondaryRemovalWitness,
    ) -> Result<()> {
        CustomReplicatorHost::apply_common_action(
            self,
            RuntimeEffectAction::ObserveSecondaryRemovalWitness(Box::new(witness)),
        )
        .await
    }

    async fn observe_secondary_removal_progress(
        &self,
        witness: kuberic_protocol::types::SecondaryRemovalWitness,
        committed: kuberic_protocol::types::SecondaryScaleDownCleanup,
    ) -> Result<()> {
        CustomReplicatorHost::apply_common_action(
            self,
            RuntimeEffectAction::ObserveSecondaryRemovalProgress {
                witness: Box::new(witness),
                committed: Box::new(committed),
            },
        )
        .await
    }

    async fn accept_secondary_removal(
        &self,
        committed: kuberic_protocol::types::SecondaryScaleDownCleanup,
    ) -> Result<()> {
        CustomReplicatorHost::apply_common_action(
            self,
            RuntimeEffectAction::AcceptSecondaryRemovalCommit(Box::new(committed)),
        )
        .await
    }

    async fn accept_historical_secondary_removal(
        &self,
        command: kuberic_protocol::command::AcceptSecondaryRemovalCommit,
    ) -> Result<()> {
        CustomReplicatorHost::apply_common_action(
            self,
            RuntimeEffectAction::AcceptHistoricalSecondaryRemovalCommit(Box::new(command)),
        )
        .await
    }

    async fn fence_retirement(
        &self,
        retired: kuberic_runtime_internal::authority::RetiredAuthority,
    ) -> Result<()> {
        CustomReplicatorHost::apply_common_action(
            self,
            RuntimeEffectAction::FenceRetirement(Box::new(retired)),
        )
        .await
    }

    async fn complete_retirement(
        &self,
        retired: kuberic_runtime_internal::authority::RetiredAuthority,
    ) -> Result<()> {
        CustomReplicatorHost::apply_common_action(
            self,
            RuntimeEffectAction::CompleteRetirement(Box::new(retired)),
        )
        .await
    }

    async fn snapshot(&self) -> RuntimeSnapshot {
        CustomReplicatorHost::snapshot(self).await
    }

    async fn cancel_outbound_build(&self, id: &OperationId) -> Result<()> {
        CustomReplicatorHost::cancel_outbound_build(self, id).await
    }

    async fn cancel_outbound_build_attempt(&self, id: &OperationId, generation: u64) -> Result<()> {
        if !self.cancel_common_build_attempt(id, generation).await? {
            return Ok(());
        }
        self.configure().await?;
        self.complete_common_build_cancellation(id, generation)
            .await;
        Ok(())
    }

    async fn build_generation(&self, id: &OperationId) -> u64 {
        CustomReplicatorHost::build_generation(self, id).await
    }

    async fn next_outbound(&self) -> Option<OutboundOperation> {
        CustomReplicatorHost::next_outbound(self).await
    }

    async fn wait_for_build_completion(
        &self,
        build_id: &OperationId,
        target: &ReplicaIdentity,
    ) -> Result<()> {
        let generation = self.build_generation(build_id).await;
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(75);
        loop {
            let changed = self.changed.notified();
            let snapshot = CustomReplicatorHost::snapshot(self).await;
            if snapshot.builds.iter().any(|build| {
                &build.authority.build_id == build_id
                    && &build.authority.target == target
                    && build.completed
            }) {
                return Ok(());
            }

            self.ensure_build_generation(build_id, generation).await?;
            if tokio::time::Instant::now() >= deadline {
                return Err(RuntimeError::OperationCancelled);
            }
            tokio::select! {
                _ = changed => {}
                _ = tokio::time::sleep(std::time::Duration::from_millis(10)) => {}
            }
        }
    }

    async fn build_replica(&self, replica: ReplicaInformation) -> Result<()> {
        let build_id = replica.build_id.clone();
        let target = replica.identity.clone();
        if self.snapshot().await.builds.iter().any(|build| {
            build.authority.build_id == build_id
                && build.authority.target == target
                && build.completed
        }) {
            return Ok(());
        }
        self.enqueue_build_wait(ReplicaEndpoint {
            build_id: build_id.clone(),
            identity: target.clone(),
            replication_address: replica.replication_address,
        })
        .await?;
        self.wait_for_build_completion(&build_id, &target).await
    }

    async fn remove_replica(&self, replica_id: kuberic_protocol::types::ReplicaId) -> Result<()> {
        let authority_before = self.state.read().await.authority.clone();
        let sessions_before = self.sessions.read().await.clone();
        let configuration_generation = self.configuration_generation.load(Ordering::Acquire);
        self.primary.remove_replica(replica_id).await?;
        self.active_host()?;
        if authority_before != self.state.read().await.authority
            || sessions_before != *self.sessions.read().await
            || configuration_generation != self.configuration_generation.load(Ordering::Acquire)
        {
            return Err(RuntimeError::OperationCancelled);
        }
        self.remove_managed_replica(replica_id).await
    }

    async fn select_build(&self, authority: &BuildAuthority) -> Result<()> {
        CustomReplicatorHost::select_build(self, authority).await
    }

    async fn describe_peer(&self, replica: ReplicaInformation) -> Result<()> {
        CustomReplicatorHost::describe_peer(self, replica).await
    }

    async fn execute_build(&self, replica: ReplicaInformation) -> Result<Option<BuildAdmission>> {
        CustomReplicatorHost::execute_build(self, replica).await?;
        Ok(None)
    }

    async fn accept_build(&self, receipt: Option<BuildAdmission>) -> Result<()> {
        if receipt.is_some() {
            return Err(RuntimeError::Application(
                "independent build returned a managed acceptance receipt".into(),
            ));
        }
        Ok(())
    }

    async fn enqueue_build(&self, endpoint: ReplicaEndpoint) -> Result<()> {
        CustomReplicatorHost::enqueue_build(self, endpoint).await
    }

    async fn effect_evidence(
        &self,
        _action: &RuntimeEffectAction,
    ) -> Option<RuntimeOperationEvidence> {
        None
    }

    async fn confirm_build_completion(
        &self,
        build_id: &OperationId,
        target: &ReplicaIdentity,
    ) -> Result<Option<RuntimeOperationEvidence>> {
        let receipt = self
            .receipts
            .read()
            .await
            .get(build_id)
            .cloned()
            .ok_or(RuntimeError::ReconfigurationPending)?;
        if &receipt.selection.authority.target != target
            || !self
                .receipt(&receipt.selection.authority)
                .await?
                .matches_host_admission(&receipt)
            || !self.state.read().await.builds.iter().any(|build| {
                &build.authority.build_id == build_id
                    && &build.authority.target == target
                    && build.completed
            })
        {
            return Err(RuntimeError::ReconfigurationPending);
        }
        Ok(None)
    }

    async fn postcondition(&self) -> RuntimePostcondition {
        self.narrow_postcondition().await
    }
}

fn preserves_same_primary_scale_up_access(
    existing: &AdmittedAuthority,
    next: &AdmittedAuthority,
) -> bool {
    matches!(
        next.scale_up.as_deref(),
        Some(kuberic_protocol::types::ScaleUpConfigurationEvidence::Admission { .. })
    ) && existing.primary_identity() == next.primary_identity()
        && next.local_identity == *next.primary_identity()
        && existing.local_identity == next.local_identity
}

#[cfg(test)]
#[allow(clippy::items_after_test_module)]
mod receipt_validation_tests {
    use super::*;
    use kuberic_protocol::types::{
        AgentGeneration, BuildAuthorityKind, ConfigurationMember, Epoch, ReplicaId,
        ReplicaInstanceId, ReplicaRole,
    };
    use kuberic_runtime_internal::authority::{BuildSelection, DurableBuildProgress};
    use std::sync::atomic::AtomicUsize;

    fn identity(id: i64, instance: &str) -> ReplicaIdentity {
        ReplicaIdentity {
            replica_id: ReplicaId::new(id),
            instance_id: ReplicaInstanceId::new(instance),
            agent_generation: AgentGeneration::new(format!("generation-{instance}")),
        }
    }

    fn authority() -> AdmittedAuthority {
        let primary = identity(1, "primary");
        let secondary = identity(2, "secondary");
        AdmittedAuthority {
            local_identity: primary.clone(),
            transition_kind: None,
            previous_configuration: None,
            current_configuration: ConfigurationDescriptor::new(
                Epoch::new(0, 1),
                primary.replica_id,
                vec![
                    ConfigurationMember {
                        identity: primary,
                        role: ReplicaRole::Primary,
                    },
                    ConfigurationMember {
                        identity: secondary,
                        role: ReplicaRole::ActiveSecondary,
                    },
                ],
                2,
            ),
            switchover_handoff: None,
            secondary_removal: None,
            scale_up: None,
        }
    }

    fn token(authority: &AdmittedAuthority) -> NativeOperationToken {
        NativeOperationToken {
            authority: Some(authority.clone()),
            engine_session_id: "engine-session".into(),
            engine_generation: 7,
        }
    }

    struct TestFence {
        calls: AtomicUsize,
        notified: Notify,
    }

    #[async_trait]
    impl AccessRollbackFence for TestFence {
        async fn fence_writes(&self) {
            self.calls.fetch_add(1, Ordering::AcqRel);
            self.notified.notify_waiters();
        }

        fn abort(&self) {
            self.calls.fetch_add(1, Ordering::AcqRel);
            self.notified.notify_waiters();
        }
    }

    type RollbackFixture = (
        AccessPublicationRollback,
        Arc<TestFence>,
        Arc<RwLock<RuntimeSnapshot>>,
        Arc<AtomicU64>,
        Arc<Mutex<()>>,
    );

    fn rollback_fixture(generation: u64) -> RollbackFixture {
        let fence = Arc::new(TestFence {
            calls: AtomicUsize::new(0),
            notified: Notify::new(),
        });
        let mut snapshot = empty_snapshot(identity(1, "primary"));
        snapshot.read_status = AccessStatus::Granted;
        snapshot.write_status = AccessStatus::Granted;
        let state = Arc::new(RwLock::new(snapshot));
        let access_generation = Arc::new(AtomicU64::new(generation));
        let published_access_generation = Arc::new(AtomicU64::new(generation));
        let access_commit = Arc::new(Mutex::new(()));
        (
            AccessPublicationRollback {
                fence: fence.clone(),
                host: Weak::new(),
                common_state: state.clone(),
                access_generation: access_generation.clone(),
                published_access_generation,
                access_commit: access_commit.clone(),
                generation,
                armed: true,
            },
            fence,
            state,
            access_generation,
            access_commit,
        )
    }

    #[test]
    fn access_publication_requires_the_exact_preparation_receipt() {
        let authority = authority();
        let projection = AccessProjection {
            read: AccessStatus::Granted,
            write: AccessStatus::Granted,
            authority: Some(authority.clone()),
            configuration: None,
            sessions: BTreeMap::new(),
            role: ReplicaRole::Primary,
            configuration_generation: 11,
            access_generation: 13,
            faulted_grant: false,
        };
        let preparation = AccessReceipt {
            authority: Some(authority),
            engine_session_id: "engine-session".into(),
            engine_generation: 7,
            read: AccessStatus::Granted,
            write: AccessStatus::Granted,
            current_progress: 19,
            committed_lsn: 17,
            published: false,
        };
        let publication = AccessReceipt {
            published: true,
            ..preparation.clone()
        };
        validate_access_receipt(&preparation, &publication, &projection, true).unwrap();

        let mut stale = publication.clone();
        stale.engine_session_id = "other-session".into();
        assert!(validate_access_receipt(&preparation, &stale, &projection, true).is_err());
        let mut stale = publication.clone();
        stale.engine_generation += 1;
        assert!(validate_access_receipt(&preparation, &stale, &projection, true).is_err());
        let mut stale = publication;
        stale.committed_lsn -= 1;
        assert!(validate_access_receipt(&preparation, &stale, &projection, true).is_err());
    }

    #[test]
    fn standard_receipts_reject_stale_native_tokens_and_targets() {
        let authority = authority();
        let token = token(&authority);
        let catch_up = CatchUpReceipt {
            authority: authority.clone(),
            engine_session_id: token.engine_session_id.clone(),
            engine_generation: token.engine_generation,
            boundary_lsn: 8,
            current_progress: 8,
            committed_lsn: 8,
            current_configuration_quorum_progress: 8,
        };
        validate_catch_up_receipt(&catch_up, &token, Some(&authority), 8).unwrap();
        let mut stale_token = token.clone();
        stale_token.engine_generation += 1;
        assert!(validate_catch_up_receipt(&catch_up, &stale_token, Some(&authority), 8).is_err());
        let mut stale_catch_up = catch_up.clone();
        stale_catch_up.authority.current_configuration.epoch = Epoch::new(0, 2);
        assert!(validate_catch_up_receipt(&stale_catch_up, &token, Some(&authority), 8).is_err());

        let target = identity(3, "idle");
        let build_authority = BuildAuthority {
            build_id: OperationId::new("build"),
            kind: BuildAuthorityKind::Provisioning,
            source: authority.local_identity.clone(),
            target: target.clone(),
            current_configuration: authority.current_configuration.clone(),
            replication_boundary_lsn: 8,
        };
        let selection = BuildSelection {
            authority: build_authority.clone(),
            generation: 3,
        };
        let admission = BuildAdmission {
            selection: selection.clone(),
            source_session: ProcessSessionId::default(),
            target_session: ProcessSessionId::default(),
            attempt_generation: 5,
            configuration_generation: 11,
            native_token: Some(token.clone()),
        };
        let native = NativeBuildReceipt {
            selection,
            progress: DurableBuildProgress {
                authority: build_authority,
                last_sequence: 1,
                durable_lsn: 8,
                completed: true,
                catch_up_boundary_lsn: Some(8),
            },
            engine_session_id: token.engine_session_id.clone(),
            engine_generation: token.engine_generation,
        };
        validate_build_receipt(&native, &admission).unwrap();
        let mut current_admission = admission.clone();
        current_admission.native_token = None;
        assert!(current_admission.matches_host_admission(&admission));
        let mut stale_admission = current_admission.clone();
        stale_admission.source_session = ProcessSessionId::new("stale-source");
        assert!(!stale_admission.matches_host_admission(&admission));
        let mut stale_admission = current_admission.clone();
        stale_admission.target_session = ProcessSessionId::new("stale-target");
        assert!(!stale_admission.matches_host_admission(&admission));
        let mut stale_admission = current_admission.clone();
        stale_admission.attempt_generation += 1;
        assert!(!stale_admission.matches_host_admission(&admission));
        let mut stale_admission = current_admission.clone();
        stale_admission.configuration_generation += 1;
        assert!(!stale_admission.matches_host_admission(&admission));
        let mut stale_admission = current_admission;
        stale_admission.selection.generation += 1;
        assert!(!stale_admission.matches_host_admission(&admission));
        let mut stale_native = native;
        stale_native.engine_generation += 1;
        assert!(validate_build_receipt(&stale_native, &admission).is_err());

        let removal = RemovalReceipt {
            authority: authority.clone(),
            replica_id: target.replica_id,
            retired_build_ids: vec![OperationId::new("build")],
            engine_session_id: token.engine_session_id.clone(),
            engine_generation: token.engine_generation,
        };
        validate_removal_receipt(&removal, &token, Some(&authority), target.replica_id).unwrap();
        assert!(
            validate_removal_receipt(&removal, &token, Some(&authority), ReplicaId::new(99),)
                .is_err()
        );
    }

    #[test]
    fn topology_receipts_reject_stale_engine_identity_and_generation() {
        let authority = authority();
        let token = token(&authority);
        let prefix = CertifiedPrefixReceipt {
            token: token.clone(),
            verified_lsn: 9,
            settled_lsn: 8,
            committed_lsn: 8,
        };
        validate_certified_prefix_receipt(&prefix, &token).unwrap();
        let mut stale = token.clone();
        stale.engine_session_id = "restarted-engine".into();
        assert!(validate_certified_prefix_receipt(&prefix, &stale).is_err());
        let mut stale = token.clone();
        stale.engine_generation += 1;
        assert!(validate_certified_prefix_receipt(&prefix, &stale).is_err());

        let removal = SecondaryRemovalReceipt {
            token: token.clone(),
            preparation: None,
            witness: None,
            accepted: None,
            verified_lsn: Some(9),
            committed_lsn: 8,
        };
        validate_secondary_removal_receipt(&removal, &token).unwrap();
        let mut stale_removal = removal;
        stale_removal.token.engine_generation += 1;
        assert!(validate_secondary_removal_receipt(&stale_removal, &token).is_err());
    }

    #[tokio::test]
    async fn dropped_access_effect_rolls_back_the_exact_publication() {
        let (rollback, fence, state, generation, _) = rollback_fixture(5);
        let notified = fence.notified.notified();
        drop(AccessEffectCommit::managed(rollback));
        tokio::time::timeout(std::time::Duration::from_secs(1), notified)
            .await
            .unwrap();
        assert_eq!(fence.calls.load(Ordering::Acquire), 1);
        assert_eq!(generation.load(Ordering::Acquire), 6);
        let state = state.read().await;
        assert_eq!(state.read_status, AccessStatus::ReconfigurationPending);
        assert_eq!(state.write_status, AccessStatus::ReconfigurationPending);
    }

    #[tokio::test]
    async fn committed_access_effect_disarms_rollback() {
        let (rollback, fence, state, generation, _) = rollback_fixture(7);
        AccessEffectCommit::managed(rollback).commit();
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        assert_eq!(fence.calls.load(Ordering::Acquire), 0);
        assert_eq!(generation.load(Ordering::Acquire), 7);
        assert_eq!(state.read().await.write_status, AccessStatus::Granted);
    }

    #[tokio::test]
    async fn stale_rollback_cannot_fence_a_newer_access_publication() {
        let (rollback, fence, state, generation, commit) = rollback_fixture(9);
        let commit_guard = commit.lock().await;
        rollback
            .published_access_generation
            .store(11, Ordering::Release);
        drop(AccessEffectCommit::managed(rollback));
        generation.store(11, Ordering::Release);
        drop(commit_guard);
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        assert_eq!(fence.calls.load(Ordering::Acquire), 0);
        assert_eq!(state.read().await.write_status, AccessStatus::Granted);
    }
}

fn merge_builds(current: &mut Vec<BuildPostcondition>, incoming: Vec<BuildPostcondition>) {
    for build in incoming {
        if let Some(existing) = current
            .iter_mut()
            .find(|existing| existing.authority.build_id == build.authority.build_id)
        {
            *existing = build;
        } else {
            current.push(build);
        }
    }
}

fn unavailable<T>() -> Result<T> {
    Err(RuntimeError::Application(
        "the selected replicator does not support this managed operation".into(),
    ))
}
