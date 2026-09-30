//! Private hosting of service-created SF replicators without operation/copy streams.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Weak};

use async_trait::async_trait;
use kuberic_protocol::types::{AccessStatus, OperationId, ProcessSessionId, ReplicaIdentity};
use kuberic_runtime::application::{ClientWrite, Lsn};
use kuberic_runtime::internal::{PendingReplication, PendingWrite};
use kuberic_runtime::replicator::copy::{PrepareCopyRequest, PreparedCopy};
use kuberic_runtime::replicator::{
    ManagedReplicator, PrimaryReplicator, ReplicaInformation, ReplicaSetConfiguration,
    ReplicaSetQuorumMode, Replicator,
};
use kuberic_runtime::{Result, RuntimeError};
use kuberic_runtime_internal::authority::{BuildAuthority, DurableBuildProgress};
use kuberic_runtime_internal::effects::{BuildPostcondition, RuntimeEffectAction, RuntimeSnapshot};
use kuberic_runtime_internal::transport::{
    CopyAck, CopyItem, OutboundOperation, ReplicaEndpoint, ReplicationAck, ReplicationItem,
};
use tokio::sync::{Mutex, RwLock, mpsc};

use super::{RuntimeHost, empty_snapshot};

pub(super) struct CustomReplicatorHost {
    host: Weak<RuntimeHost>,
    control: Arc<dyn Replicator>,
    primary: Arc<dyn PrimaryReplicator>,
    gate: Mutex<()>,
    state: RwLock<RuntimeSnapshot>,
    sessions: RwLock<BTreeMap<ReplicaIdentity, ProcessSessionId>>,
    retired_sessions: RwLock<BTreeSet<(ReplicaIdentity, ProcessSessionId)>>,
    retired_builds: RwLock<BTreeSet<OperationId>>,
    build_generations: RwLock<BTreeMap<OperationId, u64>>,
    configuration: RwLock<Option<ReplicaSetConfiguration>>,
    outbound: mpsc::Sender<OutboundOperation>,
    receiver: Mutex<mpsc::Receiver<OutboundOperation>>,
}

