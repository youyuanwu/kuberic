mod traits;
pub(crate) mod v1;

pub use traits::{PrimaryReplicator, Replicator, ReplicatorFactory, StateReplicator};

#[derive(Clone, Copy)]
enum PublicV1FactoryConstructor {
    Default(fn() -> DefaultReplicatorFactory),
}

#[derive(Clone, Copy)]
pub(crate) struct PublicV1FactoryRegistration {
    name: &'static str,
    constructor: PublicV1FactoryConstructor,
}

impl PublicV1FactoryRegistration {
    #[cfg(kuberic_workspace_tests)]
    pub(crate) const fn alternate(
        name: &'static str,
        constructor: fn() -> DefaultReplicatorFactory,
    ) -> Self {
        Self {
            name,
            constructor: PublicV1FactoryConstructor::Default(constructor),
        }
    }

    pub(crate) const fn name(self) -> &'static str {
        self.name
    }
}

#[derive(Clone, Copy)]
pub(crate) struct PublicV1FactoryManifest {
    registrations: [PublicV1FactoryRegistration; 1],
}

impl PublicV1FactoryManifest {
    pub(crate) const fn registrations(self) -> [PublicV1FactoryRegistration; 1] {
        self.registrations
    }

    fn construct_default(self) -> DefaultReplicatorFactory {
        let PublicV1FactoryConstructor::Default(constructor) = self.registrations[0].constructor;
        constructor()
    }
}

fn construct_default_factory() -> DefaultReplicatorFactory {
    DefaultReplicatorFactory
}

pub(crate) const PUBLIC_V1_FACTORIES: PublicV1FactoryManifest = PublicV1FactoryManifest {
    registrations: [PublicV1FactoryRegistration {
        name: "DefaultReplicatorFactory",
        constructor: PublicV1FactoryConstructor::Default(construct_default_factory),
    }],
};

pub(crate) fn validate_public_v1_factories(manifest: PublicV1FactoryManifest) -> Result<()> {
    if manifest
        .registrations()
        .map(PublicV1FactoryRegistration::name)
        != ["DefaultReplicatorFactory"]
    {
        return Err(RuntimeError::Application(
            "Public V1 factory registrations changed".into(),
        ));
    }
    Ok(())
}
mod custom;
pub mod stream;
pub use custom::CustomReplicatorControl;
#[cfg(feature = "testing")]
mod testing_transport;
#[cfg(feature = "testing")]
pub use testing_transport::{TestingTcpRoute, TestingTcpRouteKind};

/// Native queue admission. The assigned LSN is available before quorum
/// completion; neither observation commits primary-local application state.
#[must_use = "assignment is not replication completion or service commitment"]
pub struct Replication {
    pub lsn: crate::application::Lsn,
    pub completion: std::pin::Pin<Box<dyn std::future::Future<Output = crate::Result<()>> + Send>>,
}

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex, OnceLock};

use crate::capabilities::{ReplicatorCreationIdentity, RuntimeHostToken};
use crate::protocol::types::{
    AccessStatus, ConfigurationDescriptor, FaultType, LoadMetric, OperationId,
    PartitionInformation, ProcessSessionId, ReplicaId, ReplicaIdentity, ReplicaRole,
    ReplicaStorageIncarnationContext,
};
use async_trait::async_trait;

use crate::application::{INVALID_LSN, Lsn, OperationData, StateProvider};
use crate::host::store::PublicInboundBuildAdmission;
use crate::protocol::public_operations::PublicInboundBuildAuthority;
use crate::protocol::public_operations::PublicV1LifecycleBinding;
use crate::protocol::public_operations::{DataLossReplay, PublicReplicaDescriptor};
use crate::{Result, RuntimeError};

#[async_trait]
pub(crate) trait NativeRuntimeControl: Send + Sync {
    fn custom(&self) -> bool {
        false
    }
    fn build_authority_open(&self) -> bool {
        true
    }
    fn containment(&self) -> tokio::sync::watch::Receiver<bool>;
    fn selected_epoch(&self) -> Option<crate::protocol::types::Epoch>;
    fn idle_replicas(&self) -> Result<Vec<ReplicaId>>;
    async fn configuration_idle_replicas(
        &self,
        _configuration: &crate::protocol::public_operations::PublicConfiguration,
    ) -> Result<Vec<ReplicaId>> {
        self.idle_replicas()
    }

    fn progress_query_readiness(&self) -> Result<bool> {
        Ok(true)
    }

