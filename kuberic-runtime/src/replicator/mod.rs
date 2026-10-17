pub mod stateless;
mod traits;
mod v1;

pub use traits::{PrimaryReplicator, Replicator, ReplicatorFactory, StateReplicator};

#[derive(Clone, Copy)]
enum PublicV1FactoryConstructor {
    Default(fn(Arc<dyn DurableState>) -> DefaultReplicatorFactory),
    Stateless(fn(Arc<dyn DurableState>) -> stateless::StatelessReplicatorFactory),
}

#[derive(Clone, Copy)]
pub(crate) struct PublicV1FactoryRegistration {
    name: &'static str,
    constructor: PublicV1FactoryConstructor,
}

impl PublicV1FactoryRegistration {
    pub(crate) const fn stateless(
        name: &'static str,
        constructor: fn(Arc<dyn DurableState>) -> stateless::StatelessReplicatorFactory,
    ) -> Self {
        Self {
            name,
            constructor: PublicV1FactoryConstructor::Stateless(constructor),
        }
    }

    pub(crate) const fn name(self) -> &'static str {
        self.name
    }
}

#[derive(Clone, Copy)]
pub(crate) struct PublicV1FactoryManifest {
    registrations: [PublicV1FactoryRegistration; 2],
}

impl PublicV1FactoryManifest {
    pub(crate) const fn registrations(self) -> [PublicV1FactoryRegistration; 2] {
        self.registrations
    }

    fn construct_default(self, storage: Arc<dyn DurableState>) -> DefaultReplicatorFactory {
        self.registrations
            .into_iter()
            .find_map(|registration| match registration.constructor {
                PublicV1FactoryConstructor::Default(constructor) => {
                    Some(constructor(storage.clone()))
                }
                PublicV1FactoryConstructor::Stateless(_) => None,
            })
            .expect("Public V1 default factory registration")
    }

    fn construct_stateless(
        self,
        storage: Arc<dyn DurableState>,
    ) -> stateless::StatelessReplicatorFactory {
        self.registrations
            .into_iter()
            .find_map(|registration| match registration.constructor {
                PublicV1FactoryConstructor::Default(_) => None,
                PublicV1FactoryConstructor::Stateless(constructor) => {
                    Some(constructor(storage.clone()))
                }
            })
            .expect("Public V1 stateless factory registration")
    }
}

fn construct_default_factory(storage: Arc<dyn DurableState>) -> DefaultReplicatorFactory {
    DefaultReplicatorFactory { storage }
}

fn construct_stateless_factory(
    storage: Arc<dyn DurableState>,
) -> stateless::StatelessReplicatorFactory {
    stateless::StatelessReplicatorFactory::from_public_v1_manifest(storage)
}

pub(crate) const PUBLIC_V1_FACTORIES: PublicV1FactoryManifest = PublicV1FactoryManifest {
    registrations: [
        PublicV1FactoryRegistration {
            name: "DefaultReplicatorFactory",
            constructor: PublicV1FactoryConstructor::Default(construct_default_factory),
        },
        PublicV1FactoryRegistration {
            name: "StatelessReplicatorFactory",
            constructor: PublicV1FactoryConstructor::Stateless(construct_stateless_factory),
        },
    ],
};

pub(crate) fn validate_public_v1_factories(manifest: PublicV1FactoryManifest) -> Result<()> {
    if manifest
        .registrations()
        .map(PublicV1FactoryRegistration::name)
        != ["DefaultReplicatorFactory", "StatelessReplicatorFactory"]
    {
        return Err(RuntimeError::Application(
            "Public V1 factory registrations changed".into(),
        ));
    }
    Ok(())
}
pub mod stream;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex, OnceLock};

use crate::capabilities::{ReplicatorCreationIdentity, RuntimeHostToken};
use crate::protocol::types::{
    AccessStatus, ConfigurationDescriptor, FaultType, LoadMetric, OperationId,
    PartitionInformation, ProcessSessionId, ReplicaIdentity, ReplicaRole,
    ReplicaStorageIncarnationContext,
};
use async_trait::async_trait;

