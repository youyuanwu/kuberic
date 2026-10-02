//! Private SF lifecycle/effect hosting, independent of operation/copy capability.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Weak};

use async_trait::async_trait;
use kuberic_protocol::types::{AccessStatus, OperationId, ProcessSessionId, ReplicaIdentity};
use kuberic_runtime::replicator::{
    ManagedReplicatorLifecycle, PrimaryReplicator, ReplicaInformation, ReplicaSetConfiguration,
    ReplicaSetQuorumMode, Replicator,
};
use kuberic_runtime::{Result, RuntimeError};
use kuberic_runtime_internal::authority::{BuildAuthority, BuildSelection, DurableBuildProgress};
use kuberic_runtime_internal::effects::{BuildPostcondition, RuntimeEffectAction, RuntimeSnapshot};
use kuberic_runtime_internal::transport::{OutboundOperation, ReplicaEndpoint};
use tokio::sync::{Mutex, RwLock, mpsc};

use super::{RuntimeHost, empty_snapshot};
#[path = "custom_removal.rs"]
mod removal;

#[derive(Clone, PartialEq, Eq)]
struct BuildReceipt {
    selection: BuildSelection,
    source_session: ProcessSessionId,
    target_session: ProcessSessionId,
    attempt_generation: u64,
}

#[async_trait]
trait ReplicatorLifecycleBackend: Send + Sync {
    fn owns_stream_session(&self) -> bool {
        false
    }

    async fn complete_open(&self, address: String) -> Result<()>;
    async fn fence_writes(&self) -> Result<()>;
    async fn settle_primary_prefix(&self) -> Result<()>;
    async fn cancel_configuration_work(&self) -> Result<()>;
    async fn restore_authority(&self) -> Result<()>;
    async fn restore_access(&self, read: AccessStatus, write: AccessStatus) -> Result<()>;
    async fn execute_action(&self, action: RuntimeEffectAction) -> Result<()>;
    async fn snapshot(&self) -> RuntimeSnapshot;
    async fn cancel_outbound_build(&self, id: &OperationId) -> Result<()>;
    async fn next_outbound(&self) -> Option<OutboundOperation>;

    async fn select_build(&self, _authority: &BuildAuthority) -> Result<()> {
        Ok(())
    }

    async fn register_custom_peer_session(
        &self,
        _identity: ReplicaIdentity,
        _session: ProcessSessionId,
    ) -> Result<()> {
        Ok(())
    }

    async fn describe_custom_peer(&self, _replica: ReplicaInformation) -> Result<()> {
        Ok(())
    }

    async fn refresh_custom_progress(&self) -> Result<()> {
        Ok(())
    }

    async fn execute_custom_build(&self, _replica: ReplicaInformation) -> Result<bool> {
        Ok(false)
    }

    async fn enqueue_custom_build(&self, _endpoint: ReplicaEndpoint) -> Result<bool> {
        Ok(false)
    }
}

struct ManagedLifecycleBackend {
    legacy: Arc<dyn ManagedReplicatorLifecycle>,
    common: CustomReplicatorHost,
}

#[async_trait]
impl ReplicatorLifecycleBackend for ManagedLifecycleBackend {
    fn owns_stream_session(&self) -> bool {
        true
    }

    async fn complete_open(&self, address: String) -> Result<()> {
        self.common.complete_open_common(address.clone()).await;
        self.legacy.complete_open(address).await
    }

    async fn fence_writes(&self) -> Result<()> {
        self.common.invalidate_build_attempts().await?;
        self.legacy.fence_writes().await
    }

    async fn settle_primary_prefix(&self) -> Result<()> {
        self.legacy.settle_primary_prefix().await
    }

    async fn cancel_configuration_work(&self) -> Result<()> {
        self.common.invalidate_build_attempts().await?;
        self.legacy.cancel_configuration_work().await
    }

    async fn restore_authority(&self) -> Result<()> {
        self.legacy.restore_authority().await?;
        self.common
            .restore_common_snapshot(self.legacy.snapshot().await)
            .await
    }

    async fn restore_access(&self, read: AccessStatus, write: AccessStatus) -> Result<()> {
        self.legacy
            .execute_action(RuntimeEffectAction::SetAccessStatus { read, write })
            .await
    }

