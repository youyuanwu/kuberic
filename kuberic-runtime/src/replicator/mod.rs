pub(crate) mod configuration;
pub(crate) mod copy;
mod queue;
pub(crate) mod quorum;
pub(crate) mod sender;
pub mod stream;

pub(crate) mod log;

#[cfg(test)]
mod capability_tests;

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex, OnceLock};

use crate::capabilities::{ReplicatorCreationIdentity, RuntimeHostToken};
use crate::protocol::types::{
    AccessStatus, ConfigurationDescriptor, ConfigurationId, Epoch, FaultType, HistoryContext,
    LoadMetric, OperationId, PartitionInformation, ProcessSessionId, PublicProtocolGeneration,
    ReplicaId, ReplicaIdentity, ReplicaRole, ReplicaStorageIncarnationContext, ResourceUid,
    SecondaryRemovalWitness, SecondaryScaleDownCleanup, SecondaryScaleDownIntent,
    SwitchoverRequestId,
};
use crate::receipts::{NativeProgressStatus, NativeTopologyStatus};
use async_trait::async_trait;
use tokio::sync::{Mutex, OwnedMutexGuard, RwLock};

use crate::application::{ClientWrite, INVALID_LSN, Lsn, OperationData, StateProvider};
use crate::authority::{
    BuildAuthority, BuildAuthorityStore, BuildProgressStore, LocalWriteJournal,
    ReplicaAuthorityStore, ReplicationProgressStore, RetiredAuthority,
};
use crate::effects::BuildPostcondition;
use crate::engine::DurableState;
use crate::protocol::authorization::{
    AuthorizationKeyId, AuthorizationVerificationKey, BuildAuthorizationEndpointV1,
    BuildAuthorizationV1, PeerSessionAuthorizationV1,
};
use crate::protocol::public_operations::{
    DataLossReplay, PublicConfiguration, PublicReplicaDescriptor,
};
use crate::replicator::configuration::ManagedReplicaConfiguration;
use crate::replicator::copy::{PrepareCopyRequest, PreparedCopy};
#[cfg(all(test, kuberic_workspace_tests))]
use crate::runtime::PendingWrite;
use crate::runtime::{DefaultReplicatorInner, PendingReplication};
use crate::transport::{CopyAck, CopyItem, OutboundOperation, ReplicationAck, ReplicationItem};
use crate::{Result, RuntimeError};
use stream::{OperationStream, ServiceStreams};

/// IFabricReplicator, with COM Begin/End pairs collapsed to async calls.
#[async_trait]
pub trait Replicator: Send + Sync {
    async fn open(&self) -> Result<String>;
    async fn change_role(&self, epoch: Epoch, role: ReplicaRole) -> Result<()>;
    async fn update_epoch(&self, epoch: Epoch) -> Result<()>;
    async fn close(&self) -> Result<()>;
    fn abort(&self);
    /// Primary: locally durable quorum-committed progress. Secondary: highest
    /// contiguous durably accepted progress. Recovery: election-only progress
    /// while read/write access remains closed.
    async fn current_progress(&self) -> Result<Lsn>;
    /// Earliest retained public progress that can catch up an admitted peer.
    /// `INVALID_LSN` means the capability is not currently known or applicable.
    async fn catch_up_capability(&self) -> Result<Lsn>;
}

/// IFabricPrimaryReplicator; engine bookkeeping is deliberately not part of this API.
#[async_trait]
pub trait PrimaryReplicator: Replicator {
    async fn on_data_loss(&self) -> Result<bool>;
    async fn update_catch_up_replica_set_configuration(
        &self,
        current: ReplicaSetConfiguration,
        previous: ReplicaSetConfiguration,
    ) -> Result<()>;
    async fn wait_for_catch_up_quorum(&self, mode: ReplicaSetQuorumMode) -> Result<()>;
    async fn update_current_replica_set_configuration(
        &self,
        current: ReplicaSetConfiguration,
    ) -> Result<()>;
    async fn build_replica(&self, replica: ReplicaInformation) -> Result<()>;
    async fn remove_replica(&self, replica_id: ReplicaId) -> Result<()>;
}

#[async_trait]
pub trait StateReplicator: Send + Sync {
    /// Completes only after durable local acceptance and the admitted PC/CC write quorums.
    async fn replicate(&self, data: OperationData) -> Result<Lsn>;
    async fn get_replication_stream(&self) -> Result<OperationStream>;
    async fn get_copy_stream(&self) -> Result<OperationStream>;
    async fn update_replicator_settings(&self, settings: ReplicatorSettings) -> Result<()>;
}