use crate::application::{INVALID_LSN, Lsn, OperationData, StateProvider};
use crate::engine::DurableState;
use crate::host::store::PublicInboundBuildAdmission;
use crate::protocol::public_operations::PublicV1LifecycleBinding;
use crate::protocol::public_operations::{DataLossReplay, PublicReplicaDescriptor};
use crate::protocol::public_operations::{PublicInboundBuildAuthority, PublicInboundBuildReceipt};
use crate::{Result, RuntimeError};
use stream::OperationStream;

#[async_trait]
pub(crate) trait StatelessRuntimeControl: Send + Sync {
    async fn reported_progress(&self) -> Result<(i64, i64)>;
    async fn shutdown_and_drain(&self) -> Result<()>;
    async fn install_inbound_build_authority(
        &self,
        authority: &PublicInboundBuildAuthority,
    ) -> Result<()>;
    async fn revoke_inbound_build_authority(
        &self,
        authority: &PublicInboundBuildAuthority,
    ) -> Result<()>;

    fn raw_retained_task_cardinality(&self) -> (usize, usize);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplicaSetQuorumMode {
    /// Catch up a write quorum of each applicable configuration, including any
    /// replica marked `must_catchup`.
    WriteQuorum,
    /// Catch up all replicas participating in the applicable configurations.
    All,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplicaInformation {
    pub build_id: OperationId,
    pub identity: ReplicaIdentity,
    pub replication_address: String,
    pub process_session_id: ProcessSessionId,
    pub role: ReplicaRole,
    /// The replica's current LSN, or [`INVALID_LSN`] if omitted, unknown, or
    /// inapplicable. For an already configured or built replica, SF uses omitted
    /// input to refer to the replicator's known progress, not empty history.
    pub current_progress: Lsn,
    /// Earliest retained catch-up progress, or [`INVALID_LSN`] if omitted,
    /// unknown, or inapplicable. Omitted known-replica input has the same meaning
    /// as for [`Self::current_progress`].
    pub catch_up_capability: Lsn,
    /// Requires this replica to catch up even in write-quorum mode, for example
    /// when it is the next primary candidate.
    pub must_catchup: bool,
    pub storage_incarnation: Option<ReplicaStorageIncarnationContext>,
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
        };
        information.validate_public_progress()?;
        Ok(information)
    }

    pub fn validate_public_progress(&self) -> Result<()> {
        self.validate_progress_values()?;
        if self.role == ReplicaRole::IdleSecondary
            && (self.current_progress != INVALID_LSN
                || self.catch_up_capability != INVALID_LSN
                || self.must_catchup)
        {
            return Err(RuntimeError::Application(
                "idle public build target must use INVALID_LSN".into(),
            ));
        }
        Ok(())
    }

    fn validate_progress_values(&self) -> Result<()> {
        crate::protocol::public_operations::validate_progress_value(self.current_progress)
            .map_err(|message| RuntimeError::Application(message.into()))?;
        crate::protocol::public_operations::validate_progress_value(self.catch_up_capability)
            .map_err(|message| RuntimeError::Application(message.into()))?;
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
            storage_incarnation: replica.storage_incarnation,
        }
    }
}

/// Voting policy plus exact replica descriptions.
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

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ReplicatorSettings {
    pub replication_address: String,
}

impl ReplicatorSettings {
    fn validate(&self) -> Result<()> {
        Ok(())
    }
}

/// Coherent replication interfaces created and registered as one bundle.
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
struct BoundReplicatorSettings {
    identity: ReplicaIdentity,
    owner: ReplicatorCreationIdentity,
    settings: ReplicatorSettings,
}