    async fn execute_action(&self, action: RuntimeEffectAction) -> Result<()> {
        match &action {
            RuntimeEffectAction::RegisterPeerSession { identity, .. } => {
                let registered_engine_peer = self
                    .legacy
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
                            .any(|member| member.identity == *identity)
                    });
                if registered_engine_peer {
                    self.legacy.execute_action(action.clone()).await?;
                }
                self.common
                    .execute_common_build_action(action.clone())
                    .await?;
                return Ok(());
            }
            RuntimeEffectAction::AdmitAuthority(_) => {
                self.legacy.execute_action(action).await?;
                self.common
                    .restore_common_snapshot(self.legacy.snapshot().await)
                    .await?;
                return Ok(());
            }
            _ => {}
        }
        self.legacy.execute_action(action).await
    }

    async fn snapshot(&self) -> RuntimeSnapshot {
        self.legacy.snapshot().await
    }

    async fn cancel_outbound_build(&self, id: &OperationId) -> Result<()> {
        self.common.cancel_common_build(id).await?;
        self.legacy.cancel_outbound_build(id).await
    }

    async fn next_outbound(&self) -> Option<OutboundOperation> {
        tokio::select! {
            item = self.common.next_outbound() => item,
            item = self.legacy.next_outbound() => item,
        }
    }

    async fn select_build(&self, authority: &BuildAuthority) -> Result<()> {
        let _gate = self.common.gate.lock().await;
        self.common.select_build_inner(authority).await
    }

    async fn register_custom_peer_session(
        &self,
        identity: ReplicaIdentity,
        session: ProcessSessionId,
    ) -> Result<()> {
        self.execute_action(RuntimeEffectAction::RegisterPeerSession { identity, session })
            .await
    }

    async fn describe_custom_peer(&self, replica: ReplicaInformation) -> Result<()> {
        self.common.describe_peer(replica).await
    }

    async fn execute_custom_build(&self, replica: ReplicaInformation) -> Result<bool> {
        let mut replica = replica;
        self.common.prepare_build_description(&mut replica).await?;
        Ok(false)
    }

    async fn enqueue_custom_build(&self, endpoint: ReplicaEndpoint) -> Result<bool> {
        self.common.enqueue_build(endpoint).await?;
        Ok(true)
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

    pub(super) async fn execute_action(&self, action: RuntimeEffectAction) -> Result<()> {
        self.backend.execute_action(action).await
    }

    pub(super) async fn snapshot(&self) -> RuntimeSnapshot {
        self.backend.snapshot().await
    }

    pub(super) async fn cancel_outbound_build(&self, id: &OperationId) -> Result<()> {
        self.backend.cancel_outbound_build(id).await
    }

    pub(super) async fn next_outbound(&self) -> Option<OutboundOperation> {
        self.backend.next_outbound().await
    }

    pub(super) async fn select_build(&self, authority: &BuildAuthority) -> Result<()> {
        self.backend.select_build(authority).await
    }

    pub(super) async fn register_custom_peer_session(
        &self,
        identity: ReplicaIdentity,
        session: ProcessSessionId,
    ) -> Result<()> {
        self.backend
            .register_custom_peer_session(identity, session)
            .await
    }

    pub(super) async fn describe_custom_peer(&self, replica: ReplicaInformation) -> Result<()> {
        self.backend.describe_custom_peer(replica).await
    }

    pub(super) async fn refresh_custom_progress(&self) -> Result<()> {
        self.backend.refresh_custom_progress().await
    }

    pub(super) async fn execute_custom_build(&self, replica: ReplicaInformation) -> Result<bool> {
        self.backend.execute_custom_build(replica).await
    }

    pub(super) async fn enqueue_custom_build(&self, endpoint: ReplicaEndpoint) -> Result<bool> {
        self.backend.enqueue_custom_build(endpoint).await
    }
}