    #[cfg(feature = "testing")]
    fn testing_tcp_binding(
        &self,
    ) -> Option<(
        ReplicaIdentity,
        ProcessSessionId,
        ReplicatorCreationReservation,
    )>;

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
    #[cfg(feature = "testing")]
    specific_quorum_suppressed: AtomicBool,
    callback_containment: StdMutex<Option<tokio::sync::watch::Receiver<bool>>>,
    native_runtime_control: StdMutex<Option<Arc<dyn NativeRuntimeControl>>>,
    settings: StdMutex<Option<BoundReplicatorSettings>>,
    effective_address: OnceLock<String>,
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
            #[cfg(feature = "testing")]
            specific_quorum_suppressed: AtomicBool::new(false),
            callback_containment: StdMutex::new(None),
            native_runtime_control: StdMutex::new(None),
            settings: StdMutex::new(None),
            effective_address: OnceLock::new(),
            armed: AtomicBool::new(true),
        }
    }

    fn capabilities(&self) -> ReplicatorCapabilities {
        let capabilities = self.capabilities;
        #[cfg(feature = "testing")]
        let capabilities = ReplicatorCapabilities {
            catch_up_specific_quorum: capabilities.catch_up_specific_quorum
                && !self.specific_quorum_suppressed.load(Ordering::Acquire),
            ..capabilities
        };
        capabilities
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

    fn bind_settings(
        &self,
        identity: ReplicaIdentity,
        mut settings: ReplicatorSettings,
    ) -> Result<()> {
        if let Some(effective) = self.effective_address.get() {
            settings.replication_address = effective.clone();
        }
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
    fn begin_replicate(&self, data: OperationData) -> Result<Replication> {
        self.inner.begin_replicate(data)
    }

    fn replication_operations(&self) -> Result<stream::OperationStream> {
        self.inner.replication_operations()
    }

    fn copy_operations(&self) -> Result<stream::OperationStream> {
        self.inner.copy_operations()
    }

    async fn replicate(&self, data: OperationData) -> Result<Lsn> {
        self.inner.replicate(data).await
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
        self.creation.capabilities()
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

    pub(crate) fn native_runtime_control(
        &self,
        _token: RuntimeHostToken,
    ) -> Option<Arc<dyn NativeRuntimeControl>> {
        self.creation
            .native_runtime_control
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn disarm(&self) {
        self.creation.armed.store(false, Ordering::Release);
    }
}

impl ReplicatorInterfaces {
    /// Attaches the application's controller companion to this factory creation.
    ///
    /// This is a local host-authority adaptation, not an addition to the native
    /// SF Replicator interfaces. The owner must fence and join its real native
    /// work; it must not substitute proposed membership or invented progress.
    pub fn with_custom_control(
        self,
        context: &ReplicatorFactoryContext,
        control: Arc<dyn CustomReplicatorControl>,
    ) -> Result<Self> {
        context.claim_native_creation()?;
        let reservation = context
            .reservation
            .ok_or(RuntimeError::AuthorityNotAdmitted)?;
        self.creation
            .bind_identity(reservation.identity(RuntimeHostToken::new()))?;
        let mut slot = self.creation.native_runtime_control.lock().unwrap();
        if slot.is_some() {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        let owner: Arc<dyn NativeRuntimeControl> =
            Arc::new(custom::CustomControl::new(context.clone(), control));
        context
            .access
            .retain_custom_creation(reservation, owner.clone())?;
        *slot = Some(owner);
        drop(slot);
        Ok(self)
    }
    pub(crate) fn with_effective_replication_address(self, address: String) -> Self {
        self.creation
            .effective_address
            .set(address)
            .expect("effective endpoint is bound once");
        self
    }

    #[cfg(feature = "testing")]
    pub(crate) fn without_catch_up_specific_quorum(self) -> Result<Self> {
        if self.creation.identity.get().is_some() {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        self.creation
            .specific_quorum_suppressed
            .store(true, Ordering::Release);
        Ok(self)
    }

    async fn failed_creation(&self, error: RuntimeError) -> RuntimeError {
        self.replicator.abort();
        let native = self
            .creation
            .native_runtime_control
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if let Some(native) = native
            && let Err(cleanup) = native.shutdown_and_drain().await
        {
            return RuntimeError::Application(format!(
                "{error}; native creation cleanup failed: {cleanup}"
            ));
        }
        error
    }

    pub(crate) fn with_native_runtime_control(
        self,
        control: Arc<dyn NativeRuntimeControl>,
    ) -> Self {
        *self
            .creation
            .native_runtime_control
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
        self.creation.capabilities()
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
    native_factory_started: Arc<AtomicBool>,
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
            native_factory_started: Arc::new(AtomicBool::new(false)),
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
        context.native_factory_started = Arc::new(AtomicBool::new(false));
        context
    }

    pub(crate) fn claim_native_creation(&self) -> Result<()> {
        let reservation = self.reservation.ok_or(RuntimeError::AuthorityNotAdmitted)?;
        self.access.validate_native_creation(reservation)?;
        self.native_factory_started
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| RuntimeError::AuthorityNotAdmitted)?;
        Ok(())
    }

    pub(crate) fn public_v1_binding(&self) -> &PublicV1LifecycleBinding {
        &self.public_v1_binding
    }

    pub(crate) fn public_v1_replication_address(&self) -> std::net::SocketAddr {
        self.public_v1_replication_address
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

    pub(crate) async fn revoke_replacement_inbound_build_authority(&self) -> Result<()> {
        self.access
            .revoke_replacement_inbound_build_authority()
            .await
    }

    pub(crate) fn release_provider_writer(&self) {
        self.access.release_provider_writer();
    }

    pub(crate) async fn native_credentials(&self) -> Result<Arc<str>> {
        self.access.native_credentials().await
    }

    pub(crate) async fn native_local_endpoint(
        &self,
        address: String,
    ) -> Result<(
        crate::protocol::types::HistoryContext,
        crate::host::native_transport::NativeEndpoint,
    )> {
        self.access.native_local_endpoint(address).await
    }

    pub(crate) async fn validate_native_binding(
        &self,
        binding: &crate::host::native_transport::NativeBinding,
    ) -> Result<()> {
        self.access.validate_native_binding(binding).await
    }

    pub(crate) fn native_write_status(&self) -> AccessStatus {
        self.access.native_write_status()
    }

    pub(crate) fn native_write_admission(&self) -> Result<Arc<std::sync::Mutex<()>>> {
        self.access.native_write_admission()
    }

    pub(crate) fn observe_native_lifecycle(&self, observation: v1::LifecycleObservation) {
        if let Some(reservation) = self.reservation {
            self.access
                .observe_native_lifecycle(reservation, observation);
        }
    }

    pub(crate) async fn native_outbound_build_authority(
        &self,
        build: &OperationId,
    ) -> Result<PublicInboundBuildAuthority> {
        self.access.native_outbound_build_authority(build).await
    }

    /// Resolves the host's exact, currently running source Build operation.
    /// The returned value is data, not a bearer grant. Revalidate around native
    /// effects and asynchronous boundaries using `validate_build_authority`.
    pub async fn outbound_build_authority(
        &self,
        build: &OperationId,
    ) -> Result<PublicInboundBuildAuthority> {
        let authority = self.native_outbound_build_authority(build).await?;
        self.validate_build_authority(&authority).await?;
        Ok(authority)
    }

    /// Checks the exact live source operation or installed target ingress grant
    /// against this registered creation, process, storage and history.
    pub async fn validate_build_authority(
        &self,
        authority: &PublicInboundBuildAuthority,
    ) -> Result<()> {
        self.access
            .validate_custom_build_authority(
                self.reservation.ok_or(RuntimeError::AuthorityNotAdmitted)?,
                authority,
                true,
            )
            .await
    }
}

#[async_trait]
#[doc(hidden)]
pub(crate) trait PartitionAccessView: Send + Sync {
    fn retain_custom_creation(
        &self,
        _reservation: ReplicatorCreationReservation,
        _owner: Arc<dyn NativeRuntimeControl>,
    ) -> Result<()> {
        Err(RuntimeError::AuthorityNotAdmitted)
    }
    async fn validate_custom_build_authority(
        &self,
        _reservation: ReplicatorCreationReservation,
        _authority: &PublicInboundBuildAuthority,
        _require_installed: bool,
    ) -> Result<()> {
        Err(RuntimeError::AuthorityNotAdmitted)
    }
    fn native_write_admission(&self) -> Result<Arc<std::sync::Mutex<()>>> {
        Err(RuntimeError::AuthorityNotAdmitted)
    }

    fn partition_information(&self) -> PartitionInformation;
    async fn read_status(&self) -> Result<AccessStatus>;

    async fn write_status(&self) -> Result<AccessStatus>;

    fn native_write_status(&self) -> AccessStatus {
        AccessStatus::NotPrimary
    }

    fn native_read_status(&self) -> AccessStatus {
        AccessStatus::NotPrimary
    }

    fn validate_native_creation(&self, _reservation: ReplicatorCreationReservation) -> Result<()> {
        Err(RuntimeError::AuthorityNotAdmitted)
    }

    fn observe_native_lifecycle(
        &self,
        _reservation: ReplicatorCreationReservation,
        _observation: v1::LifecycleObservation,
    ) {
    }

    async fn native_credentials(&self) -> Result<Arc<str>> {
        Err(RuntimeError::AuthorityNotAdmitted)
    }

    async fn native_local_endpoint(
        &self,
        _address: String,
    ) -> Result<(
        crate::protocol::types::HistoryContext,
        crate::host::native_transport::NativeEndpoint,
    )> {
        Err(RuntimeError::AuthorityNotAdmitted)
    }

    async fn validate_native_binding(
        &self,
        _binding: &crate::host::native_transport::NativeBinding,
    ) -> Result<()> {
        Err(RuntimeError::AuthorityNotAdmitted)
    }

    async fn native_outbound_build_authority(
        &self,
        _build: &OperationId,
    ) -> Result<PublicInboundBuildAuthority> {
        Err(RuntimeError::AuthorityNotAdmitted)
    }

    async fn classify_replacement_inbound_build_authority(
        &self,
        authority: &PublicInboundBuildAuthority,
    ) -> Result<PublicInboundBuildAdmission>;

    async fn install_replacement_inbound_build_authority(
        &self,
        authority: &PublicInboundBuildAuthority,
    ) -> Result<()>;

    async fn revoke_replacement_inbound_build_authority(&self) -> Result<()>;

    fn release_provider_writer(&self);

    async fn report_load(&self, metrics: Vec<LoadMetric>) -> Result<()>;

    async fn report_fault(&self, fault: FaultType) -> Result<()>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[doc(hidden)]
pub(crate) struct ReplicatorCreationReservation(ReplicatorCreationIdentity);

#[doc(hidden)]
impl ReplicatorCreationReservation {
    #[cfg(test)]
    pub(crate) fn new(_token: RuntimeHostToken) -> Self {
        Self(ReplicatorCreationIdentity::new(RuntimeHostToken::new()))
    }

    pub(crate) fn from_identity(
        _token: RuntimeHostToken,
        identity: ReplicatorCreationIdentity,
    ) -> Self {
        Self(identity)
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

    /// Nonblocking host access observation for synchronous application commit
    /// callbacks. Lock contention is reported as reconfiguration pending.
    pub fn get_write_status_now(&self) -> AccessStatus {
        self.context.access.native_write_status()
    }

    pub fn get_read_status_now(&self) -> AccessStatus {
        self.context.access.native_read_status()
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
        #[cfg(feature = "testing")]
        let testing_route = testing_transport::before_attachment(
            self.context.identity.instance_id.as_str(),
            &self.context.process_session_id,
        );
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
                return Err(interfaces.failed_creation(error).await);
            }
        };
        if let Err(error) = interfaces
            .creation
            .bind_settings(self.context.identity.clone(), settings)
        {
            self.registration.cancel_replicator_creation(reservation);
            return Err(interfaces.failed_creation(error).await);
        }
        if let Err(error) = self
            .registration
            .register_interfaces(&attachment, state_provider, reservation)
            .await
        {
            self.registration.cancel_replicator_creation(reservation);
            return Err(interfaces.failed_creation(error).await);
        }
        attachment.disarm();
        #[cfg(feature = "testing")]
        if let Some(control) = attachment.native_runtime_control(RuntimeHostToken::new())
            && let Some(route) = testing_route.filter(|_| {
                control.testing_tcp_binding()
                    == Some((
                        self.context.identity.clone(),
                        self.context.process_session_id.clone(),
                        reservation,
                    ))
            })
        {
            testing_transport::acknowledge(route, &interfaces.creation);
        }
        Ok(interfaces)
    }
}

#[derive(Default)]
pub struct DefaultReplicatorFactory;

impl DefaultReplicatorFactory {
    pub fn new() -> Self {
        PUBLIC_V1_FACTORIES.construct_default()
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
        v1::create_production(context, state_provider, settings).await
    }
}