struct ReplicatorCreation {
    identity: OnceLock<ReplicatorCreationIdentity>,
    guarded_control: StdMutex<Arc<dyn Replicator>>,
    capabilities: ReplicatorCapabilities,
    callback_containment: StdMutex<Option<tokio::sync::watch::Receiver<bool>>>,
    stateless_runtime_control: StdMutex<Option<Arc<dyn StatelessRuntimeControl>>>,
    settings: StdMutex<Option<BoundReplicatorSettings>>,
    armed: AtomicBool,
}

impl ReplicatorCreation {
    fn new(
        guarded_control: Arc<dyn Replicator>,
        identity: Option<ReplicatorCreationIdentity>,
        capabilities: ReplicatorCapabilities,
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
            callback_containment: StdMutex::new(None),
            stateless_runtime_control: StdMutex::new(None),
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
            if existing.identity != identity || existing.owner != owner {
                return Err(RuntimeError::Application(
                    "replicator registration cannot change identity".into(),
                ));
            }
            return Ok(());
        }
        *bound = Some(BoundReplicatorSettings {
            identity,
            owner,
            settings,
        });
        Ok(())
    }

    fn prepare_settings_update(&self, settings: ReplicatorSettings) -> Result<ReplicatorSettings> {
        let bound = self
            .settings
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let existing = bound.as_ref().ok_or_else(|| {
            RuntimeError::Application("replicator settings are not registration-bound".into())
        })?;
        settings.validate()?;
        if existing.settings.replication_address != settings.replication_address {
            return Err(RuntimeError::Application(
                "replicator settings cannot change the replication address".into(),
            ));
        }

        Ok(settings)
    }

    fn complete_settings_update(&self, settings: ReplicatorSettings) -> Result<()> {
        let mut bound = self
            .settings
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let existing = bound.as_mut().ok_or_else(|| {
            RuntimeError::Application("replicator settings are not registration-bound".into())
        })?;
        existing.settings = settings;
        Ok(())
    }
}

impl Drop for ReplicatorCreation {
    fn drop(&mut self) {
        if !self.armed.load(Ordering::Acquire) {
            return;
        }
        let control = self
            .guarded_control
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        control.abort();
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
        let settings = self.creation.prepare_settings_update(settings)?;
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
    has_provider_writer: bool,
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

    pub(crate) fn has_provider_writer(&self, _token: RuntimeHostToken) -> bool {
        self.has_provider_writer
    }

    pub(crate) fn callback_containment(
        &self,
        _token: RuntimeHostToken,
    ) -> Option<tokio::sync::watch::Receiver<bool>> {
        self.creation
            .callback_containment
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub(crate) fn stateless_runtime_control(
        &self,
        _token: RuntimeHostToken,
    ) -> Option<Arc<dyn StatelessRuntimeControl>> {
        self.creation
            .stateless_runtime_control
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn disarm(&self) {
        self.creation.armed.store(false, Ordering::Release);
    }
}

impl ReplicatorInterfaces {
    pub(crate) fn with_callback_containment(
        self,
        containment: tokio::sync::watch::Receiver<bool>,
    ) -> Self {
        *self
            .creation
            .callback_containment
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(containment);
        self
    }

    pub(crate) fn with_stateless_runtime_control(
        self,
        control: Arc<dyn StatelessRuntimeControl>,
    ) -> Self {
        *self
            .creation
            .stateless_runtime_control
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(control);
        self
    }

    pub fn secondary(
        replicator: Arc<dyn Replicator>,
        state_replicator: Option<Arc<dyn StateReplicator>>,
        capabilities: ReplicatorCapabilities,
    ) -> Self {
        let creation = Arc::new(ReplicatorCreation::new(
            replicator.clone(),
            None,
            capabilities,
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
        ));
        let state_replicator = protect_state_replicator(state_replicator, &creation);
        Self {
            replicator,
            state_replicator,
            primary_replicator: Some(primary_replicator),
            creation,
        }
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
            has_provider_writer: self.state_replicator.is_some(),
        })
    }
}

#[derive(Clone)]
pub struct ReplicatorFactoryContext {
    identity: ReplicaIdentity,
    process_session_id: ProcessSessionId,
    access: Arc<dyn PartitionAccessView>,
    reservation: Option<ReplicatorCreationReservation>,
    public_v1_binding: PublicV1LifecycleBinding,
    public_v1_replication_address: std::net::SocketAddr,
}

impl ReplicatorFactoryContext {
    #[doc(hidden)]
    pub(crate) fn new(
        _token: RuntimeHostToken,
        identity: ReplicaIdentity,
        process_session_id: ProcessSessionId,
        access: Arc<dyn PartitionAccessView>,
        public_v1_binding: PublicV1LifecycleBinding,
        public_v1_replication_address: std::net::SocketAddr,
    ) -> Self {
        Self {
            identity,
            process_session_id,
            access,
            reservation: None,
            public_v1_binding,
            public_v1_replication_address,
        }
    }