#[async_trait]
#[doc(hidden)]
pub(crate) trait ManagedReplicatorLifecycle: Send + Sync {
    async fn fence_writes(&self) -> Result<()>;
    async fn settle_primary_prefix(&self) -> Result<ManagedCertifiedPrefixOutcome>;
    async fn cancel_configuration_work(&self) -> Result<()>;
    async fn prepare_access(
        &self,
        read: AccessStatus,
        write: AccessStatus,
    ) -> Result<ManagedAccessPreparation>;
    async fn publish_access(&self, preparation: ManagedAccessPreparation) -> Result<()>;
    async fn lock_native_fence(
        &self,
        expected: &ManagedOperationFence,
    ) -> Result<ManagedFenceGuard>;
    async fn native_fence(&self) -> Result<ManagedOperationFence>;
    async fn current_engine_fence(&self) -> ManagedOperationFence;
    async fn progress_status(&self) -> NativeProgressStatus;
    async fn topology_status(&self) -> NativeTopologyStatus;
    async fn prepare_replica_configuration(
        &self,
        configuration: ManagedReplicaConfiguration,
        host_generation: u64,
    ) -> Result<ManagedConfigurationPreparation>;
    async fn commit_replica_configuration(
        &self,
        preparation: ManagedConfigurationPreparation,
    ) -> Result<()>;
    async fn synchronize_replica_configuration(&self, host_generation: u64) -> Result<()>;
    async fn authorize_failover_prefix(
        &self,
        boundary: Lsn,
    ) -> Result<ManagedCertifiedPrefixOutcome>;
    async fn prepare_switchover(
        &self,
        preparation_generation: u64,
        request_id: SwitchoverRequestId,
        source: ReplicaIdentity,
        target: ReplicaIdentity,
        starting_configuration_id: ConfigurationId,
        starting_epoch: Epoch,
    ) -> Result<ManagedSwitchoverOutcome>;
    async fn prepare_secondary_removal(
        &self,
        intent: SecondaryScaleDownIntent,
        process_session_id: ProcessSessionId,
        report_sequence: u64,
    ) -> Result<ManagedRemovalPreparationOutcome>;
    async fn observe_secondary_removal_witness(
        &self,
        witness: SecondaryRemovalWitness,
    ) -> Result<ManagedRemovalWitnessOutcome>;
    async fn observe_secondary_removal_progress(
        &self,
        witness: SecondaryRemovalWitness,
        committed: SecondaryScaleDownCleanup,
    ) -> Result<ManagedRemovalProgressOutcome>;
    async fn accept_secondary_removal_commit(
        &self,
        committed: SecondaryScaleDownCleanup,
    ) -> Result<ManagedRemovalAcceptanceOutcome>;
    async fn accept_historical_secondary_removal_commit(
        &self,
        command: crate::protocol::command::AcceptSecondaryRemovalCommit,
    ) -> Result<ManagedHistoricalRemovalAcceptanceOutcome>;
    async fn fence_retirement(
        &self,
        retired: RetiredAuthority,
    ) -> Result<ManagedRetirementFenceOutcome>;
    async fn complete_retirement(
        &self,
        retired: RetiredAuthority,
    ) -> Result<ManagedRetirementCompletionOutcome>;
    async fn register_peer_session_proof(
        &self,
        identity: ReplicaIdentity,
        session: ProcessSessionId,
    ) -> Result<()>;
    async fn admit_build_authority_proof(&self, authority: BuildAuthority) -> Result<()>;
    async fn retire_build_proof(&self, build_id: OperationId) -> Result<()>;
    async fn refresh_progress_proof(&self) -> Result<()>;
    async fn restore_engine_proof(
        &self,
        configuration: Option<ManagedReplicaConfiguration>,
        host_generation: u64,
    ) -> Result<()>;
    async fn observe_engine(&self) -> ManagedReplicaObservation;
    async fn cancel_outbound_build(&self, build_id: &OperationId) -> Result<()>;
    async fn detach_outbound_build_stream(&self, build_id: &OperationId) -> Result<()>;
    async fn complete_open(&self, replication_address: String) -> Result<()>;
    async fn attach_interfaces(
        &self,
        control: Arc<dyn Replicator>,
        primary: Option<Arc<dyn PrimaryReplicator>>,
    ) -> Result<()>;
    fn abort(&self);
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ManagedOperationFence {
    pub(crate) configuration: Option<ManagedReplicaConfiguration>,
    pub(crate) engine_session_id: String,
    pub(crate) engine_generation: u64,
}

#[derive(Debug)]
pub(crate) struct ManagedConfigurationPreparation {
    pub(crate) fence: ManagedOperationFence,
    pub(crate) configuration: ManagedReplicaConfiguration,
    pub(crate) host_generation: u64,
    pub(crate) active: Arc<AtomicBool>,
    pub(crate) native_generation: Arc<AtomicU64>,
}

impl Drop for ManagedConfigurationPreparation {
    fn drop(&mut self) {
        if self.active.swap(false, Ordering::AcqRel) {
            self.native_generation.fetch_add(1, Ordering::AcqRel);
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ManagedAccessPreparation {
    pub(crate) fence: ManagedOperationFence,
    pub(crate) read: AccessStatus,
    pub(crate) write: AccessStatus,
    pub(crate) current_progress: Lsn,
    pub(crate) committed_lsn: Lsn,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ManagedCertifiedPrefixOutcome {
    pub(crate) fence: ManagedOperationFence,
    pub(crate) verified_lsn: Lsn,
    pub(crate) settled_lsn: Lsn,
    pub(crate) committed_lsn: Lsn,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ManagedSwitchoverOutcome {
    pub(crate) fence: ManagedOperationFence,
    pub(crate) preparation_generation: u64,
    pub(crate) request_id: SwitchoverRequestId,
    pub(crate) source: ReplicaIdentity,
    pub(crate) target: ReplicaIdentity,
    pub(crate) starting_configuration_id: ConfigurationId,
    pub(crate) starting_epoch: Epoch,
    pub(crate) handoff_lsn: Lsn,
    pub(crate) committed_lsn: Lsn,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ManagedRemovalPreparationOutcome {
    pub(crate) fence: ManagedOperationFence,
    pub(crate) preparation: crate::protocol::types::SecondaryRemovalPreparation,
    pub(crate) verified_lsn: Option<Lsn>,
    pub(crate) committed_lsn: Lsn,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ManagedRemovalWitnessOutcome {
    pub(crate) fence: ManagedOperationFence,
    pub(crate) witness: SecondaryRemovalWitness,
    pub(crate) verified_lsn: Option<Lsn>,
    pub(crate) committed_lsn: Lsn,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ManagedRemovalProgressOutcome {
    pub(crate) fence: ManagedOperationFence,
    pub(crate) witness: SecondaryRemovalWitness,
    pub(crate) accepted: SecondaryScaleDownCleanup,
    pub(crate) verified_lsn: Option<Lsn>,
    pub(crate) committed_lsn: Lsn,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ManagedRemovalAcceptanceOutcome {
    pub(crate) fence: ManagedOperationFence,
    pub(crate) accepted: SecondaryScaleDownCleanup,
    pub(crate) verified_lsn: Option<Lsn>,
    pub(crate) committed_lsn: Lsn,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ManagedHistoricalRemovalAcceptanceOutcome {
    pub(crate) fence: ManagedOperationFence,
    pub(crate) accepted: SecondaryScaleDownCleanup,
    pub(crate) verified_lsn: Option<Lsn>,
    pub(crate) committed_lsn: Lsn,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ManagedRetirementFenceOutcome {
    pub(crate) fence: ManagedOperationFence,
    pub(crate) retired: RetiredAuthority,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ManagedRetirementCompletionOutcome {
    pub(crate) fence: ManagedOperationFence,
    pub(crate) retired: RetiredAuthority,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ManagedReplicaObservation {
    pub(crate) fence: ManagedOperationFence,
    pub(crate) host_generation: u64,
    #[allow(dead_code)]
    pub(crate) diagnostic_revision: u64,
    pub(crate) progress: NativeProgressStatus,
    pub(crate) catch_up_capability: Lsn,
    pub(crate) builds: Vec<BuildPostcondition>,
    pub(crate) prepared_secondary_removal:
        Option<crate::protocol::types::SecondaryRemovalPreparation>,
    pub(crate) accepted_secondary_removal: Option<SecondaryScaleDownCleanup>,
    pub(crate) retired_authority: Option<RetiredAuthority>,
}

#[async_trait]
pub(crate) trait ManagedReplicaStore: Send + Sync {
    async fn load_configuration(&self) -> Result<Option<ManagedReplicaConfiguration>>;
    async fn load_secondary_removal(
        &self,
    ) -> Result<Option<crate::protocol::types::SecondaryRemovalPreparation>>;
    async fn record_secondary_removal(
        &self,
        preparation: &crate::protocol::types::SecondaryRemovalPreparation,
    ) -> Result<()>;
    async fn load_secondary_removal_commit(&self) -> Result<Option<SecondaryScaleDownCleanup>>;
    async fn record_secondary_removal_commit(
        &self,
        committed: &SecondaryScaleDownCleanup,
    ) -> Result<()>;
    async fn load_retired_authority(&self) -> Result<Option<RetiredAuthority>>;
    async fn load_retirement_started(&self) -> Result<Option<RetiredAuthority>>;
    async fn record_retirement_started(&self, authority: &RetiredAuthority) -> Result<()>;
    async fn retire(&self, authority: &RetiredAuthority) -> Result<()>;
}

#[doc(hidden)]
pub(crate) struct ManagedFenceGuard {
    _delivery: OwnedMutexGuard<()>,
    progress: NativeProgressStatus,
}

impl ManagedFenceGuard {
    pub(crate) fn new(delivery: OwnedMutexGuard<()>, progress: NativeProgressStatus) -> Self {
        Self {
            _delivery: delivery,
            progress,
        }
    }

    pub(crate) fn progress(&self) -> &NativeProgressStatus {
        &self.progress
    }
}

#[async_trait]
#[doc(hidden)]
pub(crate) trait ManagedReplicatorDataPlane: Send + Sync {
    async fn next_outbound_item(&self) -> Option<OutboundOperation>;
    async fn repair_peer(&self, identity: ReplicaIdentity, progress: Lsn) -> Result<()>;
    #[cfg(all(test, kuberic_workspace_tests))]
    async fn begin_write(&self, write: ClientWrite) -> Result<PendingWrite>;
    async fn observe_acknowledgement(
        &self,
        acknowledgement: ReplicationAck,
        session: ProcessSessionId,
    ) -> Result<()>;
    async fn accept_acknowledgement(&self, acknowledgement: ReplicationAck) -> Result<()>;
    async fn prepare_copy(&self, request: PrepareCopyRequest) -> Result<PreparedCopy>;
    async fn accept_copy_acknowledgement(&self, acknowledgement: CopyAck) -> Result<()>;
    async fn receive_copy_item(&self, item: CopyItem) -> Result<CopyAck>;
    async fn receive_replication(&self, item: ReplicationItem) -> Result<PendingReplication>;
    fn abort(&self);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplicaSetQuorumMode {
    WriteQuorum,
    All,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplicaInformation {
    pub build_id: OperationId,
    pub identity: ReplicaIdentity,
    pub replication_address: String,
    pub process_session_id: ProcessSessionId,
    pub role: ReplicaRole,
    pub current_progress: Lsn,
    pub catch_up_capability: Lsn,
    pub must_catchup: bool,
    pub storage_incarnation: Option<ReplicaStorageIncarnationContext>,
    pub build_authorization: Option<BuildAuthorizationV1>,
    pub peer_authorization: Option<PeerSessionAuthorizationV1>,
    pub configuration_revision: Option<u64>,
}

impl ReplicaInformation {
    /// Compatibility constructor for an idle build target. New code should use
    /// `idle_build` to make the invalid-progress choice explicit.
    pub fn new(
        build_id: OperationId,
        identity: ReplicaIdentity,
        replication_address: String,
    ) -> Self {
        Self::idle_build(build_id, identity, replication_address)
    }

    pub fn idle_build(
        build_id: OperationId,
        identity: ReplicaIdentity,
        replication_address: String,
    ) -> Self {
        Self {
            build_id,
            identity,
            replication_address,
            process_session_id: ProcessSessionId::default(),
            role: ReplicaRole::IdleSecondary,
            current_progress: INVALID_LSN,
            catch_up_capability: INVALID_LSN,
            must_catchup: false,
            storage_incarnation: None,
            build_authorization: None,
            peer_authorization: None,
            configuration_revision: None,
        }
    }

    pub fn active_peer(
        identity: ReplicaIdentity,
        replication_address: String,
        process_session_id: ProcessSessionId,
        current_progress: Lsn,
        catch_up_capability: Lsn,
        must_catchup: bool,
    ) -> Result<Self> {
        let information = Self {
            build_id: OperationId::default(),
            identity,
            replication_address,
            process_session_id,
            role: ReplicaRole::ActiveSecondary,
            current_progress,
            catch_up_capability,
            must_catchup,
            storage_incarnation: None,
            build_authorization: None,
            peer_authorization: None,
            configuration_revision: None,
        };
        information.validate_public_progress()?;
        Ok(information)
    }

    pub fn validate_public_progress(&self) -> Result<()> {
        crate::protocol::public_operations::validate_progress_value(self.current_progress)
            .map_err(|message| RuntimeError::Application(message.into()))?;
        crate::protocol::public_operations::validate_progress_value(self.catch_up_capability)
            .map_err(|message| RuntimeError::Application(message.into()))?;
        if self.role == ReplicaRole::IdleSecondary
            && (self.current_progress != INVALID_LSN
                || self.catch_up_capability != INVALID_LSN
                || self.must_catchup)
        {
            return Err(RuntimeError::Application(
                "idle public build target must use INVALID_LSN".into(),
            ));
        }
        self.validate_authorization_shape()?;
        Ok(())
    }

    pub fn validate_authorization_shape(&self) -> Result<()> {
        if self.build_authorization.is_some() && self.peer_authorization.is_some() {
            return Err(RuntimeError::Application(
                "build and peer authorization are structurally exclusive".into(),
            ));
        }
        match &self.build_authorization {
            Some(authorization) => {
                let storage = self.storage_incarnation.as_ref().ok_or_else(|| {
                    RuntimeError::Application(
                        "signed build target lacks its storage incarnation".into(),
                    )
                })?;
                if self.role != ReplicaRole::IdleSecondary
                    || self.current_progress != INVALID_LSN
                    || self.catch_up_capability != INVALID_LSN
                    || self.must_catchup
                    || self.build_id != authorization.claims.build_id
                    || self.identity != authorization.claims.target.storage_incarnation.replica
                    || storage != &authorization.claims.target.storage_incarnation
                    || self.process_session_id != authorization.claims.target.process_session_id
                    || self.replication_address != authorization.claims.target.replication_address
                    || authorization.claims.target.role != ReplicaRole::IdleSecondary
                    || self.configuration_revision.is_some()
                {
                    return Err(RuntimeError::Application(
                        "signed build authorization differs from the idle target descriptor".into(),
                    ));
                }
            }
            None if self.storage_incarnation.is_some() => {
                return Err(RuntimeError::Application(
                    "descriptor storage incarnation requires build authorization".into(),
                ));
            }
            None => {}
        }
        if let Some(authorization) = &self.peer_authorization {
            if self.role != ReplicaRole::ActiveSecondary
                || !self.build_id.is_empty()
                || self.storage_incarnation.is_some()
                || self.identity != authorization.claims.secondary.replica
                || self.process_session_id != authorization.claims.secondary.process_session_id
                || self.replication_address != authorization.claims.secondary.replication_address
                || authorization.claims.secondary.role != ReplicaRole::ActiveSecondary
                || self.configuration_revision != Some(authorization.claims.configuration_revision)
            {
                return Err(RuntimeError::Application(
                    "peer authorization differs from the active remote descriptor".into(),
                ));
            }
        } else if self.configuration_revision.is_some() {
            return Err(RuntimeError::Application(
                "configuration revision requires peer authorization".into(),
            ));
        }
        Ok(())
    }

    pub fn validate_build_authorization(
        &self,
        trust: &PublicAuthorizationTrust,
        history: &HistoryContext,
        epoch: Epoch,
        operation_id: &OperationId,
        source: &BuildAuthorizationEndpointV1,
    ) -> Result<()> {
        trust.validate()?;
        self.validate_authorization_shape()?;
        let authorization = self.build_authorization.as_ref().ok_or_else(|| {
            RuntimeError::Application("signed public build lacks authorization".into())
        })?;
        trust
            .verification_key
            .verify_build_authorization(authorization)
            .map_err(|error| RuntimeError::Application(error.to_string()))?;
        let claims = &authorization.claims;
        if claims.resource_uid != trust.resource_uid
            || claims.protocol_generation != trust.protocol_generation
            || claims.key_id != trust.key_id
            || &claims.history != history
            || claims.epoch != epoch
            || &claims.operation_id != operation_id
            || &claims.source != source
        {
            return Err(RuntimeError::Application(
                "signed build authorization differs from immutable local authority".into(),
            ));
        }
        Ok(())
    }
}

impl From<PublicReplicaDescriptor> for ReplicaInformation {
    fn from(replica: PublicReplicaDescriptor) -> Self {
        Self {
            build_id: replica.build_id,
            identity: replica.identity,
            replication_address: replica.replication_address,
            process_session_id: replica.process_session_id,
            role: replica.role,
            current_progress: replica.current_progress,
            catch_up_capability: replica.catch_up_capability,
            must_catchup: replica.must_catchup,
            storage_incarnation: None,
            build_authorization: None,
            peer_authorization: replica.peer_authorization,
            configuration_revision: replica.configuration_revision,
        }
    }
}

/// SF ReplicaSetConfiguration: voting policy plus exact replica descriptions.
/// Idle replicas may be described without belonging to the voting configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplicaSetConfiguration {
    pub configuration: ConfigurationDescriptor,
    pub replicas: Vec<ReplicaInformation>,
}

impl From<ConfigurationDescriptor> for ReplicaSetConfiguration {
    fn from(configuration: ConfigurationDescriptor) -> Self {
        Self {
            configuration,
            replicas: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicAuthorizationTrust {
    pub resource_uid: ResourceUid,
    pub protocol_generation: PublicProtocolGeneration,
    pub verification_key: AuthorizationVerificationKey,
    pub key_id: AuthorizationKeyId,
}

impl PublicAuthorizationTrust {
    pub fn validate(&self) -> Result<()> {
        if self.resource_uid.is_empty() || self.protocol_generation.value() == 0 {
            return Err(RuntimeError::Application(
                "public authorization trust lacks resource or generation".into(),
            ));
        }
        if self.key_id != self.verification_key.key_id() {
            return Err(RuntimeError::Application(
                "public authorization key ID does not match the verification key".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ReplicatorSettings {
    pub replication_address: String,
    pub public_authorization: Option<PublicAuthorizationTrust>,
}

impl ReplicatorSettings {
    fn validate(&self) -> Result<()> {
        if let Some(trust) = &self.public_authorization {
            trust.validate()?;
            if self.replication_address.is_empty() {
                return Err(RuntimeError::Application(
                    "signed public authorization requires an exact replication address".into(),
                ));
            }
        }
        Ok(())
    }
}

#[derive(Clone)]
struct RegisteredPublicAuthorizationSettings {
    owner: ReplicatorCreationIdentity,
    identity: ReplicaIdentity,
    trust: PublicAuthorizationTrust,
    replication_address: String,
    context: Option<PublicAuthorizationRuntimeContext>,
    peers: Vec<PeerSessionAuthorizationV1>,
    builds: Vec<BuildAuthorizationV1>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PublicAuthorizationRuntimeContext {
    pub(crate) identity: ReplicaIdentity,
    pub(crate) history: HistoryContext,
    pub(crate) storage_incarnation: ReplicaStorageIncarnationContext,
    pub(crate) process_session_id: ProcessSessionId,
    pub(crate) role: ReplicaRole,
    pub(crate) epoch: Epoch,
    pub(crate) previous_configuration_id: Option<ConfigurationId>,
    pub(crate) current_configuration_id: Option<ConfigurationId>,
}

#[derive(Clone)]
pub(crate) struct PublicPeerAuthorizationSnapshot {
    identity: ReplicaIdentity,
    context: Option<PublicAuthorizationRuntimeContext>,
    peers: Vec<PeerSessionAuthorizationV1>,
    builds: Vec<BuildAuthorizationV1>,
}

#[derive(Default)]
struct PublicAuthorizationRegistry {
    settings: Vec<RegisteredPublicAuthorizationSettings>,
}

fn public_authorization_registry() -> &'static StdMutex<PublicAuthorizationRegistry> {
    static REGISTRY: OnceLock<StdMutex<PublicAuthorizationRegistry>> = OnceLock::new();
    REGISTRY.get_or_init(|| StdMutex::new(PublicAuthorizationRegistry::default()))
}

fn registered_public_authorization_settings(
    identity: &ReplicaIdentity,
) -> Option<RegisteredPublicAuthorizationSettings> {
    public_authorization_registry()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .settings
        .iter()
        .find(|settings| &settings.identity == identity)
        .cloned()
}

fn register_public_authorization_settings(
    owner: ReplicatorCreationIdentity,
    identity: ReplicaIdentity,
    settings: &ReplicatorSettings,
) -> Result<()> {
    settings.validate()?;
    let Some(trust) = settings.public_authorization.clone() else {
        return Ok(());
    };
    let mut registry = public_authorization_registry()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(existing) = registry
        .settings
        .iter()
        .find(|registered| registered.identity == identity)
    {
        if existing.owner == owner
            && existing.trust == trust
            && existing.replication_address == settings.replication_address
        {
            return Ok(());
        }
        return Err(RuntimeError::Application(
            "replicator registration cannot replace immutable public authorization trust".into(),
        ));
    }
    registry
        .settings
        .push(RegisteredPublicAuthorizationSettings {
            owner,
            identity,
            trust,
            replication_address: settings.replication_address.clone(),
            context: None,
            peers: Vec::new(),
            builds: Vec::new(),
        });
    Ok(())
}

fn update_public_authorization_settings(
    owner: ReplicatorCreationIdentity,
    identity: &ReplicaIdentity,
    settings: &ReplicatorSettings,
) -> Result<()> {
    settings.validate()?;
    let mut registry = public_authorization_registry()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(registered) = registry
        .settings
        .iter_mut()
        .find(|registered| registered.identity == *identity && registered.owner == owner)
    else {
        return if settings.public_authorization.is_none() {
            Ok(())
        } else {
            Err(RuntimeError::Application(
                "signed public authorization settings are not registered".into(),
            ))
        };
    };
    if settings.public_authorization.as_ref() != Some(&registered.trust) {
        return Err(RuntimeError::Application(
            "replicator settings cannot change public authorization trust or generation".into(),
        ));
    }
    registered.replication_address = settings.replication_address.clone();
    Ok(())
}

fn unregister_public_authorization_settings(owner: ReplicatorCreationIdentity) {
    let mut registry = public_authorization_registry()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    registry.settings.retain(|settings| settings.owner != owner);
}

#[cfg(any(test, feature = "testing"))]
pub(crate) fn bind_public_authorization_context(
    context: PublicAuthorizationRuntimeContext,
) -> Result<()> {
    let Some(settings) = registered_public_authorization_settings(&context.identity) else {
        return Ok(());
    };
    if context.history.resource_uid != settings.trust.resource_uid
        || context.history.protocol_generation != settings.trust.protocol_generation
        || context.storage_incarnation.replica != context.identity
        || context.process_session_id.is_empty()
    {
        return Err(RuntimeError::Application(
            "public authorization runtime context differs from immutable trust".into(),
        ));
    }
    context
        .history
        .validate()
        .map_err(|message| RuntimeError::Application(message.into()))?;
    context
        .storage_incarnation
        .validate()
        .map_err(|message| RuntimeError::Application(message.into()))?;
    if context.epoch.data_loss_number < 0 || context.epoch.configuration_number < 0 {
        return Err(RuntimeError::Application(
            "public authorization runtime context lacks a valid current epoch".into(),
        ));
    }
    let mut registry = public_authorization_registry()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let registered = registry
        .settings
        .iter_mut()
        .find(|registered| registered.identity == context.identity)
        .ok_or_else(|| {
            RuntimeError::Application(
                "public authorization runtime context lacks registered trust".into(),
            )
        })?;
    registered.context = Some(context);
    Ok(())
}

#[cfg(any(test, feature = "testing"))]
pub(crate) fn advance_public_authorization_context(
    context: PublicAuthorizationRuntimeContext,
) -> Result<()> {
    let Some(settings) = registered_public_authorization_settings(&context.identity) else {
        return Ok(());
    };
    if context.history.resource_uid != settings.trust.resource_uid
        || context.history.protocol_generation != settings.trust.protocol_generation
        || context.storage_incarnation.replica != context.identity
        || context.process_session_id.is_empty()
        || context.epoch.data_loss_number < 0
        || context.epoch.configuration_number < 0
    {
        return Err(RuntimeError::Application(
            "advanced public authorization context differs from immutable trust".into(),
        ));
    }
    let mut registry = public_authorization_registry()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let registered = registry
        .settings
        .iter_mut()
        .find(|registered| registered.identity == context.identity)
        .ok_or_else(|| {
            RuntimeError::Application(
                "advanced public authorization context lacks registered trust".into(),
            )
        })?;
    registered.context = Some(context);
    registered.peers.clear();
    registered.builds.clear();
    Ok(())
}

#[cfg(any(test, feature = "testing"))]
pub(crate) fn clear_public_authorization_context(identity: &ReplicaIdentity) {
    let mut registry = public_authorization_registry()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(registered) = registry
        .settings
        .iter_mut()
        .find(|registered| registered.identity == *identity)
    {
        registered.context = None;
        registered.peers.clear();
        registered.builds.clear();
    }
}

#[cfg(any(test, feature = "testing"))]
pub(crate) fn admit_public_peer_configuration(
    context: &PublicAuthorizationRuntimeContext,
    configuration: &PublicConfiguration,
) -> Result<Option<PublicPeerAuthorizationSnapshot>> {
    configuration
        .validate_for_local(context.identity.replica_id)
        .map_err(|message| RuntimeError::Application(message.into()))?;
    let Some(settings) = registered_public_authorization_settings(&context.identity) else {
        if configuration
            .replicas
            .iter()
            .any(|replica| replica.peer_authorization.is_some())
        {
            return Err(RuntimeError::Application(
                "signed peer authorization lacks immutable runtime trust".into(),
            ));
        }
        return Ok(None);
    };
    if context.role != ReplicaRole::Primary
        || configuration.current.primary_id != context.identity.replica_id
    {
        return Err(RuntimeError::Application(
            "only the exact local primary can admit peer authorization".into(),
        ));
    }
    let previous_configuration_id = configuration
        .previous
        .as_ref()
        .map(|configuration| configuration.configuration_id.clone());
    let mut admitted = Vec::with_capacity(configuration.replicas.len());
    for replica in &configuration.replicas {
        let authorization = replica.peer_authorization.as_ref().ok_or_else(|| {
            RuntimeError::Application(
                "public-v1 active remote membership requires peer authorization".into(),
            )
        })?;
        settings
            .trust
            .verification_key
            .verify_peer_session_authorization(authorization)
            .map_err(|error| RuntimeError::Application(error.to_string()))?;
        let claims = &authorization.claims;
        if claims.resource_uid != settings.trust.resource_uid
            || claims.protocol_generation != settings.trust.protocol_generation
            || claims.key_id != settings.trust.key_id
            || claims.history != context.history
            || claims.epoch != configuration.current.epoch
            || claims.current_configuration_id != configuration.current.configuration_id
            || claims.previous_configuration_id != previous_configuration_id
            || claims.primary.replica != context.identity
            || claims.primary.process_session_id != context.process_session_id
            || claims.primary.replication_address != settings.replication_address
            || claims.primary.role != ReplicaRole::Primary
            || claims.secondary.replica != replica.identity
            || claims.secondary.process_session_id != replica.process_session_id
            || claims.secondary.replication_address != replica.replication_address
            || claims.secondary.role != ReplicaRole::ActiveSecondary
            || replica.configuration_revision != Some(claims.configuration_revision)
        {
            return Err(RuntimeError::Application(
                "peer authorization differs from current local configuration authority".into(),
            ));
        }
        admitted.push(authorization.clone());
    }
    let mut registry = public_authorization_registry()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let registered = registry
        .settings
        .iter_mut()
        .find(|registered| registered.identity == context.identity)
        .ok_or_else(|| {
            RuntimeError::Application(
                "public peer authorization lacks registered local trust".into(),
            )
        })?;
    let snapshot = PublicPeerAuthorizationSnapshot {
        identity: registered.identity.clone(),
        context: registered.context.clone(),
        peers: registered.peers.clone(),
        builds: registered.builds.clone(),
    };
    let mut current_context = context.clone();
    current_context.epoch = configuration.current.epoch;
    current_context.previous_configuration_id = previous_configuration_id;
    current_context.current_configuration_id = Some(configuration.current.configuration_id.clone());
    registered.context = Some(current_context);
    registered.peers = admitted;
    registered.builds.clear();
    Ok(Some(snapshot))
}

#[cfg(any(test, feature = "testing"))]
pub(crate) fn restore_public_peer_authorization(
    snapshot: PublicPeerAuthorizationSnapshot,
) -> Result<()> {
    let mut registry = public_authorization_registry()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let registered = registry
        .settings
        .iter_mut()
        .find(|registered| registered.identity == snapshot.identity)
        .ok_or_else(|| {
            RuntimeError::Application(
                "public peer authorization rollback lacks registered local trust".into(),
            )
        })?;
    registered.context = snapshot.context;
    registered.peers = snapshot.peers;
    registered.builds = snapshot.builds;
    Ok(())
}

#[cfg(any(test, feature = "testing"))]
pub(crate) fn authorize_public_build_descriptor(
    context: &PublicAuthorizationRuntimeContext,
    epoch: Epoch,
    operation_id: &OperationId,
    replica: &ReplicaInformation,
) -> Result<()> {
    let settings = registered_public_authorization_settings(&context.identity);
    match settings {
        None => {
            if replica.build_authorization.is_some() {
                return Err(RuntimeError::Application(
                    "signed build authorization lacks immutable runtime trust".into(),
                ));
            }
            return Ok(());
        }
        Some(settings) => {
            if context.role != ReplicaRole::Primary || context.epoch != epoch {
                return Err(RuntimeError::Application(
                    "only the exact current-epoch local primary can authorize a public build"
                        .into(),
                ));
            }
            let source = BuildAuthorizationEndpointV1 {
                storage_incarnation: context.storage_incarnation.clone(),
                process_session_id: context.process_session_id.clone(),
                replication_address: settings.replication_address,
                role: ReplicaRole::Primary,
            };
            replica.validate_build_authorization(
                &settings.trust,
                &context.history,
                epoch,
                operation_id,
                &source,
            )?;
        }
    }
    let authorization = replica
        .build_authorization
        .as_ref()
        .expect("validated signed build authorization")
        .clone();
    let mut registry = public_authorization_registry()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let registered = registry
        .settings
        .iter_mut()
        .find(|registered| registered.identity == context.identity)
        .ok_or_else(|| {
            RuntimeError::Application(
                "public build authorization lacks registered local trust".into(),
            )
        })?;
    registered.builds.retain(|existing| {
        existing.claims.source.replica() != authorization.claims.source.replica()
            || existing.claims.target.replica() != authorization.claims.target.replica()
            || existing.claims.build_id != authorization.claims.build_id
    });
    registered.builds.push(authorization);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn validate_peer_authorization_for_wire(
    local: &ReplicaIdentity,
    authorization: Option<&PeerSessionAuthorizationV1>,
    sender: &ReplicaIdentity,
    receiver: &ReplicaIdentity,
    epoch: Epoch,
    previous_configuration_id: Option<&ConfigurationId>,
    current_configuration_id: &ConfigurationId,
    sender_session: &ProcessSessionId,
    receiver_session: &ProcessSessionId,
) -> Result<()> {
    let settings = registered_public_authorization_settings(local);
    let Some(settings) = settings else {
        return if authorization.is_none() {
            Ok(())
        } else {
            Err(RuntimeError::InvalidReplication(
                "signed peer authorization lacks immutable local trust".into(),
            ))
        };
    };
    let authorization = authorization.ok_or_else(|| {
        RuntimeError::InvalidReplication("public-v1 replication requires peer authorization".into())
    })?;
    settings
        .trust
        .verification_key
        .verify_peer_session_authorization(authorization)
        .map_err(|error| RuntimeError::InvalidReplication(error.to_string()))?;
    let claims = &authorization.claims;
    if claims.resource_uid != settings.trust.resource_uid
        || claims.protocol_generation != settings.trust.protocol_generation
        || claims.key_id != settings.trust.key_id
        || claims.epoch != epoch
        || claims.current_configuration_id != *current_configuration_id
        || claims.previous_configuration_id.as_ref() != previous_configuration_id
        || claims.primary.replica != *sender
        || claims.secondary.replica != *receiver
        || claims.primary.process_session_id != *sender_session
        || claims.secondary.process_session_id != *receiver_session
    {
        return Err(RuntimeError::InvalidReplication(
            "peer authorization differs from the exact replication envelope".into(),
        ));
    }
    let local_endpoint = if local == sender {
        &claims.primary
    } else if local == receiver {
        &claims.secondary
    } else {
        return Err(RuntimeError::InvalidReplication(
            "peer authorization does not contain the local identity".into(),
        ));
    };
    if local_endpoint.replication_address != settings.replication_address {
        return Err(RuntimeError::InvalidReplication(
            "peer authorization changed the local replication address".into(),
        ));
    }
    let context = settings.context.as_ref().ok_or_else(|| {
        RuntimeError::InvalidReplication(
            "signed peer authorization lacks current local runtime authority".into(),
        )
    })?;
    let expected_role = if local == sender {
        ReplicaRole::Primary
    } else {
        ReplicaRole::ActiveSecondary
    };
    if context.identity != *local
        || context.history != claims.history
        || context.epoch != claims.epoch
        || context.previous_configuration_id.as_ref() != claims.previous_configuration_id.as_ref()
        || context.current_configuration_id.as_ref() != Some(&claims.current_configuration_id)
        || context.process_session_id != local_endpoint.process_session_id
        || context.role != expected_role
        || local_endpoint.role != expected_role
    {
        return Err(RuntimeError::InvalidReplication(
            "peer authorization differs from current local role, epoch, configuration, history, or process session"
                .into(),
        ));
    }
    Ok(())
}

pub(crate) fn outbound_peer_authorization(
    sender: &ReplicaIdentity,
    receiver: &ReplicaIdentity,
    epoch: Epoch,
    previous_configuration_id: Option<&ConfigurationId>,
    current_configuration_id: &ConfigurationId,
    sender_session: &ProcessSessionId,
    receiver_session: &ProcessSessionId,
) -> Result<Option<Vec<u8>>> {
    let Some(_) = registered_public_authorization_settings(sender) else {
        return Ok(None);
    };
    let authorization = public_authorization_registry()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .settings
        .iter()
        .find(|settings| settings.identity == *sender)
        .and_then(|settings| {
            settings
                .peers
                .iter()
                .rev()
                .find(|authorization| {
                    authorization.claims.primary.replica == *sender
                        && authorization.claims.secondary.replica == *receiver
                        && authorization.claims.epoch == epoch
                        && authorization.claims.current_configuration_id
                            == *current_configuration_id
                        && authorization.claims.previous_configuration_id.as_ref()
                            == previous_configuration_id
                })
                .cloned()
        })
        .ok_or_else(|| {
            RuntimeError::InvalidReplication(
                "public-v1 replication lacks an admitted peer authorization".into(),
            )
        })?;
    validate_peer_authorization_for_wire(
        sender,
        Some(&authorization),
        sender,
        receiver,
        epoch,
        previous_configuration_id,
        current_configuration_id,
        sender_session,
        receiver_session,
    )?;
    authorization
        .canonical_bytes()
        .map(Some)
        .map_err(|error| RuntimeError::InvalidReplication(error.to_string()))
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn verify_inbound_peer_authorization(
    local: &ReplicaIdentity,
    authorization: Option<PeerSessionAuthorizationV1>,
    sender: &ReplicaIdentity,
    receiver: &ReplicaIdentity,
    epoch: Epoch,
    previous_configuration_id: Option<&ConfigurationId>,
    current_configuration_id: &ConfigurationId,
    sender_session: &ProcessSessionId,
    receiver_session: &ProcessSessionId,
) -> Result<()> {
    validate_peer_authorization_for_wire(
        local,
        authorization.as_ref(),
        sender,
        receiver,
        epoch,
        previous_configuration_id,
        current_configuration_id,
        sender_session,
        receiver_session,
    )?;
    if let Some(authorization) = authorization {
        let mut registry = public_authorization_registry()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let registered = registry
            .settings
            .iter_mut()
            .find(|registered| registered.identity == *local)
            .ok_or_else(|| {
                RuntimeError::InvalidReplication(
                    "inbound peer authorization lacks registered local trust".into(),
                )
            })?;
        registered.peers.retain(|existing| {
            existing.claims.primary.replica != authorization.claims.primary.replica
                || existing.claims.secondary.replica != authorization.claims.secondary.replica
                || existing.claims.current_configuration_id
                    != authorization.claims.current_configuration_id
                || existing.claims.previous_configuration_id
                    != authorization.claims.previous_configuration_id
        });
        registered.peers.push(authorization);
    }
    Ok(())
}

pub(crate) fn response_peer_authorization(
    sender: &ReplicaIdentity,
    receiver: &ReplicaIdentity,
    epoch: Epoch,
    previous_configuration_id: Option<&ConfigurationId>,
    current_configuration_id: &ConfigurationId,
) -> Result<Option<Vec<u8>>> {
    let local = if registered_public_authorization_settings(sender).is_some() {
        sender
    } else if registered_public_authorization_settings(receiver).is_some() {
        receiver
    } else {
        return Ok(None);
    };
    let authorization = public_authorization_registry()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .settings
        .iter()
        .find(|settings| settings.identity == *local)
        .and_then(|settings| {
            settings.peers.iter().rev().find(|authorization| {
                authorization.claims.primary.replica == *sender
                    && authorization.claims.secondary.replica == *receiver
                    && authorization.claims.epoch == epoch
                    && authorization.claims.current_configuration_id == *current_configuration_id
                    && authorization.claims.previous_configuration_id.as_ref()
                        == previous_configuration_id
            })
        })
        .cloned()
        .ok_or_else(|| {
            RuntimeError::InvalidReplication(
                "public-v1 acknowledgement lacks peer authorization".into(),
            )
        })?;
    validate_peer_authorization_for_wire(
        local,
        Some(&authorization),
        sender,
        receiver,
        epoch,
        previous_configuration_id,
        current_configuration_id,
        &authorization.claims.primary.process_session_id,
        &authorization.claims.secondary.process_session_id,
    )?;
    authorization
        .canonical_bytes()
        .map(Some)
        .map_err(|error| RuntimeError::InvalidReplication(error.to_string()))
}

#[allow(clippy::too_many_arguments)]
fn validate_build_authorization_for_wire(
    local: &ReplicaIdentity,
    authorization: Option<&BuildAuthorizationV1>,
    resource_uid: Option<&ResourceUid>,
    operation_id: &OperationId,
    sender: &ReplicaIdentity,
    receiver: &ReplicaIdentity,
    epoch: Epoch,
    sender_session: &ProcessSessionId,
    receiver_session: &ProcessSessionId,
) -> Result<()> {
    let settings = registered_public_authorization_settings(local);
    let Some(settings) = settings else {
        return if authorization.is_none() {
            Ok(())
        } else {
            Err(RuntimeError::InvalidReplication(
                "signed build authorization lacks immutable local trust".into(),
            ))
        };
    };
    let authorization = authorization.ok_or_else(|| {
        RuntimeError::InvalidReplication("public-v1 copy requires build authorization".into())
    })?;
    settings
        .trust
        .verification_key
        .verify_build_authorization(authorization)
        .map_err(|error| RuntimeError::InvalidReplication(error.to_string()))?;
    let claims = &authorization.claims;
    if claims.resource_uid != settings.trust.resource_uid
        || resource_uid.is_some_and(|resource_uid| resource_uid != &claims.resource_uid)
        || claims.protocol_generation != settings.trust.protocol_generation
        || claims.key_id != settings.trust.key_id
        || claims.epoch != epoch
        || claims.operation_id != *operation_id
        || claims.build_id != *operation_id
        || claims.source.storage_incarnation.replica != *sender
        || claims.target.storage_incarnation.replica != *receiver
        || claims.source.process_session_id != *sender_session
        || claims.target.process_session_id != *receiver_session
    {
        return Err(RuntimeError::InvalidReplication(
            "build authorization differs from the exact copy envelope".into(),
        ));
    }
    let local_endpoint = if local == sender {
        &claims.source
    } else if local == receiver {
        &claims.target
    } else {
        return Err(RuntimeError::InvalidReplication(
            "build authorization does not contain the local identity".into(),
        ));
    };
    if local_endpoint.replication_address != settings.replication_address {
        return Err(RuntimeError::InvalidReplication(
            "build authorization changed the local replication address".into(),
        ));
    }
    let context = settings.context.as_ref().ok_or_else(|| {
        RuntimeError::InvalidReplication(
            "signed build authorization lacks current local runtime authority".into(),
        )
    })?;
    if context.identity != *local
        || context.history != claims.history
        || context.epoch != claims.epoch
        || context.storage_incarnation != local_endpoint.storage_incarnation
        || context.process_session_id != local_endpoint.process_session_id
        || context.role != local_endpoint.role
    {
        return Err(RuntimeError::InvalidReplication(
            "build authorization differs from current local role, epoch, history, storage, or process session"
                .into(),
        ));
    }
    Ok(())
}

pub(crate) fn outbound_build_authorization(
    operation_id: &OperationId,
    sender: &ReplicaIdentity,
    receiver: &ReplicaIdentity,
    epoch: Epoch,
    sender_session: &ProcessSessionId,
    receiver_session: &ProcessSessionId,
) -> Result<Option<Vec<u8>>> {
    let Some(_) = registered_public_authorization_settings(sender) else {
        return Ok(None);
    };
    let authorization = public_authorization_registry()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .settings
        .iter()
        .find(|settings| settings.identity == *sender)
        .and_then(|settings| {
            settings.builds.iter().rev().find(|authorization| {
                authorization.claims.build_id == *operation_id
                    && authorization.claims.source.storage_incarnation.replica == *sender
                    && authorization.claims.target.storage_incarnation.replica == *receiver
                    && authorization.claims.epoch == epoch
            })
        })
        .cloned()
        .ok_or_else(|| {
            RuntimeError::InvalidReplication(
                "public-v1 copy lacks an admitted build authorization".into(),
            )
        })?;
    validate_build_authorization_for_wire(
        sender,
        Some(&authorization),
        None,
        operation_id,
        sender,
        receiver,
        epoch,
        sender_session,
        receiver_session,
    )?;
    authorization
        .canonical_bytes()
        .map(Some)
        .map_err(|error| RuntimeError::InvalidReplication(error.to_string()))
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn verify_inbound_build_authorization(
    local: &ReplicaIdentity,
    authorization: Option<BuildAuthorizationV1>,
    resource_uid: Option<&ResourceUid>,
    operation_id: &OperationId,
    sender: &ReplicaIdentity,
    receiver: &ReplicaIdentity,
    epoch: Epoch,
    sender_session: &ProcessSessionId,
    receiver_session: &ProcessSessionId,
) -> Result<()> {
    validate_build_authorization_for_wire(
        local,
        authorization.as_ref(),
        resource_uid,
        operation_id,
        sender,
        receiver,
        epoch,
        sender_session,
        receiver_session,
    )?;
    if let Some(authorization) = authorization {
        let mut registry = public_authorization_registry()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let registered = registry
            .settings
            .iter_mut()
            .find(|registered| registered.identity == *local)
            .ok_or_else(|| {
                RuntimeError::InvalidReplication(
                    "inbound build authorization lacks registered local trust".into(),
                )
            })?;
        registered.builds.retain(|existing| {
            existing.claims.build_id != authorization.claims.build_id
                || existing.claims.source.replica() != authorization.claims.source.replica()
                || existing.claims.target.replica() != authorization.claims.target.replica()
        });
        registered.builds.push(authorization);
    }
    Ok(())
}

pub(crate) fn response_build_authorization(
    operation_id: &OperationId,
    sender: &ReplicaIdentity,
    receiver: &ReplicaIdentity,
    epoch: Epoch,
) -> Result<Option<Vec<u8>>> {
    let local = if registered_public_authorization_settings(sender).is_some() {
        sender
    } else if registered_public_authorization_settings(receiver).is_some() {
        receiver
    } else {
        return Ok(None);
    };
    let authorization = public_authorization_registry()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .settings
        .iter()
        .find(|settings| settings.identity == *local)
        .and_then(|settings| {
            settings.builds.iter().rev().find(|authorization| {
                authorization.claims.build_id == *operation_id
                    && authorization.claims.source.storage_incarnation.replica == *sender
                    && authorization.claims.target.storage_incarnation.replica == *receiver
                    && authorization.claims.epoch == epoch
            })
        })
        .cloned()
        .ok_or_else(|| {
            RuntimeError::InvalidReplication(
                "public-v1 copy acknowledgement lacks build authorization".into(),
            )
        })?;
    validate_build_authorization_for_wire(
        local,
        Some(&authorization),
        None,
        operation_id,
        sender,
        receiver,
        epoch,
        &authorization.claims.source.process_session_id,
        &authorization.claims.target.process_session_id,
    )?;
    authorization
        .canonical_bytes()
        .map(Some)
        .map_err(|error| RuntimeError::InvalidReplication(error.to_string()))
}

pub(crate) fn has_public_authorization_trust(identity: &ReplicaIdentity) -> bool {
    registered_public_authorization_settings(identity).is_some()
}

#[cfg(test)]
pub(crate) fn testing_register_public_authorization(
    identity: ReplicaIdentity,
    replication_address: String,
    trust: PublicAuthorizationTrust,
    context: Option<PublicAuthorizationRuntimeContext>,
) -> Result<()> {
    let owner = ReplicatorCreationIdentity::new(RuntimeHostToken::new());
    register_public_authorization_settings(
        owner,
        identity,
        &ReplicatorSettings {
            replication_address,
            public_authorization: Some(trust),
        },
    )?;
    if let Some(context) = context {
        bind_public_authorization_context(context)?;
    }
    Ok(())
}

/// Rust's explicit counterpart of obtaining coherent interfaces from CreateReplicator.
pub struct ReplicatorInterfaces {
    replicator: Arc<dyn Replicator>,
    state_replicator: Option<Arc<dyn StateReplicator>>,
    primary_replicator: Option<Arc<dyn PrimaryReplicator>>,
    creation: Arc<ReplicatorCreation>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReplicatorCapabilities {
    pub catch_up_specific_quorum: bool,
    pub data_loss_replay: DataLossReplay,
}

impl ReplicatorCapabilities {
    pub const fn conservative() -> Self {
        Self {
            catch_up_specific_quorum: false,
            data_loss_replay: DataLossReplay::ReplaceOnAmbiguity,
        }
    }
}

#[derive(Clone)]
struct ManagedReplicatorCapabilities {
    lifecycle: Arc<dyn ManagedReplicatorLifecycle>,
    data_plane: Arc<dyn ManagedReplicatorDataPlane>,
}

#[derive(Clone)]
struct BoundReplicatorSettings {
    identity: ReplicaIdentity,
    owner: ReplicatorCreationIdentity,
    settings: ReplicatorSettings,
}

struct ReplicatorCreation {
    identity: OnceLock<ReplicatorCreationIdentity>,
    guarded_control: StdMutex<Arc<dyn Replicator>>,
    capabilities: ReplicatorCapabilities,
    managed: Option<ManagedReplicatorCapabilities>,
    settings: StdMutex<Option<BoundReplicatorSettings>>,
    armed: AtomicBool,
}

impl ReplicatorCreation {
    fn new(
        guarded_control: Arc<dyn Replicator>,
        identity: Option<ReplicatorCreationIdentity>,
        capabilities: ReplicatorCapabilities,
        managed: Option<ManagedReplicatorCapabilities>,
    ) -> Self {
        let creation_identity = OnceLock::new();
        if let Some(identity) = identity {
            creation_identity
                .set(identity)
                .expect("new creation identity is unset");
        }
        Self {
            identity: creation_identity,
            guarded_control: StdMutex::new(guarded_control),
            capabilities,
            managed,
            settings: StdMutex::new(None),
            armed: AtomicBool::new(true),
        }
    }

    fn bind_identity(&self, expected: ReplicatorCreationIdentity) -> Result<()> {
        if let Some(actual) = self.identity.get() {
            if *actual != expected {
                return Err(RuntimeError::Application(
                    "replicator capability creation identity does not match reservation".into(),
                ));
            }
            return Ok(());
        }
        self.identity.set(expected).map_err(|actual| {
            RuntimeError::Application(format!(
                "replicator capability creation identity was concurrently bound to {actual:?}"
            ))
        })
    }

    fn replace_guarded_control(&self, control: Arc<dyn Replicator>) {
        let mut guarded = self
            .guarded_control
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *guarded = control;
    }

    fn bind_settings(&self, identity: ReplicaIdentity, settings: ReplicatorSettings) -> Result<()> {
        settings.validate()?;
        let owner = *self
            .identity
            .get()
            .ok_or_else(|| RuntimeError::Application("replicator identity is not bound".into()))?;
        let mut bound = self
            .settings
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(existing) = bound.as_ref() {
            if existing.identity != identity
                || existing.owner != owner
                || existing.settings.public_authorization != settings.public_authorization
            {
                return Err(RuntimeError::Application(
                    "replicator registration cannot change identity or public trust".into(),
                ));
            }
            return Ok(());
        }
        register_public_authorization_settings(owner, identity.clone(), &settings)?;
        *bound = Some(BoundReplicatorSettings {
            identity,
            owner,
            settings,
        });
        Ok(())
    }

    fn validate_settings_update(&self, settings: &ReplicatorSettings) -> Result<()> {
        settings.validate()?;
        let bound = self
            .settings
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let existing = bound.as_ref().ok_or_else(|| {
            RuntimeError::Application("replicator settings are not registration-bound".into())
        })?;
        if existing.settings.public_authorization != settings.public_authorization {
            return Err(RuntimeError::Application(
                "replicator settings cannot change trust or protocol generation".into(),
            ));
        }
        Ok(())
    }

    fn complete_settings_update(&self, settings: ReplicatorSettings) -> Result<()> {
        let mut bound = self
            .settings
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let existing = bound.as_mut().ok_or_else(|| {
            RuntimeError::Application("replicator settings are not registration-bound".into())
        })?;
        update_public_authorization_settings(existing.owner, &existing.identity, &settings)?;
        existing.settings = settings;
        Ok(())
    }
}

impl Drop for ReplicatorCreation {
    fn drop(&mut self) {
        if let Some(settings) = self
            .settings
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
        {
            unregister_public_authorization_settings(settings.owner);
        }
        if !self.armed.load(Ordering::Acquire) {
            return;
        }
        let control = self
            .guarded_control
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        control.abort();
        if let Some(managed) = self.managed.as_ref() {
            managed.lifecycle.abort();
            managed.data_plane.abort();
        }
    }
}

struct ProtectedStateReplicator {
    inner: Arc<dyn StateReplicator>,
    creation: Arc<ReplicatorCreation>,
}

#[async_trait]
impl StateReplicator for ProtectedStateReplicator {
    async fn replicate(&self, data: OperationData) -> Result<Lsn> {
        self.inner.replicate(data).await
    }

    async fn get_replication_stream(&self) -> Result<OperationStream> {
        self.inner.get_replication_stream().await
    }

    async fn get_copy_stream(&self) -> Result<OperationStream> {
        self.inner.get_copy_stream().await
    }

    async fn update_replicator_settings(&self, settings: ReplicatorSettings) -> Result<()> {
        self.creation.validate_settings_update(&settings)?;
        self.inner
            .update_replicator_settings(settings.clone())
            .await?;
        self.creation.complete_settings_update(settings)
    }
}

fn protect_state_replicator(
    state_replicator: Option<Arc<dyn StateReplicator>>,
    creation: &Arc<ReplicatorCreation>,
) -> Option<Arc<dyn StateReplicator>> {
    state_replicator.map(|inner| {
        Arc::new(ProtectedStateReplicator {
            inner,
            creation: creation.clone(),
        }) as Arc<dyn StateReplicator>
    })
}

#[doc(hidden)]
pub(crate) struct ReplicatorAttachment {
    creation: Arc<ReplicatorCreation>,
    replicator: Arc<dyn Replicator>,
    primary_replicator: Option<Arc<dyn PrimaryReplicator>>,
}

#[doc(hidden)]
impl ReplicatorAttachment {
    pub(crate) fn identity(&self, _token: RuntimeHostToken) -> ReplicatorCreationIdentity {
        *self
            .creation
            .identity
            .get()
            .expect("replicator attachment identity is bound")
    }

    pub(crate) fn managed_lifecycle(
        &self,
        _token: RuntimeHostToken,
    ) -> Option<Arc<dyn ManagedReplicatorLifecycle>> {
        self.creation
            .managed
            .as_ref()
            .map(|managed| managed.lifecycle.clone())
    }

    pub(crate) fn managed_data_plane(
        &self,
        _token: RuntimeHostToken,
    ) -> Option<Arc<dyn ManagedReplicatorDataPlane>> {
        self.creation
            .managed
            .as_ref()
            .map(|managed| managed.data_plane.clone())
    }

    pub(crate) fn replicator(&self, _token: RuntimeHostToken) -> Arc<dyn Replicator> {
        self.replicator.clone()
    }

    pub(crate) fn primary_replicator(
        &self,
        _token: RuntimeHostToken,
    ) -> Option<Arc<dyn PrimaryReplicator>> {
        self.primary_replicator.clone()
    }

    pub(crate) fn capabilities(&self, _token: RuntimeHostToken) -> ReplicatorCapabilities {
        self.creation.capabilities
    }

    fn disarm(&self) {
        self.creation.armed.store(false, Ordering::Release);
    }

    #[cfg(all(test, feature = "testing"))]
    pub(crate) fn testing_disarm(&self, _token: RuntimeHostToken) {
        self.disarm();
    }
}

impl ReplicatorInterfaces {
    pub fn secondary(
        replicator: Arc<dyn Replicator>,
        state_replicator: Option<Arc<dyn StateReplicator>>,
        capabilities: ReplicatorCapabilities,
    ) -> Self {
        let creation = Arc::new(ReplicatorCreation::new(
            replicator.clone(),
            None,
            capabilities,
            None,
        ));
        let state_replicator = protect_state_replicator(state_replicator, &creation);
        Self {
            replicator,
            state_replicator,
            primary_replicator: None,
            creation,
        }
    }

    pub fn primary<T>(
        primary_replicator: Arc<T>,
        state_replicator: Option<Arc<dyn StateReplicator>>,
        capabilities: ReplicatorCapabilities,
    ) -> Self
    where
        T: PrimaryReplicator + 'static,
    {
        let replicator: Arc<dyn Replicator> = primary_replicator.clone();
        let primary_replicator: Arc<dyn PrimaryReplicator> = primary_replicator;
        let creation = Arc::new(ReplicatorCreation::new(
            replicator.clone(),
            None,
            capabilities,
            None,
        ));
        let state_replicator = protect_state_replicator(state_replicator, &creation);
        Self {
            replicator,
            state_replicator,
            primary_replicator: Some(primary_replicator),
            creation,
        }
    }

    fn managed_primary<T, M>(
        identity: ReplicatorCreationIdentity,
        primary_replicator: Arc<T>,
        state_replicator: Option<Arc<dyn StateReplicator>>,
        capabilities: ReplicatorCapabilities,
        managed: Arc<M>,
    ) -> Self
    where
        T: PrimaryReplicator + 'static,
        M: ManagedReplicatorLifecycle + ManagedReplicatorDataPlane + 'static,
    {
        let replicator: Arc<dyn Replicator> = primary_replicator.clone();
        let primary_replicator: Arc<dyn PrimaryReplicator> = primary_replicator;
        let lifecycle: Arc<dyn ManagedReplicatorLifecycle> = managed.clone();
        let data_plane: Arc<dyn ManagedReplicatorDataPlane> = managed;
        let creation = Arc::new(ReplicatorCreation::new(
            replicator.clone(),
            Some(identity),
            capabilities,
            Some(ManagedReplicatorCapabilities {
                lifecycle,
                data_plane,
            }),
        ));
        let state_replicator = protect_state_replicator(state_replicator, &creation);
        Self {
            replicator,
            state_replicator,
            primary_replicator: Some(primary_replicator),
            creation,
        }
    }

    #[cfg(all(test, feature = "testing"))]
    #[doc(hidden)]
    pub(crate) fn testing_managed_primary<T, M>(
        _token: RuntimeHostToken,
        identity: ReplicatorCreationIdentity,
        primary_replicator: Arc<T>,
        state_replicator: Option<Arc<dyn StateReplicator>>,
        capabilities: ReplicatorCapabilities,
        managed: Arc<M>,
    ) -> Self
    where
        T: PrimaryReplicator + 'static,
        M: ManagedReplicatorLifecycle + ManagedReplicatorDataPlane + 'static,
    {
        Self::managed_primary(
            identity,
            primary_replicator,
            state_replicator,
            capabilities,
            managed,
        )
    }

    /// Replaces the public primary interfaces while preserving this bundle's
    /// opaque creation identity and optional host-only capabilities.
    pub fn wrap_primary<T>(
        self,
        primary_replicator: Arc<T>,
        state_replicator: Option<Arc<dyn StateReplicator>>,
    ) -> Self
    where
        T: PrimaryReplicator + 'static,
    {
        let replicator: Arc<dyn Replicator> = primary_replicator.clone();
        let primary_replicator: Arc<dyn PrimaryReplicator> = primary_replicator;
        self.creation.replace_guarded_control(replicator.clone());
        let state_replicator = protect_state_replicator(state_replicator, &self.creation);
        Self {
            replicator,
            state_replicator,
            primary_replicator: Some(primary_replicator),
            creation: self.creation.clone(),
        }
    }

    pub fn replicator(&self) -> Arc<dyn Replicator> {
        self.replicator.clone()
    }

    pub fn state_replicator(&self) -> Option<Arc<dyn StateReplicator>> {
        self.state_replicator.clone()
    }

    pub fn primary_replicator(&self) -> Option<Arc<dyn PrimaryReplicator>> {
        self.primary_replicator.clone()
    }

    pub fn capabilities(&self) -> ReplicatorCapabilities {
        self.creation.capabilities
    }

    fn prepare_attachment(
        &self,
        reservation: ReplicatorCreationReservation,
    ) -> Result<ReplicatorAttachment> {
        self.creation
            .bind_identity(reservation.identity(RuntimeHostToken::new()))?;
        Ok(ReplicatorAttachment {
            creation: self.creation.clone(),
            replicator: self.replicator(),
            primary_replicator: self.primary_replicator(),
        })
    }

    #[cfg(all(test, feature = "testing"))]
    #[doc(hidden)]
    pub(crate) fn testing_prepare_attachment(
        &self,
        _token: RuntimeHostToken,
        reservation: ReplicatorCreationReservation,
    ) -> Result<ReplicatorAttachment> {
        self.prepare_attachment(reservation)
    }
}

#[derive(Clone)]
pub struct ReplicatorFactoryContext {
    identity: ReplicaIdentity,
    access: Arc<dyn PartitionAccessView>,
    reservation: Option<ReplicatorCreationReservation>,
    pub(crate) default_dependencies: Option<DefaultReplicatorDependencies>,
}

impl ReplicatorFactoryContext {
    #[doc(hidden)]
    pub(crate) fn new(
        _token: RuntimeHostToken,
        identity: ReplicaIdentity,
        access: Arc<dyn PartitionAccessView>,
        default_dependencies: DefaultReplicatorDependencies,
    ) -> Self {
        Self {
            identity,
            access,
            reservation: None,
            default_dependencies: Some(default_dependencies),
        }
    }

    pub fn identity(&self) -> Result<ReplicaIdentity> {
        Ok(self.identity.clone())
    }

    pub async fn write_status(&self) -> Result<AccessStatus> {
        self.access.write_status().await
    }

    pub async fn read_status(&self) -> Result<AccessStatus> {
        self.access.read_status().await
    }

    pub fn partition_information(&self) -> PartitionInformation {
        self.access.partition_information()
    }

    pub async fn report_load(&self, metrics: Vec<LoadMetric>) -> Result<()> {
        self.access.report_load(metrics).await
    }

    pub async fn report_fault(&self, fault: FaultType) -> Result<()> {
        self.access.report_fault(fault).await
    }

    fn for_creation(&self, reservation: ReplicatorCreationReservation) -> Self {
        let mut context = self.clone();
        context.reservation = Some(reservation);
        context
    }

    fn creation_identity(&self) -> Result<ReplicatorCreationIdentity> {
        self.reservation
            .map(|reservation| reservation.identity(RuntimeHostToken::new()))
            .ok_or_else(|| RuntimeError::Application("replicator creation is not reserved".into()))
    }
}

#[async_trait]
#[doc(hidden)]
pub(crate) trait PartitionAccessView: Send + Sync {
    fn partition_information(&self) -> PartitionInformation;
    async fn read_status(&self) -> Result<AccessStatus>;

    async fn write_status(&self) -> Result<AccessStatus>;

    async fn report_load(&self, metrics: Vec<LoadMetric>) -> Result<()>;

    async fn report_fault(&self, fault: FaultType) -> Result<()>;
}

#[doc(hidden)]
#[derive(Clone)]
pub(crate) struct DefaultReplicatorDependencies {
    pub(crate) replica_authority_store: Arc<dyn ReplicaAuthorityStore>,
    pub(crate) managed_store: Arc<dyn ManagedReplicaStore>,
    pub(crate) replication_progress_store: Arc<dyn ReplicationProgressStore>,
    pub(crate) local_write_journal: Arc<dyn LocalWriteJournal>,
    pub(crate) build_authority_store: Arc<dyn BuildAuthorityStore>,
    pub(crate) build_progress_store: Arc<dyn BuildProgressStore>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[doc(hidden)]
pub(crate) struct ReplicatorCreationReservation(ReplicatorCreationIdentity);

#[doc(hidden)]
impl ReplicatorCreationReservation {
    pub(crate) fn new(_token: RuntimeHostToken) -> Self {
        Self(ReplicatorCreationIdentity::new(RuntimeHostToken::new()))
    }

    pub(crate) fn identity(&self, _token: RuntimeHostToken) -> ReplicatorCreationIdentity {
        self.0
    }
}

#[async_trait]
#[doc(hidden)]
pub(crate) trait ReplicatorRegistration: Send + Sync {
    fn reserve_replicator_creation(&self) -> Result<ReplicatorCreationReservation>;

    fn cancel_replicator_creation(&self, reservation: ReplicatorCreationReservation);

    async fn register_interfaces(
        &self,
        attachment: &ReplicatorAttachment,
        provider: Option<Arc<dyn StateProvider>>,
        reservation: ReplicatorCreationReservation,
    ) -> Result<()>;
}

#[async_trait]
pub trait ReplicatorFactory: Send + Sync {
    async fn create_replicator(
        &self,
        context: ReplicatorFactoryContext,
        state_provider: Option<Arc<dyn StateProvider>>,
        settings: ReplicatorSettings,
    ) -> Result<ReplicatorInterfaces>;
}

#[derive(Clone)]
pub struct StatefulServicePartition {
    context: ReplicatorFactoryContext,
    registration: Arc<dyn ReplicatorRegistration>,
    factory: Option<Arc<dyn ReplicatorFactory>>,
}

impl StatefulServicePartition {
    #[doc(hidden)]
    pub(crate) fn new(
        _token: RuntimeHostToken,
        registration: Arc<dyn ReplicatorRegistration>,
        context: ReplicatorFactoryContext,
    ) -> Self {
        Self {
            context,
            registration,
            factory: None,
        }
    }

    /// Select an implementation during service Open, not in the PodRuntime constructor.
    pub fn with_factory(&self, factory: Arc<dyn ReplicatorFactory>) -> Self {
        Self {
            context: self.context.clone(),
            registration: self.registration.clone(),
            factory: Some(factory),
        }
    }

    pub async fn get_write_status(&self) -> Result<AccessStatus> {
        self.context.write_status().await
    }

    pub async fn get_read_status(&self) -> Result<AccessStatus> {
        self.context.read_status().await
    }

    pub fn get_partition_information(&self) -> PartitionInformation {
        self.context.partition_information()
    }

    pub async fn report_load(&self, metrics: Vec<LoadMetric>) -> Result<()> {
        self.context.report_load(metrics).await
    }

    pub async fn report_fault(&self, fault: FaultType) -> Result<()> {
        self.context.report_fault(fault).await
    }

    pub async fn create_replicator(
        &self,
        state_provider: Option<Arc<dyn StateProvider>>,
        settings: Option<ReplicatorSettings>,
    ) -> Result<ReplicatorInterfaces> {
        let factory = self.factory.as_ref().ok_or_else(|| {
            RuntimeError::Application("select a replicator factory during Open".into())
        })?;
        let reservation = self.registration.reserve_replicator_creation()?;
        let context = self.context.for_creation(reservation);
        let settings = settings.unwrap_or_default();
        let interfaces = match factory
            .create_replicator(context, state_provider.clone(), settings.clone())
            .await
        {
            Ok(interfaces) => interfaces,
            Err(error) => {
                self.registration.cancel_replicator_creation(reservation);
                return Err(error);
            }
        };
        let attachment = match interfaces.prepare_attachment(reservation) {
            Ok(attachment) => attachment,
            Err(error) => {
                self.registration.cancel_replicator_creation(reservation);
                return Err(error);
            }
        };
        if let Err(error) = interfaces
            .creation
            .bind_settings(self.context.identity.clone(), settings)
        {
            self.registration.cancel_replicator_creation(reservation);
            return Err(error);
        }
        if let Err(error) = self
            .registration
            .register_interfaces(&attachment, state_provider, reservation)
            .await
        {
            self.registration.cancel_replicator_creation(reservation);
            return Err(error);
        }
        attachment.disarm();
        Ok(interfaces)
    }
}

pub struct DefaultReplicatorFactory {
    storage: Arc<dyn DurableState>,
}

impl DefaultReplicatorFactory {
    pub fn new(storage: Arc<dyn DurableState>) -> Self {
        Self { storage }
    }
}

#[async_trait]
impl ReplicatorFactory for DefaultReplicatorFactory {
    async fn create_replicator(
        &self,
        context: ReplicatorFactoryContext,
        state_provider: Option<Arc<dyn StateProvider>>,
        settings: ReplicatorSettings,
    ) -> Result<ReplicatorInterfaces> {
        let state_provider = state_provider.ok_or_else(|| {
            RuntimeError::Application("the default replicator requires a state provider".into())
        })?;
        let streams = Arc::new(ServiceStreams::new());
        let dependencies = context.default_dependencies.clone().ok_or_else(|| {
            RuntimeError::Application(
                "default replicator dependencies are unavailable for this partition".into(),
            )
        })?;
        let engine = DefaultReplicatorInner::new(
            context.identity.clone(),
            dependencies.managed_store,
            dependencies.replication_progress_store,
            dependencies.local_write_journal,
            dependencies.build_authority_store,
            dependencies.build_progress_store,
        );
        engine
            .install_provider(
                state_provider.clone(),
                self.storage.clone(),
                streams.clone(),
            )
            .await?;
        let pending = Arc::new(Mutex::new(None));
        let next_operation = Arc::new(AtomicU64::new(0));
        let replicator = Arc::new(DefaultReplicator {
            engine: engine.clone(),
            provider: state_provider,
            settings: Arc::new(RwLock::new(settings)),
            pending: pending.clone(),
            next_operation: next_operation.clone(),
        });
        let state_replicator = Arc::new(DefaultStateReplicator {
            engine: engine.clone(),
            streams,
            settings: replicator.settings.clone(),
            next_operation,
            pending,
        });
        Ok(ReplicatorInterfaces::managed_primary(
            context.creation_identity()?,
            replicator,
            Some(state_replicator),
            ReplicatorCapabilities {
                catch_up_specific_quorum: true,
                data_loss_replay: DataLossReplay::ReplaceOnAmbiguity,
            },
            engine,
        ))
    }
}

pub struct DefaultReplicator {
    engine: Arc<DefaultReplicatorInner>,
    provider: Arc<dyn StateProvider>,
    settings: Arc<RwLock<ReplicatorSettings>>,
    pending: Arc<Mutex<Option<ClientWrite>>>,
    next_operation: Arc<AtomicU64>,
}

#[async_trait]
impl Replicator for DefaultReplicator {
    async fn open(&self) -> Result<String> {
        let committed_lsn = self.provider.last_committed_lsn().await?;
        self.engine.control_open(committed_lsn).await?;
        Ok(self.settings.read().await.replication_address.clone())
    }

    async fn change_role(&self, epoch: Epoch, role: ReplicaRole) -> Result<()> {
        self.engine.control_change_role(epoch, role).await
    }

    async fn update_epoch(&self, epoch: Epoch) -> Result<()> {
        self.engine
            .control_update_epoch(epoch, self.provider.as_ref())
            .await
    }

    async fn close(&self) -> Result<()> {
        self.engine.control_close().await
    }

    fn abort(&self) {
        self.engine.control_abort();
    }

    async fn current_progress(&self) -> Result<Lsn> {
        self.engine.control_progress().await
    }

    async fn catch_up_capability(&self) -> Result<Lsn> {
        self.engine.control_catch_up_capability().await
    }
}

#[async_trait]
impl PrimaryReplicator for DefaultReplicator {
    async fn on_data_loss(&self) -> Result<bool> {
        if let Some(progress) = self
            .engine
            .control_on_data_loss(self.provider.as_ref())
            .await?
        {
            *self.pending.lock().await = None;
            self.next_operation
                .store(progress.max(0) as u64, Ordering::Release);
            Ok(true)
        } else {
            Ok(false)
        }
    }

    async fn update_catch_up_replica_set_configuration(
        &self,
        current: ReplicaSetConfiguration,
        previous: ReplicaSetConfiguration,
    ) -> Result<()> {
        for replica in current.replicas.iter().chain(&previous.replicas) {
            replica.validate_public_progress()?;
        }
        self.engine
            .configure_replicas(current.configuration, Some(previous.configuration))
            .await
    }

    async fn wait_for_catch_up_quorum(&self, mode: ReplicaSetQuorumMode) -> Result<()> {
        self.engine.wait_for_quorum(mode).await
    }

    async fn update_current_replica_set_configuration(
        &self,
        current: ReplicaSetConfiguration,
    ) -> Result<()> {
        for replica in &current.replicas {
            replica.validate_public_progress()?;
        }
        self.engine
            .configure_replicas(current.configuration, None)
            .await
    }

    async fn build_replica(&self, replica: ReplicaInformation) -> Result<()> {
        replica.validate_public_progress()?;
        self.engine.wait_for_build(replica).await
    }

    async fn remove_replica(&self, replica_id: ReplicaId) -> Result<()> {
        self.engine.remove_replica(replica_id).await
    }
}

struct DefaultStateReplicator {
    engine: Arc<DefaultReplicatorInner>,
    streams: Arc<ServiceStreams>,
    settings: Arc<RwLock<ReplicatorSettings>>,
    next_operation: Arc<AtomicU64>,
    pending: Arc<Mutex<Option<ClientWrite>>>,
}

#[async_trait]
impl StateReplicator for DefaultStateReplicator {
    async fn replicate(&self, data: OperationData) -> Result<Lsn> {
        let engine = &self.engine;
        engine.require_write_access().await?;
        let mut reservation = self.pending.lock().await;
        engine.require_write_access().await?;
        let created_reservation = if let Some(write) = reservation.as_ref() {
            if write.data != data {
                return Err(RuntimeError::LocalWritePending(
                    write.operation_id.to_string(),
                ));
            }
            false
        } else {
            let id = self.next_operation.fetch_add(1, Ordering::Relaxed);
            *reservation = Some(engine.recover_replicate_write(data, id).await);
            true
        };
        let write = reservation.as_ref().expect("write reserved").clone();
        let mut pending = match engine.begin_write(write.clone()).await {
            Ok(pending) => pending,
            Err(
                error @ (RuntimeError::DataLossFenced
                | RuntimeError::WriteClosed(_)
                | RuntimeError::AuthorityMismatch(_)
                | RuntimeError::NotPrimary),
            ) => {
                *reservation = None;
                return Err(error);
            }
            Err(error @ RuntimeError::LocalWritePending(_)) if created_reservation => {
                *reservation = None;
                return Err(error);
            }
            Err(error) => return Err(error),
        };
        let lsn = loop {
            if let Err(error) = engine.publish_replication(&pending).await {
                if matches!(
                    error,
                    RuntimeError::DataLossFenced
                        | RuntimeError::WriteClosed(_)
                        | RuntimeError::AuthorityMismatch(_)
                        | RuntimeError::NotPrimary
                ) {
                    *reservation = None;
                }
                return Err(error);
            }
            match pending.committed().await {
                Ok(receipt) => break receipt.lsn,
                Err(RuntimeError::WriteCompletionClosed) => {
                    pending = engine.begin_write(write.clone()).await?;
                }
                Err(error) => {
                    if matches!(
                        error,
                        RuntimeError::DataLossFenced
                            | RuntimeError::WriteClosed(_)
                            | RuntimeError::AuthorityMismatch(_)
                            | RuntimeError::NotPrimary
                    ) {
                        *reservation = None;
                    }
                    return Err(error);
                }
            }
        };
        *reservation = None;
        Ok(lsn)
    }

    async fn get_replication_stream(&self) -> Result<OperationStream> {
        self.streams.take_replication().await
    }

    async fn get_copy_stream(&self) -> Result<OperationStream> {
        self.streams.take_copy().await
    }

    async fn update_replicator_settings(&self, settings: ReplicatorSettings) -> Result<()> {
        *self.settings.write().await = settings;
        Ok(())
    }
}