pub(super) struct CustomReplicatorHost {
    host: Weak<RuntimeHost>,
    control: Arc<dyn Replicator>,
    primary: Arc<dyn PrimaryReplicator>,
    native_receipts: bool,
    gate: Mutex<()>,
    state: RwLock<RuntimeSnapshot>,
    sessions: RwLock<BTreeMap<ReplicaIdentity, ProcessSessionId>>,
    addresses: RwLock<BTreeMap<ReplicaIdentity, (ProcessSessionId, String)>>,
    retired_sessions: RwLock<BTreeSet<(ReplicaIdentity, ProcessSessionId)>>,
    retired_builds: RwLock<BTreeSet<OperationId>>,
    build_generations: RwLock<BTreeMap<OperationId, u64>>,
    receipts: RwLock<BTreeMap<OperationId, BuildReceipt>>,
    configuration: RwLock<Option<ReplicaSetConfiguration>>,
    restored_access: RwLock<Option<(AccessStatus, AccessStatus)>>,
    removal_witnesses:
        RwLock<BTreeMap<ReplicaIdentity, kuberic_protocol::types::SecondaryRemovalWitness>>,
    outbound: mpsc::Sender<OutboundOperation>,
    receiver: Mutex<mpsc::Receiver<OutboundOperation>>,
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
            gate: Mutex::new(()),
            state: RwLock::new(snapshot),
            sessions: RwLock::default(),
            addresses: RwLock::default(),
            retired_sessions: RwLock::default(),
            retired_builds: RwLock::default(),
            build_generations: RwLock::default(),
            receipts: RwLock::default(),
            configuration: RwLock::default(),
            restored_access: RwLock::default(),
            removal_witnesses: RwLock::default(),
            outbound,
            receiver: Mutex::new(receiver),
        }
    }

    fn host(&self) -> Result<Arc<RuntimeHost>> {
        let host = self.host.upgrade().ok_or(RuntimeError::Closed)?;
        if host.aborted.load(std::sync::atomic::Ordering::Acquire) {
            return Err(RuntimeError::Closed);
        }
        Ok(host)
    }

    pub(super) async fn describe_peer(&self, replica: ReplicaInformation) -> Result<()> {
        self.execute_action(RuntimeEffectAction::RegisterPeerSession {
            identity: replica.identity.clone(),
            session: replica.process_session_id.clone(),
        })
        .await?;
        let _gate = self.gate.lock().await;
        if self.sessions.read().await.get(&replica.identity) != Some(&replica.process_session_id) {
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
        self.configure().await
    }

    pub(super) async fn select_build(&self, authority: &BuildAuthority) -> Result<()> {
        let _gate = self.gate.lock().await;
        self.select_build_inner(authority).await?;
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
        Ok(())
    }

    async fn receipt(&self, authority: &BuildAuthority) -> Result<BuildReceipt> {
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
        Ok(BuildReceipt {
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
            .configuration
            .read()
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

    async fn configure(&self) -> Result<()> {
        let Some(current) = self.descriptions().await? else {
            return Ok(());
        };
        let result = if let Some(previous) = self
            .state
            .read()
            .await
            .authority
            .as_ref()
            .and_then(|a| a.previous_configuration.clone())
        {
            self.primary
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
                .await
        } else {
            self.primary
                .update_current_replica_set_configuration(current.clone())
                .await
        };
        result?;
        *self.configuration.write().await = Some(current);
        Ok(())
    }

    async fn set_access(&self, read: AccessStatus, write: AccessStatus) -> Result<()> {
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
        if write == AccessStatus::Granted {
            if host.state.read().await.fallback_snapshot.role
                != kuberic_protocol::types::ReplicaRole::Primary
            {
                return Err(RuntimeError::NotPrimary);
            }
            let state = self.state.read().await;
            let authority = state
                .authority
                .as_ref()
                .ok_or(RuntimeError::AuthorityNotAdmitted)?;
            if authority.primary_identity() != &host.identity {
                return Err(RuntimeError::NotPrimary);
            }
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
        {
            let mut state = host.state.write().await;
            state.fallback_snapshot.read_status = read;
            state.fallback_snapshot.write_status = write;
        }
        // The application observes partition access while reporting progress.
        // Never publish an effect receipt before that observation has completed.
        if let Err(error) = self.control.current_progress().await {
            // The effect/configuration gate serializes platform ownership.
            // A custom replicator can also retire its own work while this call
            // is pending. SF cancellation is not authority to abort its successor.
            if matches!(error, RuntimeError::OperationCancelled) {
                return Err(error);
            }
            let mut state = host.state.write().await;
            state.fallback_snapshot.read_status = AccessStatus::ReconfigurationPending;
            state.fallback_snapshot.write_status = AccessStatus::ReconfigurationPending;
            drop(state);
            let mut state = self.state.write().await;
            state.read_status = AccessStatus::ReconfigurationPending;
            state.write_status = AccessStatus::ReconfigurationPending;
            drop(state);
            if matches!(error, RuntimeError::ReconfigurationPending) {
                return Err(error);
            }
            self.control.abort();
            return Err(error);
        }
        let mut state = self.state.write().await;
        state.read_status = read;
        state.write_status = write;
        if faulted_grant {
            Err(RuntimeError::ReconfigurationPending)
        } else {
            Ok(())
        }
    }

    async fn record_completion(&self, receipt: BuildReceipt) -> Result<()> {
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

    async fn refresh(&self) -> Result<()> {
        let host = self.host()?;
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
        if progress < 0 {
            return Err(RuntimeError::InvalidReplication(
                "negative custom replicator progress".into(),
            ));
        }
        let described = self.configuration.read().await.clone();
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
                && self.receipts.read().await.get(&build.build_id) != Some(&receipt)
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
        let mut generations = self.build_generations.write().await;
        let generation = generations.entry(endpoint.build_id.clone()).or_default();
        *generation = generation
            .checked_add(1)
            .ok_or(RuntimeError::OperationCancelled)?;
        drop(generations);
        self.outbound
            .try_send(OutboundOperation::Build(endpoint))
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

    async fn prepare_build(&self, replica: &mut ReplicaInformation) -> Result<BuildReceipt> {
        let authority = self.prepare_build_description(replica).await?;
        self.receipt(&authority).await
    }

    pub(super) async fn execute_build(&self, mut replica: ReplicaInformation) -> Result<()> {
        let receipt = self.prepare_build(&mut replica).await?;
        self.primary.build_replica(replica).await?;
        let _gate = self.gate.lock().await;
        self.record_completion(receipt).await?;
        self.refresh().await
    }

    async fn complete_open_common(&self, address: String) {
        let mut state = self.state.write().await;
        state.open = true;
        state.replication_address = Some(address);
    }

    async fn invalidate_build_attempts(&self) -> Result<()> {
        for generation in self.build_generations.write().await.values_mut() {
            *generation = generation
                .checked_add(1)
                .ok_or(RuntimeError::OperationCancelled)?;
        }
        Ok(())
    }

    async fn restore_common_snapshot(&self, snapshot: RuntimeSnapshot) -> Result<()> {
        {
            let mut state = self.state.write().await;
            state.open = snapshot.open;
            state.replication_address = snapshot.replication_address;
            state.authority = snapshot.authority;
            state.retired_authority = snapshot.retired_authority;
        }
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
                self.state
                    .write()
                    .await
                    .builds
                    .retain(|b| b.authority.build_id != id);
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
        self.state
            .write()
            .await
            .builds
            .retain(|b| &b.authority.build_id != id);
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

    pub(super) async fn complete_open(&self, address: String) -> Result<()> {
        self.complete_open_common(address).await;
        Ok(())
    }
    pub(super) async fn fence_writes(&self) -> Result<()> {
        *self.restored_access.write().await = None;
        self.removal_witnesses.write().await.clear();
        let configuration = self.configuration.read().await.clone();
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
        self.fence_writes().await
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
        self.configure().await?;
        self.refresh().await
    }
    pub(super) async fn execute_action(&self, action: RuntimeEffectAction) -> Result<()> {
        let _gate = self.gate.lock().await;
        let host = self.host()?;
        match action {
            RuntimeEffectAction::AdmitAuthority(authority) => {
                authority.validate()?;
                if let Some(evidence) = &authority.scale_up {
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
                        if build.source != intent.primary
                            || build.target != intent.target
                            || build.current_configuration != intent.previous_configuration
                            || build.replication_boundary_lsn != intent.snapshot_boundary_lsn
                            || self.receipts.read().await.get(&intent.build_id)
                                != Some(&self.receipt(&build).await?)
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
                self.fence_writes().await?;
                host.default_dependencies
                    .replica_authority_store
                    .admit(&authority)
                    .await?;
                self.state.write().await.authority = Some(*authority);
                self.configure().await?;
            }
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
                self.configure().await?;
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
                    self.fence_writes().await?;
                }
                self.configure().await?;
            }
            RuntimeEffectAction::RetireBuild(id) => {
                self.retired_builds.write().await.insert(id.clone());
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
            RuntimeEffectAction::SetAccessStatus { read, write } => {
                self.set_access(read, write).await?
            }
            RuntimeEffectAction::SetReadStatus(read) => {
                let write = self.state.read().await.write_status;
                self.set_access(read, write).await?;
            }
            RuntimeEffectAction::SetWriteStatus(write) => {
                let read = self.state.read().await.read_status;
                self.set_access(read, write).await?;
            }
            RuntimeEffectAction::WaitForCatchup => {
                self.primary
                    .wait_for_catch_up_quorum(ReplicaSetQuorumMode::WriteQuorum)
                    .await?;
                let boundary = self.control.current_progress().await?;
                let mut state = self.state.write().await;
                state.catch_up_boundary = Some(boundary);
                state.catch_up_complete = true;
            }
            RuntimeEffectAction::AuthorizeFailoverPrefix(boundary) => {
                let progress = self.control.current_progress().await?;
                if progress < boundary {
                    return Err(RuntimeError::ReconfigurationPending);
                }
                self.state.write().await.verified_replication_lsn = Some(boundary);
            }
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
        let receipts = self.receipts.read().await.clone();
        let mut current = Vec::new();
        for build in snapshot.builds {
            if let Some(receipt) = receipts.get(&build.authority.build_id)
                && self.receipt(&build.authority).await.as_ref().ok() == Some(receipt)
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
        if let Some(build) = self
            .host()?
            .default_dependencies
            .build_authority_store
            .load_build(id)
            .await?
        {
            self.primary.remove_replica(build.target.replica_id).await?;
        }
        self.configure().await
    }
    pub(super) async fn next_outbound(&self) -> Option<OutboundOperation> {
        self.receiver.lock().await.recv().await
    }
}

#[async_trait]
impl ReplicatorLifecycleBackend for CustomReplicatorHost {
    async fn complete_open(&self, address: String) -> Result<()> {
        CustomReplicatorHost::complete_open(self, address).await
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

    async fn execute_action(&self, action: RuntimeEffectAction) -> Result<()> {
        CustomReplicatorHost::execute_action(self, action).await
    }

    async fn snapshot(&self) -> RuntimeSnapshot {
        CustomReplicatorHost::snapshot(self).await
    }

    async fn cancel_outbound_build(&self, id: &OperationId) -> Result<()> {
        CustomReplicatorHost::cancel_outbound_build(self, id).await
    }

    async fn next_outbound(&self) -> Option<OutboundOperation> {
        CustomReplicatorHost::next_outbound(self).await
    }

    async fn select_build(&self, authority: &BuildAuthority) -> Result<()> {
        CustomReplicatorHost::select_build(self, authority).await
    }

    async fn register_custom_peer_session(
        &self,
        identity: ReplicaIdentity,
        session: ProcessSessionId,
    ) -> Result<()> {
        CustomReplicatorHost::execute_action(
            self,
            RuntimeEffectAction::RegisterPeerSession { identity, session },
        )
        .await
    }

    async fn describe_custom_peer(&self, replica: ReplicaInformation) -> Result<()> {
        CustomReplicatorHost::describe_peer(self, replica).await
    }

    async fn refresh_custom_progress(&self) -> Result<()> {
        if !CustomReplicatorHost::snapshot(self).await.open {
            return Ok(());
        }
        CustomReplicatorHost::execute_action(self, RuntimeEffectAction::RefreshApplicationProgress)
            .await
    }

    async fn execute_custom_build(&self, replica: ReplicaInformation) -> Result<bool> {
        CustomReplicatorHost::execute_build(self, replica).await?;
        Ok(true)
    }

    async fn enqueue_custom_build(&self, endpoint: ReplicaEndpoint) -> Result<bool> {
        CustomReplicatorHost::enqueue_build(self, endpoint).await?;
        Ok(true)
    }
}

fn unavailable<T>() -> Result<T> {
    Err(RuntimeError::Application(
        "the selected replicator does not support this managed operation".into(),
    ))
}
