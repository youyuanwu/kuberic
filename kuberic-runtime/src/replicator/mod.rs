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
pub mod stream;
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
    AccessStatus, ConfigurationDescriptor, FaultType, LoadMetric, PartitionInformation,
    ProcessSessionId, ReplicaIdentity, ReplicaRole, ReplicaStorageIncarnationContext,
};
use async_trait::async_trait;

use crate::application::{INVALID_LSN, Lsn, OperationData, StateProvider};
use crate::protocol::public_operations::PublicV1LifecycleBinding;
use crate::protocol::public_operations::{DataLossReplay, PublicReplicaDescriptor};
use crate::{Result, RuntimeError};

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
    pub fn new(identity: ReplicaIdentity, replication_address: String) -> Self {
        Self::idle_build(identity, replication_address)
    }

    pub fn idle_build(identity: ReplicaIdentity, replication_address: String) -> Self {
        Self {
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
    builtin: OnceLock<()>,
    identity: OnceLock<ReplicatorCreationIdentity>,
    guarded_control: StdMutex<Arc<dyn Replicator>>,
    capabilities: ReplicatorCapabilities,
    #[cfg(feature = "testing")]
    specific_quorum_suppressed: AtomicBool,
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
            builtin: OnceLock::new(),
            identity: creation_identity,
            guarded_control: StdMutex::new(guarded_control),
            capabilities,
            #[cfg(feature = "testing")]
            specific_quorum_suppressed: AtomicBool::new(false),
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

    fn disarm(&self) {
        self.creation.armed.store(false, Ordering::Release);
    }
}

impl ReplicatorInterfaces {
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

    async fn failed_creation(
        &self,
        error: RuntimeError,
        registration: &dyn ReplicatorRegistration,
    ) -> RuntimeError {
        if self.creation.armed.swap(false, Ordering::AcqRel) {
            self.replicator.abort();
        }
        if let Err(cleanup) = self.replicator.close().await {
            registration.record_creation_cleanup_failure(cleanup.to_string());
            return RuntimeError::Application(format!(
                "{error}; ordinary creation Close failed: {cleanup}"
            ));
        }
        error
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
    ordinary_contract: bool,
    retained_member_instructions: bool,
    contained_reconstruction_instructions: bool,
    builtin_history_bound: bool,
    builtin_factory_created: Arc<OnceLock<std::sync::Weak<ReplicatorCreation>>>,
    builtin_registered: Arc<OnceLock<crate::protocol::types::ReplicatorCreationId>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CheckedRetainedMember {
    scope: crate::protocol::public_operations::ReplicaScope,
    qualification: crate::protocol::public_operations::RetainedMemberQualification,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CheckedCreationReconstruction {
    goal: crate::protocol::public_operations::ReplicaGoal,
}

impl CheckedCreationReconstruction {
    pub(crate) fn new(goal: crate::protocol::public_operations::ReplicaGoal) -> Self {
        Self { goal }
    }

    pub(crate) fn scope(&self) -> &crate::protocol::public_operations::ReplicaScope {
        &self.goal.scope
    }

    pub(crate) fn addition(
        &self,
    ) -> Option<&crate::protocol::public_operations::MembershipAdditionRecovery> {
        match &self.goal.desired {
            crate::protocol::public_operations::ReplicaGoalKind::RecoverContainedCreation {
                recovery,
            } => recovery.addition.as_deref(),
            _ => None,
        }
    }

    pub(crate) fn role(&self) -> crate::protocol::types::ReplicaRole {
        match &self.goal.desired {
            crate::protocol::public_operations::ReplicaGoalKind::RecoverContainedCreation {
                recovery,
            } => recovery.role,
            _ => unreachable!("checked reconstruction"),
        }
    }
}

impl CheckedRetainedMember {
    pub(crate) fn new(
        scope: crate::protocol::public_operations::ReplicaScope,
        qualification: crate::protocol::public_operations::RetainedMemberQualification,
    ) -> Self {
        Self {
            scope,
            qualification,
        }
    }

    pub(crate) fn scope(&self) -> &crate::protocol::public_operations::ReplicaScope {
        &self.scope
    }

    pub(crate) fn epoch(&self) -> crate::protocol::types::Epoch {
        self.scope.epoch
    }

    pub(crate) fn history(&self) -> &crate::protocol::types::HistoryContext {
        &self.scope.history
    }

    pub(crate) fn qualification(
        &self,
    ) -> &crate::protocol::public_operations::RetainedMemberQualification {
        &self.qualification
    }
}

impl ReplicatorFactoryContext {
    /// Historical engine provenance is not a restriction on service factory choice.
    pub(crate) fn bind_builtin_history(&mut self) {
        self.builtin_history_bound = true;
    }

    pub(crate) fn builtin_history_bound(&self) -> bool {
        self.builtin_history_bound
    }

    pub(crate) async fn creation_reconstruction(
        &self,
    ) -> Result<Option<CheckedCreationReconstruction>> {
        if !self.ordinary_contract {
            return Ok(None);
        }
        self.access.creation_reconstruction(self.reservation).await
    }

    pub(crate) async fn confirm_reconstructed_native_initialization(
        &self,
        selected: &CheckedCreationReconstruction,
    ) -> Result<()> {
        if self.creation_reconstruction().await?.as_ref() != Some(selected)
            || self
                .builtin_factory_created
                .get()
                .and_then(std::sync::Weak::upgrade)
                .is_none()
            || self.builtin_registered.get() != Some(&selected.scope().creation_id)
        {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        self.access
            .confirm_reconstructed_native_initialization(
                self.reservation.ok_or(RuntimeError::AuthorityNotAdmitted)?,
                selected,
            )
            .await
    }

    fn builtin_creation_matches(&self, creation: &Arc<ReplicatorCreation>) -> bool {
        self.builtin_factory_created
            .get()
            .and_then(std::sync::Weak::upgrade)
            .is_some_and(|actual| Arc::ptr_eq(&actual, creation))
    }

    /// Called by the builtin adapter only after the actual retained role
    /// constructor and its real provider callbacks have succeeded.
    pub(crate) async fn confirm_retained_native_initialization(
        &self,
        selected: &CheckedRetainedMember,
    ) -> Result<()> {
        let current = self
            .retained_member_restore()
            .await?
            .ok_or(RuntimeError::AuthorityNotAdmitted)?;
        if &current != selected
            || self
                .builtin_factory_created
                .get()
                .and_then(std::sync::Weak::upgrade)
                .is_none()
            || self.builtin_registered.get() != Some(&current.scope.creation_id)
        {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        self.access
            .confirm_retained_native_initialization(
                self.reservation.ok_or(RuntimeError::AuthorityNotAdmitted)?,
                &current,
            )
            .await
    }

    pub(crate) async fn retained_member_restore(&self) -> Result<Option<CheckedRetainedMember>> {
        if !self.ordinary_contract {
            return Ok(None);
        }
        self.access
            .retained_member_restore(self.reservation.ok_or(RuntimeError::AuthorityNotAdmitted)?)
            .await
    }

    pub(crate) fn with_ordinary_contract(mut self, ordinary: bool) -> Self {
        self.ordinary_contract = ordinary;
        self
    }

    pub fn ordinary_hosting(&self) -> bool {
        self.ordinary_contract
    }

    pub fn creation_id(&self) -> Result<crate::protocol::types::ReplicatorCreationId> {
        Ok(self
            .reservation
            .ok_or(RuntimeError::AuthorityNotAdmitted)?
            .identity(RuntimeHostToken::new())
            .to_public_id())
    }

    pub async fn local_publication(
        &self,
    ) -> Result<Option<crate::protocol::public_operations::ReplicaPublication>> {
        self.access
            .local_publication(self.reservation.ok_or(RuntimeError::AuthorityNotAdmitted)?)
            .await
    }

    pub async fn discover_peer(
        &self,
        query: &crate::protocol::public_operations::ReplicaDiscoveryQuery,
    ) -> Result<Option<crate::protocol::public_operations::ReplicaPublication>> {
        use crate::protocol::public_operations::ORDINARY_ACTION_FORMAT;
        if query.format != ORDINARY_ACTION_FORMAT
            || query.history.resource_uid != self.public_v1_binding.resource_uid
        {
            return Err(RuntimeError::AuthorityMismatch(
                "incompatible discovery scope".into(),
            ));
        }
        let credentials = self.access.native_credentials().await?;
        let address = self.access.peer_control_endpoint(&query.replica)?;
        crate::control::resolve_replica_publication(&address, &credentials, query).await
    }

    pub(crate) async fn native_operation_snapshot(
        &self,
        history: &crate::protocol::types::HistoryContext,
        replica: &ReplicaIdentity,
    ) -> Result<crate::protocol::public_operations::ReplicaAgentSnapshot> {
        if !self.ordinary_contract
            || history.resource_uid != self.public_v1_binding.resource_uid
            || self.builtin_registered.get() != Some(&self.creation_id()?)
        {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        let credentials = self.access.native_credentials().await?;
        let address = self.access.peer_control_endpoint(replica)?;
        crate::control::resolve_native_snapshot(&address, &credentials, history, replica).await
    }

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
            ordinary_contract: false,
            retained_member_instructions: false,
            contained_reconstruction_instructions: false,
            builtin_history_bound: false,
            builtin_factory_created: Arc::new(OnceLock::new()),
            builtin_registered: Arc::new(OnceLock::new()),
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
        context.builtin_factory_created = Arc::new(OnceLock::new());
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
}

#[async_trait]
#[doc(hidden)]
pub(crate) trait PartitionAccessView: Send + Sync {
    fn reconstructed_native_initialization(&self) -> Option<CheckedCreationReconstruction> {
        None
    }

    async fn creation_reconstruction(
        &self,
        _reservation: Option<ReplicatorCreationReservation>,
    ) -> Result<Option<CheckedCreationReconstruction>> {
        Ok(None)
    }

    async fn confirm_reconstructed_native_initialization(
        &self,
        _reservation: ReplicatorCreationReservation,
        _selected: &CheckedCreationReconstruction,
    ) -> Result<()> {
        Err(RuntimeError::AuthorityNotAdmitted)
    }

    fn retained_native_initialization(&self) -> Option<CheckedRetainedMember> {
        None
    }

    async fn confirm_retained_native_initialization(
        &self,
        _reservation: ReplicatorCreationReservation,
        _selected: &CheckedRetainedMember,
    ) -> Result<()> {
        Err(RuntimeError::AuthorityNotAdmitted)
    }

    async fn retained_member_restore(
        &self,
        _reservation: ReplicatorCreationReservation,
    ) -> Result<Option<CheckedRetainedMember>> {
        Ok(None)
    }

    async fn local_publication(
        &self,
        _reservation: ReplicatorCreationReservation,
    ) -> Result<Option<crate::protocol::public_operations::ReplicaPublication>> {
        Err(RuntimeError::AuthorityNotAdmitted)
    }
    fn peer_control_endpoint(&self, _identity: &ReplicaIdentity) -> Result<String> {
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

    fn record_creation_cleanup_failure(&self, error: String);

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
    factory: Option<ReplicatorFactorySelection>,
}

#[derive(Clone)]
enum ReplicatorFactorySelection {
    Builtin,
    Override(Arc<dyn ReplicatorFactory>),
}

impl StatefulServicePartition {
    pub fn reconstructing_contained_creation(&self) -> bool {
        self.context.contained_reconstruction_instructions
    }

    pub(crate) fn with_contained_reconstruction_instructions(mut self) -> Self {
        self.context.contained_reconstruction_instructions = true;
        self
    }

    pub(crate) fn with_builtin_history(mut self) -> Self {
        self.context.bind_builtin_history();
        self
    }

    pub(crate) async fn creation_reconstruction(
        &self,
    ) -> Result<Option<CheckedCreationReconstruction>> {
        self.context.creation_reconstruction().await
    }

    pub(crate) fn reconstructed_native_initialization(
        &self,
    ) -> Option<CheckedCreationReconstruction> {
        self.context.access.reconstructed_native_initialization()
    }

    pub fn restoring_retained_member(&self) -> bool {
        self.context.retained_member_instructions
    }

    pub(crate) fn with_retained_member_instructions(mut self) -> Self {
        self.context.retained_member_instructions = true;
        self
    }

    pub(crate) fn builtin_native_registration(
        &self,
    ) -> Option<crate::protocol::types::ReplicatorCreationId> {
        self.context.builtin_registered.get().cloned()
    }

    pub(crate) fn retained_native_initialization(&self) -> Option<CheckedRetainedMember> {
        self.context.access.retained_native_initialization()
    }

    pub(crate) fn record_creation_cleanup_failure(&self, error: String) {
        self.registration.record_creation_cleanup_failure(error);
    }

    pub fn ordinary_hosting(&self) -> bool {
        self.context.ordinary_hosting()
    }
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

    /// Select the SDK-owned builtin during service Open.
    ///
    /// Construction uses the private builtin manifest. Applications may also
    /// select a factory that delegates to the builtin; actual registration,
    /// not the selected route, establishes provenance. Selection grants no access.
    pub fn with_builtin_replicator(&self) -> Self {
        Self {
            context: self.context.clone(),
            registration: self.registration.clone(),
            factory: Some(ReplicatorFactorySelection::Builtin),
        }
    }

    /// Select an arbitrary implementation during service Open.
    ///
    /// The selected factory is invoked even when historical engine provenance
    /// is builtin. Returned interfaces are checked separately; generic Open
    /// does not grant builtin-private recovery eligibility.
    pub fn with_factory(&self, factory: Arc<dyn ReplicatorFactory>) -> Self {
        Self {
            context: self.context.clone(),
            registration: self.registration.clone(),
            factory: Some(ReplicatorFactorySelection::Override(factory)),
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
    /// Explicitly select [`Self::with_builtin_replicator`] or
    /// [`Self::with_factory`] during service Open. No selection is an error.
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
        let provenance = context.clone();
        #[cfg(feature = "testing")]
        let testing_route = testing_transport::before_attachment(
            self.context.identity.instance_id.as_str(),
            &self.context.process_session_id,
        );
        let settings = settings.unwrap_or_default();
        let constructed = match factory {
            ReplicatorFactorySelection::Builtin => {
                PUBLIC_V1_FACTORIES
                    .construct_default()
                    .create_replicator(context, state_provider.clone(), settings.clone())
                    .await
            }
            ReplicatorFactorySelection::Override(factory) => {
                factory
                    .create_replicator(context, state_provider.clone(), settings.clone())
                    .await
            }
        };
        let interfaces = match constructed {
            Ok(interfaces) => interfaces,
            Err(error) => {
                self.registration.cancel_replicator_creation(reservation);
                return Err(error);
            }
        };
        let interfaces = self
            .register_created_replicator(
                interfaces,
                provenance,
                state_provider,
                settings,
                reservation,
            )
            .await?;
        #[cfg(feature = "testing")]
        if let Some(route) = testing_route
            .filter(|route| testing_transport::has_native_transport(route, reservation))
        {
            testing_transport::acknowledge(route, &interfaces.creation);
        }
        Ok(interfaces)
    }

    async fn register_created_replicator(
        &self,
        interfaces: ReplicatorInterfaces,
        provenance: ReplicatorFactoryContext,
        state_provider: Option<Arc<dyn StateProvider>>,
        settings: ReplicatorSettings,
        reservation: ReplicatorCreationReservation,
    ) -> Result<ReplicatorInterfaces> {
        tracing::debug!(
            historical_builtin = provenance.builtin_history_bound(),
            actual_builtin = interfaces.creation.builtin.get().is_some(),
            "ordinary selected factory returned its interface bundle"
        );
        let attachment = match interfaces.prepare_attachment(reservation) {
            Ok(attachment) => attachment,
            Err(error) => {
                self.registration.cancel_replicator_creation(reservation);
                return Err(interfaces
                    .failed_creation(error, self.registration.as_ref())
                    .await);
            }
        };
        if (interfaces.creation.builtin.get().is_some()
            || self.restoring_retained_member()
            || self.reconstructing_contained_creation())
            && !provenance.builtin_creation_matches(&interfaces.creation)
        {
            self.registration.cancel_replicator_creation(reservation);
            return Err(interfaces
                .failed_creation(
                    RuntimeError::AuthorityMismatch(
                        "replicator bundle does not match its actual builtin recovery provenance"
                            .into(),
                    ),
                    self.registration.as_ref(),
                )
                .await);
        }
        if let Err(error) = interfaces
            .creation
            .bind_settings(self.context.identity.clone(), settings)
        {
            self.registration.cancel_replicator_creation(reservation);
            return Err(interfaces
                .failed_creation(error, self.registration.as_ref())
                .await);
        }
        if let Err(error) = self
            .registration
            .register_interfaces(&attachment, state_provider, reservation)
            .await
        {
            self.registration.cancel_replicator_creation(reservation);
            return Err(interfaces
                .failed_creation(error, self.registration.as_ref())
                .await);
        }
        attachment.disarm();
        if provenance.builtin_creation_matches(&interfaces.creation) {
            let _ = self
                .context
                .builtin_registered
                .set(reservation.identity(RuntimeHostToken::new()).to_public_id());
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
        let provenance = context.clone();
        let interfaces = v1::create_production(context, state_provider, settings).await?;
        interfaces
            .creation
            .builtin
            .set(())
            .map_err(|_| RuntimeError::AuthorityNotAdmitted)?;
        provenance
            .builtin_factory_created
            .set(Arc::downgrade(&interfaces.creation))
            .map_err(|_| RuntimeError::AuthorityNotAdmitted)?;
        Ok(interfaces)
    }
}

#[cfg(test)]
mod factory_selection_tests {
    use super::*;
    use crate::application::OperationDataStream;
    use crate::protocol::public_operations::{PublicV1Identity, StatePersistence};
    use crate::protocol::types::{
        AgentGeneration, Epoch, PartitionId, PodUid, PublicProtocolGeneration, PvcUid, ReplicaId,
        ReplicaInstanceId, ReplicaStorageIdentity, ResourceUid, StorageGeneration,
        derive_bootstrap_history, derive_history_initialization_id,
    };
    use std::sync::atomic::AtomicUsize;

    // The existing attachment fixture's actual context/reservation seam, without
    // testing routes or any projection of host readiness, role or progress.
    #[derive(Default)]
    struct Fixture {
        binding: StdMutex<Option<(ReplicaIdentity, ProcessSessionId)>>,
        creation: StdMutex<Creation>,
        write_admission: Arc<StdMutex<()>>,
        reservations: AtomicUsize,
        cancellations: AtomicUsize,
        registrations: AtomicUsize,
        native_endpoints: AtomicUsize,
        lifecycle_observations: AtomicUsize,
    }

    #[derive(Default)]
    struct Creation {
        reserved: Option<ReplicatorCreationReservation>,
        retained: Vec<Arc<dyn Replicator>>,
    }

    impl Fixture {
        async fn drain(&self) {
            let owners = {
                let mut creation = self.creation.lock().unwrap();
                assert!(creation.reserved.is_none());
                std::mem::take(&mut creation.retained)
            };
            for owner in owners {
                owner.abort();
                owner.close().await.unwrap();
            }
        }

        fn assert_no_construction(&self) {
            assert_eq!(self.reservations.load(Ordering::SeqCst), 0);
            assert_eq!(self.cancellations.load(Ordering::SeqCst), 0);
            assert_eq!(self.registrations.load(Ordering::SeqCst), 0);
            assert_eq!(self.native_endpoints.load(Ordering::SeqCst), 0);
            let creation = self.creation.lock().unwrap();
            assert!(creation.reserved.is_none());
            assert!(creation.retained.is_empty());
        }
    }

    #[async_trait]
    impl ReplicatorRegistration for Fixture {
        fn reserve_replicator_creation(&self) -> Result<ReplicatorCreationReservation> {
            self.reservations.fetch_add(1, Ordering::SeqCst);
            let mut creation = self.creation.lock().unwrap();
            if creation.reserved.is_some() {
                return Err(RuntimeError::AuthorityNotAdmitted);
            }
            let reservation = ReplicatorCreationReservation::new(RuntimeHostToken::new());
            creation.reserved = Some(reservation);
            Ok(reservation)
        }

        fn cancel_replicator_creation(&self, reservation: ReplicatorCreationReservation) {
            self.cancellations.fetch_add(1, Ordering::SeqCst);
            let mut creation = self.creation.lock().unwrap();
            assert_eq!(creation.reserved, Some(reservation));
            creation.reserved = None;
        }

        fn record_creation_cleanup_failure(&self, error: String) {
            panic!("unexpected factory boundary cleanup failure: {error}");
        }

        async fn register_interfaces(
            &self,
            attachment: &ReplicatorAttachment,
            _: Option<Arc<dyn StateProvider>>,
            reservation: ReplicatorCreationReservation,
        ) -> Result<()> {
            self.registrations.fetch_add(1, Ordering::SeqCst);
            let mut creation = self.creation.lock().unwrap();
            if creation.reserved != Some(reservation)
                || attachment.identity(RuntimeHostToken::new())
                    != reservation.identity(RuntimeHostToken::new())
            {
                return Err(RuntimeError::AuthorityNotAdmitted);
            }
            creation.reserved = None;
            creation
                .retained
                .push(attachment.replicator(RuntimeHostToken::new()));
            Ok(())
        }
    }

    #[async_trait]
    impl PartitionAccessView for Fixture {
        fn validate_native_creation(
            &self,
            reservation: ReplicatorCreationReservation,
        ) -> Result<()> {
            if self.creation.lock().unwrap().reserved == Some(reservation) {
                Ok(())
            } else {
                Err(RuntimeError::AuthorityNotAdmitted)
            }
        }

        fn native_write_admission(&self) -> Result<Arc<StdMutex<()>>> {
            Ok(self.write_admission.clone())
        }

        async fn native_credentials(&self) -> Result<Arc<str>> {
            Ok(Arc::from("factory-selection-fixture"))
        }

        async fn native_local_endpoint(
            &self,
            address: String,
        ) -> Result<(
            crate::protocol::types::HistoryContext,
            crate::host::native_transport::NativeEndpoint,
        )> {
            self.native_endpoints.fetch_add(1, Ordering::SeqCst);
            let (identity, process_session_id) = self.binding.lock().unwrap().clone().unwrap();
            let resource = ResourceUid::new(identity.instance_id.as_str());
            let generation = PublicProtocolGeneration::new(1);
            let initialization = derive_history_initialization_id(&resource, generation);
            let history = derive_bootstrap_history(&resource, generation, &initialization);
            let endpoint = crate::host::native_transport::NativeEndpoint {
                storage: ReplicaStorageIncarnationContext {
                    replica: identity.clone(),
                    storage: ReplicaStorageIdentity::Persistent {
                        pod_uid: PodUid::new(identity.instance_id.as_str()),
                        pvc_uid: PvcUid::new(identity.instance_id.as_str()),
                    },
                    storage_generation: StorageGeneration::new(identity.instance_id.as_str()),
                },
                identity,
                process_session_id,
                address,
            };
            Ok((history, endpoint))
        }

        fn observe_native_lifecycle(
            &self,
            _: ReplicatorCreationReservation,
            _: v1::LifecycleObservation,
        ) {
            self.lifecycle_observations.fetch_add(1, Ordering::SeqCst);
        }

        fn partition_information(&self) -> PartitionInformation {
            PartitionInformation {
                partition_id: PartitionId::new("factory-selection"),
            }
        }

        async fn read_status(&self) -> Result<AccessStatus> {
            Ok(AccessStatus::NotPrimary)
        }

        async fn write_status(&self) -> Result<AccessStatus> {
            Ok(AccessStatus::NotPrimary)
        }

        async fn report_load(&self, _: Vec<LoadMetric>) -> Result<()> {
            Err(RuntimeError::AuthorityNotAdmitted)
        }

        async fn report_fault(&self, _: FaultType) -> Result<()> {
            Err(RuntimeError::AuthorityNotAdmitted)
        }
    }

    #[async_trait]
    impl StateProvider for Fixture {
        async fn update_epoch(&self, _: Epoch, _: Lsn) -> Result<()> {
            Err(RuntimeError::AuthorityNotAdmitted)
        }

        async fn last_committed_lsn(&self) -> Result<Lsn> {
            Err(RuntimeError::AuthorityNotAdmitted)
        }

        async fn get_copy_context(&self) -> Result<OperationDataStream> {
            Err(RuntimeError::AuthorityNotAdmitted)
        }

        async fn get_copy_state(
            &self,
            _: Lsn,
            _: OperationDataStream,
        ) -> Result<OperationDataStream> {
            Err(RuntimeError::AuthorityNotAdmitted)
        }

        async fn on_data_loss(&self) -> Result<bool> {
            Err(RuntimeError::AuthorityNotAdmitted)
        }
    }

    #[derive(Default)]
    struct IndependentControl {
        aborts: AtomicUsize,
        closes: AtomicUsize,
    }

    #[async_trait]
    impl Replicator for IndependentControl {
        async fn open(&self) -> Result<String> {
            Err(RuntimeError::AuthorityNotAdmitted)
        }

        async fn close(&self) -> Result<()> {
            self.closes.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        async fn change_role(&self, _: Epoch, _: ReplicaRole) -> Result<()> {
            Err(RuntimeError::AuthorityNotAdmitted)
        }

        async fn update_epoch(&self, _: Epoch) -> Result<()> {
            Err(RuntimeError::AuthorityNotAdmitted)
        }

        fn abort(&self) {
            self.aborts.fetch_add(1, Ordering::SeqCst);
        }

        async fn current_progress(&self) -> Result<Lsn> {
            Err(RuntimeError::AuthorityNotAdmitted)
        }

        async fn catch_up_capability(&self) -> Result<Lsn> {
            Err(RuntimeError::AuthorityNotAdmitted)
        }
    }

    include!("factory_selection_test_factories.rs");

    fn partition(fixture: Arc<Fixture>) -> StatefulServicePartition {
        let instance = uuid::Uuid::new_v4().to_string();
        let identity = ReplicaIdentity {
            replica_id: ReplicaId::new(1),
            instance_id: ReplicaInstanceId::new(&instance),
            agent_generation: AgentGeneration::new(&instance),
        };
        let session = ProcessSessionId::new(&instance);
        *fixture.binding.lock().unwrap() = Some((identity.clone(), session.clone()));
        StatefulServicePartition::new(
            RuntimeHostToken::new(),
            fixture.clone(),
            ReplicatorFactoryContext::new(
                RuntimeHostToken::new(),
                identity,
                session,
                fixture,
                PublicV1LifecycleBinding {
                    identity: PublicV1Identity::new(1),
                    resource_uid: ResourceUid::new(instance),
                    spec_generation: 1,
                    state_persistence: StatePersistence::Persisted,
                },
                "127.0.0.1:0".parse().unwrap(),
            ),
        )
    }

    fn settings() -> ReplicatorSettings {
        ReplicatorSettings {
            replication_address: "http://127.0.0.1:0".into(),
        }
    }

    #[test]
    fn builtin_history_defaults_false_and_survives_context_and_per_creation_clones() {
        let fixture = Arc::new(Fixture::default());
        let mut partition = partition(fixture.clone());
        assert!(!partition.context.builtin_history_bound());
        partition.context.bind_builtin_history();
        partition.context.bind_builtin_history();
        let context = partition.context.clone();
        assert!(context.builtin_history_bound());
        assert!(partition.clone().context.builtin_history_bound());
        for _ in 0..2 {
            let reservation = fixture.reserve_replicator_creation().unwrap();
            let creation = context.for_creation(reservation);
            assert!(creation.builtin_history_bound());
            assert!(creation.clone().builtin_history_bound());
            assert_eq!(creation.reservation, Some(reservation));
            assert!(!creation.retained_member_instructions);
            assert!(!creation.contained_reconstruction_instructions);
            assert!(!creation.ordinary_hosting());
            assert!(!creation.native_factory_started.load(Ordering::SeqCst));
            assert!(creation.builtin_factory_created.get().is_none());
            fixture.cancel_replicator_creation(reservation);
        }
        assert_eq!(fixture.native_endpoints.load(Ordering::SeqCst), 0);
        assert_eq!(fixture.lifecycle_observations.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn bound_concrete_builtin_constructs_and_registers_actual_bundle_without_role_credit() {
        let fixture = Arc::new(Fixture::default());
        let mut partition = partition(fixture.clone());
        partition.context.bind_builtin_history();
        let partition = partition.with_builtin_replicator().clone();
        assert!(!partition.restoring_retained_member());
        assert!(!partition.reconstructing_contained_creation());
        let interfaces = partition
            .create_replicator(Some(fixture.clone()), Some(settings()))
            .await
            .unwrap();
        assert_eq!(
            partition.builtin_native_registration(),
            Some(interfaces.creation.identity.get().unwrap().to_public_id())
        );
        assert!(interfaces.primary_replicator().is_some());
        assert!(interfaces.state_replicator().is_some());
        assert_eq!(fixture.reservations.load(Ordering::SeqCst), 1);
        assert_eq!(fixture.registrations.load(Ordering::SeqCst), 1);
        assert_eq!(fixture.cancellations.load(Ordering::SeqCst), 0);
        assert_eq!(fixture.native_endpoints.load(Ordering::SeqCst), 1);
        assert_eq!(fixture.lifecycle_observations.load(Ordering::SeqCst), 0);
        assert_eq!(
            partition.get_read_status().await.unwrap(),
            AccessStatus::NotPrimary
        );
        assert_eq!(
            partition.get_write_status().await.unwrap(),
            AccessStatus::NotPrimary
        );
        assert_eq!(partition.get_read_status_now(), AccessStatus::NotPrimary);
        assert_eq!(partition.get_write_status_now(), AccessStatus::NotPrimary);
        fixture.drain().await;
    }

    async fn assert_history_bound_factory_is_invoked(
        factory: Arc<dyn ReplicatorFactory>,
        calls: Arc<AtomicUsize>,
        bind_before_selection: bool,
        actual_builtin: bool,
    ) {
        let fixture = Arc::new(Fixture::default());
        let mut partition = partition(fixture.clone());
        if bind_before_selection {
            partition.context.bind_builtin_history();
        }
        partition = partition
            .with_builtin_replicator()
            .with_factory(factory.clone());
        if !bind_before_selection {
            partition.context.bind_builtin_history();
        }
        let selected = partition.clone().with_factory(factory);
        assert!(selected.context.builtin_history_bound());
        let interfaces = selected
            .create_replicator(Some(fixture.clone()), Some(settings()))
            .await
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(fixture.reservations.load(Ordering::SeqCst), 1);
        assert_eq!(fixture.registrations.load(Ordering::SeqCst), 1);
        assert_eq!(fixture.cancellations.load(Ordering::SeqCst), 0);
        assert_eq!(
            selected.builtin_native_registration().is_some(),
            actual_builtin
        );
        assert_eq!(interfaces.creation.builtin.get().is_some(), actual_builtin);
        assert_eq!(
            fixture.native_endpoints.load(Ordering::SeqCst),
            usize::from(actual_builtin)
        );
        assert_eq!(fixture.lifecycle_observations.load(Ordering::SeqCst), 0);
        assert_eq!(
            selected.get_read_status().await.unwrap(),
            AccessStatus::NotPrimary
        );
        assert_eq!(
            selected.get_write_status().await.unwrap(),
            AccessStatus::NotPrimary
        );
        fixture.drain().await;
    }

    #[tokio::test]
    async fn history_bound_independent_factory_is_invoked_in_either_builder_order_or_clone() {
        for bind_before_selection in [true, false] {
            let calls = Arc::new(AtomicUsize::new(0));
            assert_history_bound_factory_is_invoked(
                Arc::new(independent_factory(calls.clone())),
                calls,
                bind_before_selection,
                false,
            )
            .await;
        }
    }

    #[tokio::test]
    async fn history_bound_default_wrapper_is_invoked_and_registers_actual_builtin() {
        for bind_before_selection in [true, false] {
            let calls = Arc::new(AtomicUsize::new(0));
            assert_history_bound_factory_is_invoked(
                Arc::new(DefaultWrapper {
                    calls: calls.clone(),
                }),
                calls,
                bind_before_selection,
                true,
            )
            .await;
        }
    }

    #[tokio::test]
    async fn independent_factory_invocation_does_not_grant_builtin_private_recovery() {
        for reconstructing in [false, true] {
            let fixture = Arc::new(Fixture::default());
            let calls = Arc::new(AtomicUsize::new(0));
            let factory = Arc::new(independent_factory(calls.clone()));
            let mut selected = partition(fixture.clone()).with_factory(factory.clone());
            selected.context.bind_builtin_history();
            selected = if reconstructing {
                selected.with_contained_reconstruction_instructions()
            } else {
                selected.with_retained_member_instructions()
            };
            assert!(matches!(
                selected.create_replicator(None, None).await,
                Err(RuntimeError::AuthorityMismatch(_))
            ));
            assert_eq!(calls.load(Ordering::SeqCst), 1);
            assert_eq!(*factory.requirements.lock().unwrap(), [true]);
            assert_eq!(fixture.reservations.load(Ordering::SeqCst), 1);
            assert_eq!(fixture.cancellations.load(Ordering::SeqCst), 1);
            assert_eq!(fixture.registrations.load(Ordering::SeqCst), 0);
            assert_eq!(fixture.native_endpoints.load(Ordering::SeqCst), 0);
            assert_eq!(fixture.lifecycle_observations.load(Ordering::SeqCst), 0);
            assert_eq!(factory.control.aborts.load(Ordering::SeqCst), 1);
            assert_eq!(factory.control.closes.load(Ordering::SeqCst), 1);
            assert!(selected.builtin_native_registration().is_none());
            assert_eq!(
                selected.get_read_status().await.unwrap(),
                AccessStatus::NotPrimary
            );
            assert_eq!(
                selected.get_write_status().await.unwrap(),
                AccessStatus::NotPrimary
            );
            fixture.drain().await;
        }
    }

    #[tokio::test]
    async fn unselected_partition_still_errors_without_consuming_a_reservation() {
        for required in [false, true] {
            let fixture = Arc::new(Fixture::default());
            let mut partition = partition(fixture.clone());
            if required {
                partition.context.bind_builtin_history();
            }
            assert!(matches!(
                partition.clone().create_replicator(None, None).await,
                Err(RuntimeError::Application(message))
                    if message == "select a replicator factory during Open"
            ));
            fixture.assert_no_construction();
        }
    }

    #[tokio::test]
    async fn unbound_independent_override_remains_usable_after_builtin_selection() {
        let fixture = Arc::new(Fixture::default());
        let calls = Arc::new(AtomicUsize::new(0));
        let factory = Arc::new(independent_factory(calls.clone()));
        let partition = partition(fixture.clone())
            .with_builtin_replicator()
            .with_factory(factory.clone())
            .clone();
        let interfaces = partition.create_replicator(None, None).await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(*factory.requirements.lock().unwrap(), [false]);
        assert!(interfaces.primary_replicator().is_none());
        assert!(interfaces.state_replicator().is_none());
        assert_eq!(
            interfaces.capabilities(),
            ReplicatorCapabilities::conservative()
        );
        assert!(partition.builtin_native_registration().is_none());
        assert_eq!(fixture.reservations.load(Ordering::SeqCst), 1);
        assert_eq!(fixture.registrations.load(Ordering::SeqCst), 1);
        assert_eq!(fixture.native_endpoints.load(Ordering::SeqCst), 0);
        fixture.drain().await;
        assert_eq!(factory.control.aborts.load(Ordering::SeqCst), 1);
        assert_eq!(factory.control.closes.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn actual_builtin_bundle_from_a_different_per_creation_context_is_rejected() {
        let fixture = Arc::new(Fixture::default());
        let mut partition = partition(fixture.clone());
        partition.context.bind_builtin_history();
        let partition = partition.with_builtin_replicator();
        assert!(!partition.restoring_retained_member());
        assert!(!partition.reconstructing_contained_creation());
        let reservation = fixture.reserve_replicator_creation().unwrap();
        let provenance = partition.context.for_creation(reservation);
        let foreign = partition.context.for_creation(reservation);
        let interfaces = default_factory()
            .create_replicator(foreign.clone(), Some(fixture.clone()), settings())
            .await
            .unwrap();
        assert!(foreign.builtin_creation_matches(&interfaces.creation));
        assert!(!provenance.builtin_creation_matches(&interfaces.creation));
        assert!(matches!(
            partition
                .register_created_replicator(
                    interfaces,
                    provenance,
                    Some(fixture.clone()),
                    settings(),
                    reservation,
                )
                .await,
            Err(RuntimeError::AuthorityMismatch(_))
        ));
        assert_eq!(fixture.native_endpoints.load(Ordering::SeqCst), 1);
        assert_eq!(fixture.reservations.load(Ordering::SeqCst), 1);
        assert_eq!(fixture.cancellations.load(Ordering::SeqCst), 1);
        assert_eq!(fixture.registrations.load(Ordering::SeqCst), 0);
        assert!(partition.builtin_native_registration().is_none());
        fixture.drain().await;
    }
}