impl CustomReplicatorHost {
    pub(super) fn new(
        host: Weak<RuntimeHost>,
        control: Arc<dyn Replicator>,
        primary: Arc<dyn PrimaryReplicator>,
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
            gate: Mutex::new(()),
            state: RwLock::new(snapshot),
            sessions: RwLock::default(),
            retired_sessions: RwLock::default(),
            retired_builds: RwLock::default(),
            build_generations: RwLock::default(),
            configuration: RwLock::default(),
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

    async fn descriptions(&self) -> Result<Option<ReplicaSetConfiguration>> {
        let host = self.host()?;
        let builds = host
            .default_dependencies
            .build_authority_store
            .load_builds()
            .await?;
        let authority = self.state.read().await.authority.clone();
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
        let (_, local_session) = host
            .replica_session
            .get()
            .ok_or(RuntimeError::AuthorityNotAdmitted)?;
        sessions.insert(host.identity.clone(), local_session.clone());
        let address = self
            .state
            .read()
            .await
            .replication_address
            .clone()
            .unwrap_or_default();
        let mut replicas = configuration
            .members
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
            if sessions.get(&build.source).is_none_or(|s| s.is_empty()) {
                continue;
            }
            let Some(target_session) = sessions.get(&build.target).filter(|s| !s.is_empty()) else {
                continue;
            };
            if !replicas.iter().any(|r| r.identity == build.source) {
                continue;
            }
            let mut target =
                ReplicaInformation::new(build.build_id, build.target.clone(), String::new());
            target.process_session_id = target_session.clone();
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
            local.process_session_id = local_session.clone();
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
                .update_catch_up_replica_set_configuration(current.clone(), previous.into())
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
        let host = self.host()?;
        if write == AccessStatus::Granted {
            let state = self.state.read().await;
            let authority = state
                .authority
                .as_ref()
                .ok_or(RuntimeError::AuthorityNotAdmitted)?;
            if authority.primary_identity() != &host.identity {
                return Err(RuntimeError::NotPrimary);
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
            let mut state = host.state.write().await;
            state.fallback_snapshot.read_status = AccessStatus::ReconfigurationPending;
            state.fallback_snapshot.write_status = AccessStatus::ReconfigurationPending;
            drop(state);
            self.control.abort();
            return Err(error);
        }
        let mut state = self.state.write().await;
        state.read_status = read;
        state.write_status = write;
        Ok(())
    }

    async fn record_completion(&self, authority: BuildAuthority) -> Result<()> {
        let host = self.host()?;
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
            .record_build_progress(&progress)
            .await?;
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
        let progress = self.control.current_progress().await?;
        if progress < 0 {
            return Err(RuntimeError::InvalidReplication(
                "negative custom replicator progress".into(),
            ));
        }
        let host = self.host()?;
        let builds = host
            .default_dependencies
            .build_authority_store
            .load_builds()
            .await?;
        let described = self.configuration.read().await.clone();
        for build in builds {
            if self.retired_builds.read().await.contains(&build.build_id)
                || described.as_ref().is_none_or(|c| {
                    c.configuration != build.current_configuration
                        || !c
                            .replicas
                            .iter()
                            .any(|r| r.build_id == build.build_id && r.identity == build.target)
                })
            {
                continue;
            }
            if build.target == host.identity
                && progress > 0
                && progress >= build.replication_boundary_lsn
                && self.sessions.read().await.contains_key(&build.source)
                && !self
                    .state
                    .read()
                    .await
                    .builds
                    .iter()
                    .any(|b| b.authority == build)
            {
                self.record_completion(build).await?;
            }
        }
        let mut state = self.state.write().await;
        state.current_progress = progress;
        state.committed_lsn = progress;
        state.current_configuration_quorum_progress = progress;
        Ok(())
    }

    pub(super) async fn enqueue_build(&self, endpoint: ReplicaEndpoint) -> Result<()> {
        self.outbound
            .send(OutboundOperation::Build(endpoint))
            .await
            .map_err(|_| RuntimeError::Closed)
    }

    pub(super) async fn execute_build(&self, mut replica: ReplicaInformation) -> Result<()> {
        let host = self.host()?;
        let (authority, sessions, generation) = {
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
            let mut generations = self.build_generations.write().await;
            let generation = generations.entry(replica.build_id.clone()).or_default();
            *generation = generation
                .checked_add(1)
                .ok_or(RuntimeError::OperationCancelled)?;
            (authority, sessions, *generation)
        };
        self.primary.build_replica(replica).await?;
        let _gate = self.gate.lock().await;
        if *self.sessions.read().await != sessions
            || self.build_generations.read().await.get(&authority.build_id) != Some(&generation)
        {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        self.record_completion(authority).await?;
        self.refresh().await
    }
}

#[async_trait]
impl ManagedReplicator for CustomReplicatorHost {
    async fn attach_interfaces(
        &self,
        _: Arc<dyn Replicator>,
        _: Option<Arc<dyn PrimaryReplicator>>,
    ) -> Result<()> {
        Ok(())
    }
    async fn complete_open(&self, address: String) -> Result<()> {
        let mut state = self.state.write().await;
        state.open = true;
        state.replication_address = Some(address);
        Ok(())
    }
    async fn fence_writes(&self) -> Result<()> {
        self.set_access(
            AccessStatus::ReconfigurationPending,
            AccessStatus::ReconfigurationPending,
        )
        .await
    }
    async fn settle_primary_prefix(&self) -> Result<()> {
        self.refresh().await
    }
    async fn cancel_configuration_work(&self) -> Result<()> {
        let _gate = self.gate.lock().await;
        for generation in self.build_generations.write().await.values_mut() {
            *generation = generation
                .checked_add(1)
                .ok_or(RuntimeError::OperationCancelled)?;
        }
        self.fence_writes().await?;
        self.control
            .update_epoch(
                self.state
                    .read()
                    .await
                    .authority
                    .as_ref()
                    .map_or_else(Default::default, |a| a.current_configuration.epoch),
            )
            .await
    }
    async fn restore_authority(&self) -> Result<()> {
        let _gate = self.gate.lock().await;
        self.state.write().await.authority = self
            .host()?
            .default_dependencies
            .replica_authority_store
            .load()
            .await?;
        self.configure().await?;
        self.refresh().await
    }
    async fn recover_pending_writes(&self) -> Result<()> {
        Ok(())
    }
    async fn repair_peer(&self, _: ReplicaIdentity, _: Lsn) -> Result<()> {
        unavailable()
    }
    async fn execute_action(&self, action: RuntimeEffectAction) -> Result<()> {
        let _gate = self.gate.lock().await;
        let host = self.host()?;
        match action {
            RuntimeEffectAction::AdmitAuthority(authority) => {
                authority.validate()?;
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
            RuntimeEffectAction::RefreshApplicationProgress => self.refresh().await?,
            _ => return unavailable(),
        }
        Ok(())
    }
    async fn snapshot(&self) -> RuntimeSnapshot {
        self.state.read().await.clone()
    }
    async fn cancel_outbound_build(&self, id: &OperationId) -> Result<()> {
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
        Ok(())
    }
    fn abort(&self) {
        self.control.abort();
    }
    async fn begin_write(&self, _: ClientWrite) -> Result<PendingWrite> {
        unavailable()
    }
    async fn accept_acknowledgement(&self, _: ReplicationAck) -> Result<()> {
        unavailable()
    }
    async fn prepare_copy(&self, _: PrepareCopyRequest) -> Result<PreparedCopy> {
        unavailable()
    }
    async fn accept_copy_acknowledgement(&self, _: CopyAck) -> Result<()> {
        unavailable()
    }
    async fn receive_copy_item(&self, _: CopyItem) -> Result<CopyAck> {
        unavailable()
    }
    async fn receive_replication(&self, _: ReplicationItem) -> Result<PendingReplication> {
        unavailable()
    }
    async fn next_outbound(&self) -> Option<OutboundOperation> {
        self.receiver.lock().await.recv().await
    }
}

fn unavailable<T>() -> Result<T> {
    Err(RuntimeError::Application(
        "the selected replicator does not support this managed operation".into(),
    ))
}