    pub fn identity(&self) -> Result<ReplicaIdentity> {
        Ok(self.identity.clone())
    }

    pub fn process_session_id(&self) -> Result<ProcessSessionId> {
        if self.process_session_id.is_empty() {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        Ok(self.process_session_id.clone())
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

    pub(crate) fn public_v1_binding(&self) -> &PublicV1LifecycleBinding {
        &self.public_v1_binding
    }

    pub(crate) fn public_v1_replication_address(&self) -> std::net::SocketAddr {
        self.public_v1_replication_address
    }

    pub(crate) async fn publish_replacement_progress(
        &self,
        applied_lsn: i64,
        committed_lsn: i64,
        ambiguous: bool,
    ) -> Result<()> {
        self.access
            .publish_replacement_progress(applied_lsn, committed_lsn, ambiguous)
            .await
    }

    pub(crate) async fn replacement_inbound_build_authority(
        &self,
        build_id: &OperationId,
    ) -> Result<Option<PublicInboundBuildAuthority>> {
        self.access
            .replacement_inbound_build_authority(build_id)
            .await
    }

    pub(crate) async fn classify_replacement_inbound_build_authority(
        &self,
        authority: &PublicInboundBuildAuthority,
    ) -> Result<PublicInboundBuildAdmission> {
        self.access
            .classify_replacement_inbound_build_authority(authority)
            .await
    }

    pub(crate) async fn install_replacement_inbound_build_authority(
        &self,
        authority: &PublicInboundBuildAuthority,
    ) -> Result<()> {
        self.access
            .install_replacement_inbound_build_authority(authority)
            .await
    }

    pub(crate) async fn replacement_terminal_inbound_build_receipt(
        &self,
        build_id: &OperationId,
    ) -> Result<Option<PublicInboundBuildReceipt>> {
        self.access
            .replacement_terminal_inbound_build_receipt(build_id)
            .await
    }

    pub(crate) async fn revoke_replacement_inbound_build_authority(&self) -> Result<()> {
        self.access
            .revoke_replacement_inbound_build_authority()
            .await
    }

    pub(crate) async fn replacement_outbound_build_authority(
        &self,
        build_id: &OperationId,
        current_configuration_id: &crate::protocol::types::ConfigurationId,
    ) -> Result<Option<PublicInboundBuildAuthority>> {
        self.access
            .replacement_outbound_build_authority(build_id, current_configuration_id)
            .await
    }

    pub(crate) async fn complete_replacement_inbound_build(
        &self,
        receipt: &PublicInboundBuildReceipt,
    ) -> Result<()> {
        self.access
            .complete_replacement_inbound_build(receipt)
            .await
    }

    pub(crate) async fn replacement_storage_is_retired(
        &self,
        storage: &crate::protocol::types::ReplicaStorageIncarnationContext,
    ) -> Result<bool> {
        self.access.replacement_storage_is_retired(storage).await
    }

    pub(crate) async fn replacement_build_storage_is_current(
        &self,
        authority: &PublicInboundBuildAuthority,
    ) -> Result<bool> {
        self.access
            .replacement_build_storage_is_current(authority)
            .await
    }

    pub(crate) fn release_provider_writer(&self) {
        self.access.release_provider_writer();
    }
}

#[async_trait]
#[doc(hidden)]
pub(crate) trait PartitionAccessView: Send + Sync {
    fn partition_information(&self) -> PartitionInformation;
    async fn read_status(&self) -> Result<AccessStatus>;

    async fn write_status(&self) -> Result<AccessStatus>;

    async fn publish_replacement_progress(
        &self,
        applied_lsn: i64,
        committed_lsn: i64,
        ambiguous: bool,
    ) -> Result<()>;

    async fn replacement_inbound_build_authority(
        &self,
        build_id: &OperationId,
    ) -> Result<Option<PublicInboundBuildAuthority>>;

    async fn classify_replacement_inbound_build_authority(
        &self,
        authority: &PublicInboundBuildAuthority,
    ) -> Result<PublicInboundBuildAdmission>;

    async fn install_replacement_inbound_build_authority(
        &self,
        authority: &PublicInboundBuildAuthority,
    ) -> Result<()>;

    async fn replacement_terminal_inbound_build_receipt(
        &self,
        build_id: &OperationId,
    ) -> Result<Option<PublicInboundBuildReceipt>>;

    async fn revoke_replacement_inbound_build_authority(&self) -> Result<()>;

    async fn replacement_outbound_build_authority(
        &self,
        build_id: &OperationId,
        current_configuration_id: &crate::protocol::types::ConfigurationId,
    ) -> Result<Option<PublicInboundBuildAuthority>>;

    async fn complete_replacement_inbound_build(
        &self,
        receipt: &PublicInboundBuildReceipt,
    ) -> Result<()>;

    async fn replacement_storage_is_retired(
        &self,
        storage: &crate::protocol::types::ReplicaStorageIncarnationContext,
    ) -> Result<bool>;

    async fn replacement_build_storage_is_current(
        &self,
        authority: &PublicInboundBuildAuthority,
    ) -> Result<bool>;

    fn release_provider_writer(&self);

    async fn report_load(&self, metrics: Vec<LoadMetric>) -> Result<()>;

    async fn report_fault(&self, fault: FaultType) -> Result<()>;
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

/// Access checks, partition information, replicator creation, and replica reporting.
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

    /// Checks the partition's readiness for write operations. Check this before
    /// servicing a client write request.
    pub async fn get_write_status(&self) -> Result<AccessStatus> {
        self.context.write_status().await
    }

    /// Checks the replica's readiness for read operations. Check this before
    /// servicing a client read request.
    pub async fn get_read_status(&self) -> Result<AccessStatus> {
        self.context.read_status().await
    }

    /// Returns information identifying the service partition.
    pub fn get_partition_information(&self) -> PartitionInformation {
        self.context.partition_information()
    }

    /// Reports load metrics for the current replica in the partition.
    pub async fn report_load(&self, metrics: Vec<LoadMetric>) -> Result<()> {
        self.context.report_load(metrics).await
    }

    /// Reports a fault that requires the replica to be restarted or removed
    /// rather than continuing to serve requests.
    pub async fn report_fault(&self, fault: FaultType) -> Result<()> {
        self.context.report_fault(fault).await
    }

    /// Creates a replicator with the selected factory and settings, registers
    /// its interfaces and optional state provider, and returns the interfaces
    /// to the service replica.
    ///
    /// Select a factory with [`Self::with_factory`] during service open.
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
        PUBLIC_V1_FACTORIES.construct_default(storage)
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
        validate_public_v1_factories(PUBLIC_V1_FACTORIES)?;
        PUBLIC_V1_FACTORIES
            .construct_stateless(self.storage.clone())
            .create_replicator(context, state_provider, settings)
            .await
    }
}
