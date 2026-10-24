//! PostgreSQL's private two-ended session owner. Discovery is read-only.
use super::*;
use kuberic_runtime::protocol::public_operations::{
    ORDINARY_ACTION_FORMAT, RemoteReplicaSelection, ReplicaDiscoveryQuery, ReplicaPublication,
    ReplicaPublicationStage,
};

pub(super) const SESSION_LIMIT: usize = 256;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct PgSessionId {
    pub(super) nonce: [u8; 32],
    pub(super) generation: u64,
    pub(super) source: ReplicaPublication,
    pub(super) target: ReplicaPublication,
}

impl PgSessionId {
    fn validate(&self) -> Result<()> {
        if self.generation == 0 || self.nonce == [0; 32] {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        RemoteReplicaSelection {
            source: self.source.clone(),
            target: self.target.clone(),
            target_control_address: self.target.replication_address.clone(),
            generation: self.generation,
        }
        .validate()
        .map_err(application_error)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) enum SessionAction {
    Reserve,
    Activate,
    Confirm,
    Complete,
    Retire,
    Cancel,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) enum SessionStage {
    Proposed,
    Active,
    Reserved,
    Receiving,
    Completed,
    Retiring,
    Retired,
    Quarantined,
}

impl SessionStage {
    fn live(self) -> bool {
        matches!(
            self,
            Self::Proposed | Self::Active | Self::Reserved | Self::Receiving | Self::Completed
        )
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SessionRequest {
    pub(super) session: PgSessionId,
    pub(super) action: SessionAction,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SessionReply {
    pub(super) session: PgSessionId,
    pub(super) responder: ReplicaPublication,
    pub(super) action: SessionAction,
    pub(super) stage: SessionStage,
}

struct Session {
    id: PgSessionId,
    stage: SessionStage,
    effect_generation: u64,
    cancellation: CancellationToken,
    joins: Vec<crate::owned_task::PgTask>,
    cleanup: Option<std::result::Result<(), String>>,
    retirement: Option<CloseOwner>,
    reservation: Option<CloseOwner>,
    remote_retirement: Option<CloseOwner>,
    remote_settled: bool,
    native_copy_complete: bool,
    membership: Option<NativeMembership>,
    native_owner: Option<Arc<InboundOwner>>,
    source_generation: u64,
    target_generation: u64,
    membership_worker: Option<crate::owned_task::PgTask>,
    native_failure: Option<String>,
    native_lineage: Option<PgLineage>,
    native_gateway: Option<Arc<std::sync::atomic::AtomicBool>>,
    publication_serial: Arc<Mutex<()>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativeMembership {
    pub(super) current: ConfigurationDescriptor,
    pub(super) previous: ConfigurationDescriptor,
    pub(super) current_only: bool,
    pub(super) installed: bool,
    pub(super) achieved: bool,
    source_current_seen: bool,
    target_current_seen: bool,
}

#[derive(Default)]
pub(super) struct PgSessions {
    gate: std::sync::Mutex<Sessions>,
}

#[derive(Default)]
struct Sessions {
    fenced: bool,
    blocked: bool,
    generation: u64,
    entries: BTreeMap<[u8; 32], Session>,
    receiver: Option<[u8; 32]>,
}

impl PgSessions {
    pub(super) fn fence(&self) {
        let mut state = self.gate.lock().unwrap();
        state.fenced = true;
        Self::invalidate_locked(&mut state);
    }

    pub(super) fn invalidate(&self) {
        let mut state = self.gate.lock().unwrap();
        state.blocked = true;
        Self::invalidate_locked(&mut state);
    }

    pub(super) fn resume(&self) {
        self.gate.lock().unwrap().blocked = false;
    }

    fn invalidate_locked(state: &mut Sessions) {
        for entry in state.entries.values_mut() {
            if entry.stage.live() {
                entry.stage = SessionStage::Retiring;
                entry.effect_generation = entry.effect_generation.saturating_add(1);
                entry.cancellation.cancel();
            }
        }
    }

    fn record<'a>(
        state: &'a mut Sessions,
        id: &PgSessionId,
        stage: SessionStage,
    ) -> Result<&'a mut Session> {
        if state.entries.contains_key(&id.nonce) {
            let entry = state.entries.get_mut(&id.nonce).unwrap();
            if &entry.id != id {
                return Err(RuntimeError::AuthorityNotAdmitted);
            }
            return Ok(entry);
        }
        if state.entries.len() >= SESSION_LIMIT {
            return Err(application_error(
                "PostgreSQL session creation exhausted; tombstones retained",
            ));
        }
        state.entries.insert(
            id.nonce,
            Session {
                id: id.clone(),
                stage,
                effect_generation: 1,
                cancellation: CancellationToken::new(),
                joins: Vec::new(),
                cleanup: None,
                retirement: None,
                reservation: None,
                remote_retirement: None,
                remote_settled: false,
                native_copy_complete: false,
                membership: None,
                native_owner: None,
                source_generation: id.source.generation,
                target_generation: id.target.generation,
                membership_worker: None,
                native_failure: None,
                native_lineage: None,
                native_gateway: None,
                publication_serial: Arc::new(Mutex::new(())),
            },
        );
        Ok(state.entries.get_mut(&id.nonce).unwrap())
    }

    pub(super) fn lease(&self, id: &PgSessionId) -> Result<(u64, CancellationToken)> {
        let mut state = self.gate.lock().unwrap();
        if state.fenced || state.blocked {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        let entry = state
            .entries
            .get_mut(&id.nonce)
            .ok_or(RuntimeError::AuthorityNotAdmitted)?;
        if &entry.id != id || !entry.stage.live() || entry.cancellation.is_cancelled() {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        if entry
            .native_gateway
            .as_ref()
            .is_some_and(|running| !running.load(std::sync::atomic::Ordering::Acquire))
        {
            entry.stage = SessionStage::Quarantined;
            entry.effect_generation = entry.effect_generation.saturating_add(1);
            entry.cancellation.cancel();
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        Ok((entry.effect_generation, entry.cancellation.clone()))
    }

    pub(super) fn revalidate(&self, id: &PgSessionId, generation: u64) -> Result<()> {
        let (current, _) = self.lease(id)?;
        if current != generation {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        Ok(())
    }
}

impl PgReplicator {
    pub(super) fn pg_bind_native_gateway(
        &self,
        id: &PgSessionId,
        running: Arc<std::sync::atomic::AtomicBool>,
    ) -> Result<()> {
        self.public_owner().sessions.lease(id)?;
        let mut sessions = self.public_owner().sessions.gate.lock().unwrap();
        let entry = sessions.entries.get_mut(&id.nonce).unwrap();
        if entry.id != *id
            || entry.stage != SessionStage::Active
            || entry.cancellation.is_cancelled()
            || entry.native_gateway.is_some()
        {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        entry.native_gateway = Some(running);
        Ok(())
    }
    async fn pg_native_lineage_live(&self, id: &PgSessionId, generation: u64) -> Result<()> {
        let lineage = self
            .public_owner()
            .sessions
            .gate
            .lock()
            .unwrap()
            .entries
            .get(&id.nonce)
            .and_then(|entry| entry.native_lineage.clone());
        if let Some(lineage) = lineage {
            let _access = self.instance.access_lock.lock().await;
            self.public_owner().sessions.revalidate(id, generation)?;
            if !self.instance.is_running().await {
                return Err(RuntimeError::AuthorityNotAdmitted);
            }
            self.public_validate_history().await?;
            if self.public_lineage().await? != lineage {
                return Err(RuntimeError::AuthorityNotAdmitted);
            }
            self.public_owner().sessions.revalidate(id, generation)?;
        }
        Ok(())
    }

    pub(super) fn pg_bind_native_lineage(
        &self,
        id: &PgSessionId,
        lineage: &PgLineage,
    ) -> Result<()> {
        self.public_owner().sessions.lease(id)?;
        let mut sessions = self.public_owner().sessions.gate.lock().unwrap();
        let entry = sessions.entries.get_mut(&id.nonce).unwrap();
        if entry
            .native_lineage
            .as_ref()
            .is_some_and(|old| old != lineage)
        {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        entry.native_lineage = Some(lineage.clone());
        Ok(())
    }
    #[cfg(all(test, feature = "testing"))]
    pub(super) fn pg_session_ownership(&self, id: &PgSessionId) -> (SessionStage, bool, bool) {
        let sessions = self.public_owner().sessions.gate.lock().unwrap();
        let entry = &sessions.entries[&id.nonce];
        (
            entry.stage,
            sessions.receiver == Some(id.nonce),
            entry.remote_settled,
        )
    }
    pub(super) fn pg_session_ids_for_target(&self, target: ReplicaId) -> Vec<PgSessionId> {
        self.public_owner()
            .sessions
            .gate
            .lock()
            .unwrap()
            .entries
            .values()
            .filter(|entry| {
                entry.id.source.scope.storage.replica == self.public_owner().fence.local.identity
                    && entry.id.target.scope.storage.replica.replica_id == target
                    && !entry.remote_settled
            })
            .map(|entry| entry.id.clone())
            .collect()
    }
    pub(super) fn pg_native_inbounds(&self) -> Vec<Arc<InboundOwner>> {
        self.public_owner()
            .sessions
            .gate
            .lock()
            .unwrap()
            .entries
            .values()
            .filter_map(|entry| entry.native_owner.clone())
            .collect()
    }

    pub(super) fn pg_native_inbound(
        &self,
        id: &PgSessionId,
        start: impl FnOnce() -> Arc<InboundOwner>,
    ) -> Result<(Arc<InboundOwner>, bool)> {
        let mut sessions = self.public_owner().sessions.gate.lock().unwrap();
        if sessions.fenced || sessions.blocked || sessions.receiver != Some(id.nonce) {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        let entry = sessions
            .entries
            .get_mut(&id.nonce)
            .filter(|entry| {
                entry.id == *id
                    && matches!(
                        entry.stage,
                        SessionStage::Receiving | SessionStage::Completed
                    )
                    && !entry.cancellation.is_cancelled()
            })
            .ok_or(RuntimeError::AuthorityNotAdmitted)?;
        match entry.native_owner.as_ref() {
            Some(owner) => Ok((owner.clone(), true)),
            None => {
                let owner = start();
                entry.native_owner = Some(owner.clone());
                Ok((owner, false))
            }
        }
    }

    async fn pg_cleanup_native_session(&self, id: &PgSessionId) -> Result<()> {
        let mut failures = Vec::new();
        let local_source =
            id.source.scope.storage.replica == self.public_owner().fence.local.identity;
        if !local_source {
            let native = self
                .public_owner()
                .sessions
                .gate
                .lock()
                .unwrap()
                .entries
                .get(&id.nonce)
                .and_then(|entry| entry.native_owner.clone());
            if let Some(owner) = native {
                if let Err(error) = self.public_join_inbound_owner(&owner).await {
                    failures.push(error.to_string());
                }
                let _serial = self.public_owner().inbound_serial.lock().await;
                let mut state = self.public_owner().state.lock().await;
                if let Err(error) = self.instance.stop().await {
                    failures.push(error.to_string());
                }
                state.receiver = None;
                state.receiver_boundary = None;
                state.access = None;
            }
        } else {
            let mut state = self.public_owner().state.lock().await;
            let mut retired = Vec::new();
            let mut index = 0;
            while index < state.grants.len() {
                if state.grants[index].binding.session() == Some(id) {
                    retired.push(state.grants.remove(index));
                } else {
                    index += 1;
                }
            }
            for owner in self.public_retain_grant_drains(retired) {
                if let Err(error) = self.public_wait_gateway_drain(&owner).await {
                    failures.push(error.to_string());
                }
            }
            state
                .built_idle
                .retain(|_, build| build.session.as_deref() != Some(id));
            if failures.is_empty()
                && self.instance.is_running().await
                && let Err(error) = self.public_reconcile_slots(&state).await
            {
                failures.push(error.to_string());
            }
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(application_error(failures.join("; ")))
        }
    }
    pub(super) fn pg_native_copy_completed(&self, id: &PgSessionId) -> Result<()> {
        let mut sessions = self.public_owner().sessions.gate.lock().unwrap();
        if sessions.fenced || sessions.blocked {
            return Err(RuntimeError::Closed);
        }
        let entry = sessions
            .entries
            .get_mut(&id.nonce)
            .filter(|entry| {
                entry.id == *id && entry.stage.live() && !entry.cancellation.is_cancelled()
            })
            .ok_or(RuntimeError::AuthorityNotAdmitted)?;
        entry.native_copy_complete = true;
        if id.target.scope.storage.replica == self.public_owner().fence.local.identity {
            entry.stage = SessionStage::Completed;
        }
        Ok(())
    }

    pub(super) fn pg_native_session_for_target(
        &self,
        replica: &ReplicaInformation,
    ) -> Result<PgSessionId> {
        let sessions = self.public_owner().sessions.gate.lock().unwrap();
        if sessions.fenced || sessions.blocked {
            return Err(RuntimeError::Closed);
        }
        sessions
            .entries
            .values()
            .find(|entry| {
                entry.stage == SessionStage::Active
                    && entry.native_copy_complete
                    && entry.id.target.scope.storage.replica == replica.identity
                    && entry.id.target.scope.process_session_id == replica.process_session_id
                    && entry.id.target.replication_address == replica.replication_address
                    && replica.storage_incarnation.as_ref() == Some(&entry.id.target.scope.storage)
                    && !entry.cancellation.is_cancelled()
            })
            .map(|entry| entry.id.clone())
            .ok_or(RuntimeError::AuthorityNotAdmitted)
    }

    pub(super) fn pg_native_membership(
        &self,
        id: &PgSessionId,
    ) -> Result<Option<NativeMembership>> {
        self.public_owner().sessions.lease(id)?;
        let sessions = self.public_owner().sessions.gate.lock().unwrap();
        let entry = &sessions.entries[&id.nonce];
        if !entry.native_copy_complete {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        Ok(entry.membership.clone())
    }

    pub(super) fn pg_install_membership(
        &self,
        id: &PgSessionId,
        current: &ConfigurationDescriptor,
        previous: Option<&ConfigurationDescriptor>,
        installed: bool,
    ) -> Result<()> {
        self.public_owner().sessions.lease(id)?;
        let mut sessions = self.public_owner().sessions.gate.lock().unwrap();
        let entry = sessions.entries.get_mut(&id.nonce).unwrap();
        if !entry.native_copy_complete || !entry.stage.live() {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        let predecessor = &id.source.scope.configuration.current;
        let valid = current.epoch == id.source.scope.epoch
            && current.primary_id == predecessor.primary_id
            && current.members.iter().any(|member| {
                member.identity == id.target.scope.storage.replica
                    && member.role == ReplicaRole::ActiveSecondary
            })
            && current.members.iter().any(|member| {
                member.identity == id.source.scope.storage.replica
                    && member.role == ReplicaRole::Primary
            });
        if !valid {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        match &mut entry.membership {
            None if previous == Some(predecessor) => {
                entry.membership = Some(NativeMembership {
                    current: current.clone(),
                    previous: predecessor.clone(),
                    current_only: false,
                    installed,
                    achieved: false,
                    source_current_seen: false,
                    target_current_seen: false,
                });
            }
            Some(membership)
                if membership.current == *current
                    && (previous == Some(&membership.previous) || previous.is_none()) =>
            {
                if previous.is_some() && membership.current_only {
                    return Err(RuntimeError::AuthorityNotAdmitted);
                }
                if previous.is_none() {
                    membership.current_only = true;
                }
                membership.installed = installed;
            }
            _ => return Err(RuntimeError::AuthorityNotAdmitted),
        }
        Ok(())
    }

    pub(super) fn pg_native_handover_allowed(&self, epoch: Epoch) -> bool {
        let sessions = self.public_owner().sessions.gate.lock().unwrap();
        !sessions.fenced
            && !sessions.blocked
            && sessions.receiver.is_some_and(|nonce| {
                let entry = &sessions.entries[&nonce];
                entry.stage == SessionStage::Completed
                    && entry.native_copy_complete
                    && entry.id.target.scope.epoch == epoch
                    && !entry.cancellation.is_cancelled()
            })
    }

    pub(super) async fn pg_validate_native_session(
        &self,
        binding: &TransportBinding,
        continuation: bool,
    ) -> Result<()> {
        let id = binding
            .session()
            .ok_or(RuntimeError::AuthorityNotAdmitted)?;
        let (generation, cancellation) = self.public_owner().sessions.lease(id)?;
        {
            let sessions = self.public_owner().sessions.gate.lock().unwrap();
            let entry = &sessions.entries[&id.nonce];
            let expected =
                if self.public_owner().fence.local.identity == id.source.scope.storage.replica {
                    entry.stage == SessionStage::Active
                } else {
                    matches!(
                        entry.stage,
                        SessionStage::Receiving | SessionStage::Completed
                    )
                };
            if !expected {
                return Err(RuntimeError::AuthorityNotAdmitted);
            }
            if !continuation && entry.membership.is_some() {
                return Err(RuntimeError::AuthorityNotAdmitted);
            }
            if let TransportBinding::Peer(peer) = binding {
                let membership = entry
                    .membership
                    .as_ref()
                    .ok_or(RuntimeError::AuthorityNotAdmitted)?;
                if peer.current_configuration_id != membership.current.configuration_id
                    || peer.previous_configuration_id
                        != (!membership.current_only)
                            .then(|| membership.previous.configuration_id.clone())
                {
                    return Err(RuntimeError::AuthorityNotAdmitted);
                }
            }
        }
        let source = &id.source;
        let target = &id.target;
        if let TransportBinding::Build(build) = binding {
            if build.resource_uid != source.scope.history.resource_uid
                || build.source.role != ReplicaRole::Primary
                || build.target.role != ReplicaRole::IdleSecondary
            {
                return Err(RuntimeError::AuthorityNotAdmitted);
            }
        } else if let TransportBinding::Peer(peer) = binding
            && (peer.primary.role != ReplicaRole::Primary
                || peer.secondary.role != ReplicaRole::ActiveSecondary)
        {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        if binding.source() != &source.scope.storage.replica
            || binding.target() != &target.scope.storage.replica
            || binding.source_endpoint().process_session_id != source.scope.process_session_id
            || binding.target_endpoint().process_session_id != target.scope.process_session_id
            || binding.source_address() != source.replication_address
            || binding.target_address() != target.replication_address
            || binding.epoch() != source.scope.epoch
        {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        if continuation {
            self.pg_session_publications(id).await?;
        } else {
            self.pg_exact_session_publications(id).await?;
        }
        self.public_owner().sessions.revalidate(id, generation)?;
        if self.public_owner().fence.local.identity == source.scope.storage.replica {
            self.pg_native_lineage_live(id, generation).await?;
        }
        if self.public_owner().fence.local.identity == target.scope.storage.replica {
            let reply = self
                .public_read(
                    &cancellation,
                    self.pg_session_rpc(id, SessionAction::Confirm, true),
                )
                .await?;
            if reply.stage != SessionStage::Active {
                return Err(RuntimeError::AuthorityNotAdmitted);
            }
            if continuation {
                self.pg_session_publications(id).await?;
            } else {
                self.pg_exact_session_publications(id).await?;
            }
        }
        self.public_owner().sessions.revalidate(id, generation)
    }

    pub(super) async fn pg_prepare_build_session(
        &self,
        replica: &ReplicaInformation,
    ) -> Result<PgSessionId> {
        let host = self
            .public_owner()
            .host
            .get()
            .ok_or(RuntimeError::AuthorityNotAdmitted)?;
        let source = host
            .local_publication()
            .await?
            .ok_or(RuntimeError::AuthorityNotAdmitted)?;
        let target = host
            .discover_peer(&ReplicaDiscoveryQuery {
                format: ORDINARY_ACTION_FORMAT,
                history: source.scope.history.clone(),
                replica: replica.identity.clone(),
                process_session_id: Some(replica.process_session_id.clone()),
            })
            .await?
            .ok_or(RuntimeError::AuthorityNotAdmitted)?;
        if replica.identity != target.scope.storage.replica
            || replica.process_session_id != target.scope.process_session_id
            || replica.replication_address != target.replication_address
            || replica.storage_incarnation.as_ref() != Some(&target.scope.storage)
            || replica.role != ReplicaRole::IdleSecondary
        {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        let existing = self
            .public_owner()
            .sessions
            .gate
            .lock()
            .unwrap()
            .entries
            .values()
            .find(|entry| {
                entry.id.source == source && entry.id.target == target && entry.stage.live()
            })
            .map(|entry| entry.id.clone());
        let id = match existing {
            Some(id) => id,
            None => self.pg_propose_session(source, target).await?,
        };
        self.pg_establish_session(&id).await?;
        Ok(id)
    }

    async fn pg_exact_session_publications(&self, id: &PgSessionId) -> Result<()> {
        id.validate()?;
        self.public_attempt_live(&self.public_owner().work.lock().unwrap().clone())?;
        let context = self.public_owner().fence.context()?;
        let local = if id.source.scope.storage.replica == context.identity {
            &id.source
        } else if id.target.scope.storage.replica == context.identity {
            &id.target
        } else {
            return Err(RuntimeError::AuthorityNotAdmitted);
        };
        if local.scope.process_session_id != self.public_owner().fence.local.process_session_id
            || local.replication_address != self.public_owner().fence.local.replication_address
            || context.role != local.role
            || context.epoch != local.scope.epoch
        {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        #[cfg(all(test, feature = "testing"))]
        if let Some((source, target)) = self
            .public_owner()
            .session_publications
            .lock()
            .unwrap()
            .as_ref()
        {
            if source == &id.source && target == &id.target {
                return Ok(());
            }
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        let host = self
            .public_owner()
            .host
            .get()
            .ok_or(RuntimeError::AuthorityNotAdmitted)?;
        let current = host
            .local_publication()
            .await?
            .ok_or(RuntimeError::AuthorityNotAdmitted)?;
        if &current != local || current.stage != ReplicaPublicationStage::AchievedScope {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        let peer = if local == &id.source {
            &id.target
        } else {
            &id.source
        };
        let observed = host
            .discover_peer(&ReplicaDiscoveryQuery {
                format: ORDINARY_ACTION_FORMAT,
                history: peer.scope.history.clone(),
                replica: peer.scope.storage.replica.clone(),
                process_session_id: Some(peer.scope.process_session_id.clone()),
            })
            .await?
            .ok_or(RuntimeError::AuthorityNotAdmitted)?;
        if observed != *peer {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        Ok(())
    }

    async fn pg_session_publications(&self, id: &PgSessionId) -> Result<()> {
        let serial = self
            .public_owner()
            .sessions
            .gate
            .lock()
            .unwrap()
            .entries
            .get(&id.nonce)
            .filter(|entry| entry.id == *id)
            .map(|entry| entry.publication_serial.clone());
        let _serial = match serial.as_ref() {
            Some(serial) => Some(serial.lock().await),
            None => None,
        };
        let continuing = self
            .public_owner()
            .sessions
            .gate
            .lock()
            .unwrap()
            .entries
            .get(&id.nonce)
            .is_some_and(|entry| entry.membership.is_some());
        if !continuing && self.pg_exact_session_publications(id).await.is_ok() {
            return Ok(());
        }
        id.validate()?;
        let (generation, _) = self.public_owner().sessions.lease(id)?;
        let context = self.public_owner().fence.context()?;
        let source_local = context.identity == id.source.scope.storage.replica;
        if context.epoch != id.source.scope.epoch
            || (source_local && context.role != ReplicaRole::Primary)
            || (!source_local
                && !matches!(
                    context.role,
                    ReplicaRole::IdleSecondary | ReplicaRole::ActiveSecondary
                ))
        {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        let host = self
            .public_owner()
            .host
            .get()
            .ok_or(RuntimeError::AuthorityNotAdmitted)?;
        let local = host
            .local_publication()
            .await?
            .ok_or(RuntimeError::AuthorityNotAdmitted)?;
        let peer_origin = if source_local { &id.target } else { &id.source };
        let peer = host
            .discover_peer(&ReplicaDiscoveryQuery {
                format: ORDINARY_ACTION_FORMAT,
                history: peer_origin.scope.history.clone(),
                replica: peer_origin.scope.storage.replica.clone(),
                process_session_id: Some(peer_origin.scope.process_session_id.clone()),
            })
            .await?
            .ok_or(RuntimeError::AuthorityNotAdmitted)?;
        let (source, target) = if source_local {
            (&local, &peer)
        } else {
            (&peer, &local)
        };
        if self.pg_native_membership(id)?.is_none()
            && source_local
            && source.scope.configuration.previous.as_ref()
                == Some(&id.source.scope.configuration.current)
        {
            self.pg_install_membership(
                id,
                &source.scope.configuration.current,
                source.scope.configuration.previous.as_ref(),
                false,
            )?;
        }
        let membership = self
            .pg_native_membership(id)?
            .ok_or(RuntimeError::AuthorityNotAdmitted)?;
        let (source_generation, target_generation) = {
            let sessions = self.public_owner().sessions.gate.lock().unwrap();
            let entry = &sessions.entries[&id.nonce];
            (entry.source_generation, entry.target_generation)
        };
        for (observed, origin) in [(source, &id.source), (target, &id.target)] {
            let source_endpoint = origin == &id.source;
            let current_seen = if source_endpoint {
                membership.source_current_seen
            } else {
                membership.target_current_seen
            };
            observed.validate().map_err(application_error)?;
            if observed.scope.storage != origin.scope.storage
                || observed.scope.process_session_id != origin.scope.process_session_id
                || observed.scope.creation_id != origin.scope.creation_id
                || observed.scope.history != origin.scope.history
                || observed.scope.epoch != origin.scope.epoch
                || observed.replication_address != origin.replication_address
                || observed.generation
                    < if source_endpoint {
                        source_generation
                    } else {
                        target_generation
                    }
            {
                return Err(RuntimeError::AuthorityNotAdmitted);
            }
            if observed == origin && !membership.achieved && !current_seen {
                continue;
            }
            let configuration = &observed.scope.configuration;
            if configuration.current != membership.current
                || !(configuration.previous.as_ref() == Some(&membership.previous)
                    || configuration.previous.is_none())
                || ((membership.achieved || current_seen) && configuration.previous.is_some())
                || (observed.stage == ReplicaPublicationStage::AchievedScope
                    && observed.role
                        != if origin == &id.source {
                            ReplicaRole::Primary
                        } else {
                            ReplicaRole::ActiveSecondary
                        })
            {
                return Err(RuntimeError::AuthorityNotAdmitted);
            }
            let other = if origin == &id.source {
                &id.target
            } else {
                &id.source
            };
            if !configuration.replicas.iter().any(|replica| {
                replica.identity == other.scope.storage.replica
                    && replica.storage_incarnation == other.scope.storage
                    && replica.process_session_id == other.scope.process_session_id
                    && replica.replication_address == other.replication_address
            }) {
                return Err(RuntimeError::AuthorityNotAdmitted);
            }
        }
        self.public_owner().sessions.revalidate(id, generation)?;
        let both_current_achieved = [source, target].iter().all(|publication| {
            publication.stage == ReplicaPublicationStage::AchievedScope
                && publication.scope.configuration.current == membership.current
                && publication.scope.configuration.previous.is_none()
        });
        {
            let mut sessions = self.public_owner().sessions.gate.lock().unwrap();
            let entry = sessions.entries.get_mut(&id.nonce).unwrap();
            entry.source_generation = entry.source_generation.max(source.generation);
            entry.target_generation = entry.target_generation.max(target.generation);
            let membership = entry.membership.as_mut().unwrap();
            membership.source_current_seen |= source.scope.configuration.current
                == membership.current
                && source.scope.configuration.previous.is_none();
            membership.target_current_seen |= target.scope.configuration.current
                == membership.current
                && target.scope.configuration.previous.is_none();
            membership.achieved |= both_current_achieved;
        }
        Ok(())
    }

    pub(super) async fn pg_native_membership_tick(&self) -> Result<()> {
        if self.public_owner().host.get().is_none()
            || self.cancellation.is_cancelled()
            || self
                .public_owner()
                .closing
                .load(std::sync::atomic::Ordering::Acquire)
        {
            return Ok(());
        }
        let id = {
            let sessions = self.public_owner().sessions.gate.lock().unwrap();
            sessions.receiver.and_then(|nonce| {
                let entry = &sessions.entries[&nonce];
                (entry.stage == SessionStage::Completed
                    && entry.native_copy_complete
                    && !entry.cancellation.is_cancelled())
                .then(|| entry.id.clone())
            })
        };
        let Some(id) = id else {
            return Ok(());
        };
        let context = self.public_owner().fence.context()?;
        if context.role != ReplicaRole::ActiveSecondary || context.epoch != id.target.scope.epoch {
            return Ok(());
        }
        let mut sessions = self.public_owner().sessions.gate.lock().unwrap();
        let entry = sessions.entries.get_mut(&id.nonce).unwrap();
        if entry.membership_worker.is_some() {
            return Ok(());
        }
        let cancellation = entry.cancellation.clone();
        let driver = self
            .public_owner()
            .driver
            .get()
            .and_then(Weak::upgrade)
            .ok_or(RuntimeError::Closed)?;
        let task = tokio::spawn(async move {
            loop {
                let result = tokio::select! {
                    biased;
                    _ = cancellation.cancelled() => break,
                    result = driver.pg_native_membership_step(&id) => result,
                };
                if let Err(error) = result {
                    tracing::error!(%error, "native PostgreSQL continuation quarantined");
                    driver.pg_quarantine_session(&id);
                    if let Err(error) = driver.instance.stop().await {
                        driver
                            .public_owner()
                            .sessions
                            .gate
                            .lock()
                            .unwrap()
                            .entries
                            .get_mut(&id.nonce)
                            .unwrap()
                            .native_failure = Some(error.to_string());
                    }
                    break;
                }
                tokio::select! {
                    biased;
                    _ = cancellation.cancelled() => break,
                    _ = tokio::time::sleep(Duration::from_millis(20)) => {},
                }
            }
        });
        let task = crate::owned_task::PgTask::new(task);
        entry.joins.push(task.clone());
        entry.membership_worker = Some(task);
        Ok(())
    }

    async fn pg_native_membership_step(&self, id: &PgSessionId) -> Result<()> {
        #[cfg(all(test, feature = "testing"))]
        self.native_cut("membership-first-observation").await;
        let (generation, cancellation) = self.public_owner().sessions.lease(id)?;
        let response = self
            .public_read(
                &cancellation,
                self.public_rpc(
                    &id.source.replication_address,
                    &Request::Membership(Box::new(id.clone())),
                ),
            )
            .await?;
        let Response::Membership {
            session,
            mut membership,
            binding,
            credential,
            lineage,
            boundary,
        } = response
        else {
            return Err(RuntimeError::AuthorityNotAdmitted);
        };
        if session.as_ref() != id
            || binding.session.as_deref() != Some(id)
            || !membership.installed
            || membership.previous != id.source.scope.configuration.current
            || boundary < 0
        {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        self.public_owner().sessions.revalidate(id, generation)?;
        {
            let mut sessions = self.public_owner().sessions.gate.lock().unwrap();
            let entry = sessions.entries.get_mut(&id.nonce).unwrap();
            if let Some(old) = &entry.membership {
                if old.current != membership.current
                    || old.previous != membership.previous
                    || (old.current_only && !membership.current_only)
                {
                    return Err(RuntimeError::AuthorityNotAdmitted);
                }
                membership.achieved |= old.achieved;
                membership.source_current_seen |= old.source_current_seen;
                membership.target_current_seen |= old.target_current_seen;
            }
            entry.membership = Some(membership);
        }
        self.pg_session_publications(id).await?;
        let _serial = self.public_owner().inbound_serial.lock().await;
        let mut state = self.public_owner().state.lock().await;
        let transport = TransportBinding::Peer(binding.clone());
        if state.receiver.as_ref() == Some(&transport) {
            return self.pg_validate_native_session(&transport, true).await;
        }
        self.pg_validate_native_session(&transport, true).await?;
        credential.conninfo()?;
        lineage.validate().map_err(application_error)?;
        self.pg_validate_native_session(&transport, true).await?;
        self.public_validate_history().await?;
        if self.public_lineage().await? != lineage {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        if state.receiver.as_ref().is_some_and(|receiver| {
            receiver.session() == Some(id) && matches!(receiver, TransportBinding::Peer(_))
        }) {
            self.public_owner().fence.admit_peer(&binding)?;
            state.receiver = Some(transport);
            return Ok(());
        }
        self.public_owned_with(cancellation.clone(), async {
            self.pg_validate_native_session(&transport, true).await?;
            self.instance.stop().await.map_err(application_error)?;
            self.pg_validate_native_session(&transport, true).await?;
            self.instance
                .config()
                .public_receiver(
                    self.instance.data_dir(),
                    &credential.conninfo()?,
                    &crate::native::replication_slot_name(transport.target()),
                    &lineage,
                )
                .await
                .map_err(application_error)?;
            self.pg_validate_native_session(&transport, true).await?;
            self.public_owner().fence.admit_peer(&binding)?;
            self.public_start().await?;
            self.pg_validate_native_session(&transport, true).await?;
            state.receiver = Some(transport);
            state.receiver_boundary = Some(boundary);
            Ok(())
        })
        .await
    }

    async fn pg_session_rpc(
        &self,
        id: &PgSessionId,
        action: SessionAction,
        source: bool,
    ) -> Result<SessionReply> {
        let peer = if source { &id.source } else { &id.target };
        let response = self
            .public_rpc(
                &peer.replication_address,
                &Request::Session(Box::new(SessionRequest {
                    session: id.clone(),
                    action,
                })),
            )
            .await?;
        match response {
            Response::Session(reply)
                if reply.session == *id && reply.responder == *peer && reply.action == action =>
            {
                Ok(*reply)
            }
            _ => Err(RuntimeError::AuthorityNotAdmitted),
        }
    }

    pub(super) async fn pg_propose_session(
        &self,
        source: ReplicaPublication,
        target: ReplicaPublication,
    ) -> Result<PgSessionId> {
        let nonce = super::super::public_transport::random_bytes()?;
        let mut id = PgSessionId {
            nonce,
            generation: 1,
            source,
            target,
        };
        self.pg_session_publications(&id).await?;
        if id.source.scope.storage.replica != self.public_owner().fence.local.identity {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        let mut sessions = self.public_owner().sessions.gate.lock().unwrap();
        if sessions.fenced || sessions.blocked {
            return Err(RuntimeError::Closed);
        }
        if sessions.entries.values().any(|entry| {
            entry.id.target.scope.storage.replica.replica_id
                == id.target.scope.storage.replica.replica_id
                && !(entry.remote_settled
                    && entry.stage == SessionStage::Retired
                    && entry.cleanup.as_ref().is_some_and(|result| result.is_ok()))
        }) {
            return Err(application_error(
                "PostgreSQL target already has an unsettled session owner",
            ));
        }
        sessions.generation = sessions
            .generation
            .checked_add(1)
            .ok_or_else(|| application_error("PostgreSQL session generation exhausted"))?;
        id.generation = sessions.generation;
        PgSessions::record(&mut sessions, &id, SessionStage::Proposed)?;
        Ok(id)
    }

    pub(super) async fn pg_establish_session(&self, id: &PgSessionId) -> Result<()> {
        if id.source.scope.storage.replica != self.public_owner().fence.local.identity {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        let (generation, _) = self.public_owner().sessions.lease(id)?;
        self.pg_session_publications(id).await?;
        let result = self.pg_session_rpc(id, SessionAction::Reserve, false).await;
        self.public_owner().sessions.revalidate(id, generation)?;
        match result {
            Ok(reply)
                if matches!(
                    reply.stage,
                    SessionStage::Receiving | SessionStage::Completed
                ) =>
            {
                self.pg_session_publications(id).await?;
                self.public_owner().sessions.revalidate(id, generation)
            }
            _ => {
                self.pg_quarantine_session(id);
                Err(RuntimeError::AuthorityNotAdmitted)
            }
        }
    }

    async fn pg_reserve_session(&self, id: &PgSessionId) -> Result<()> {
        let operation = {
            let mut sessions = self.public_owner().sessions.gate.lock().unwrap();
            if sessions.fenced
                || sessions.blocked
                || sessions.receiver.is_some_and(|nonce| nonce != id.nonce)
            {
                return Err(RuntimeError::AuthorityNotAdmitted);
            }
            let entry = PgSessions::record(&mut sessions, id, SessionStage::Reserved)?;
            if !matches!(
                entry.stage,
                SessionStage::Reserved | SessionStage::Receiving | SessionStage::Completed
            ) {
                return Err(RuntimeError::AuthorityNotAdmitted);
            }
            let operation = if let Some(operation) = entry.reservation.as_ref() {
                operation.clone()
            } else {
                let generation = entry.effect_generation;
                let cancellation = entry.cancellation.clone();
                let driver = self
                    .public_owner()
                    .driver
                    .get()
                    .and_then(Weak::upgrade)
                    .ok_or(RuntimeError::Closed)?;
                let id = id.clone();
                let (sender, result) = tokio::sync::watch::channel(None);
                let task = tokio::spawn(async move {
                    let result = driver
                        .pg_finish_reservation(&id, generation, cancellation)
                        .await;
                    if result.is_err() {
                        driver.pg_quarantine_session(&id);
                    }
                    sender.send_replace(Some(result.map_err(|error| error.to_string())));
                });
                let operation = CloseOwner {
                    result,
                    task: crate::owned_task::PgTask::new(task),
                };
                entry.reservation = Some(operation.clone());
                operation
            };
            sessions.receiver = Some(id.nonce);
            operation
        };
        let mut result = operation.result;
        loop {
            let completed = result.borrow().clone();
            if let Some(result) = completed {
                operation.task.await.map_err(application_error)?;
                result.map_err(application_error)?;
                break;
            }
            if result.changed().await.is_err() {
                operation.task.await.map_err(application_error)?;
                return Err(application_error("lost PostgreSQL reservation owner"));
            }
        }
        let (generation, cancellation) = self.public_owner().sessions.lease(id)?;
        let confirmed = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Err(RuntimeError::OperationCancelled),
            confirmed = self.pg_session_rpc(id, SessionAction::Confirm, true) => confirmed,
        };
        if !confirmed
            .as_ref()
            .is_ok_and(|reply| reply.stage == SessionStage::Active)
        {
            self.pg_quarantine_session(id);
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        self.pg_session_publications(id).await?;
        self.public_owner().sessions.revalidate(id, generation)
    }

    async fn pg_finish_reservation(
        &self,
        id: &PgSessionId,
        generation: u64,
        cancellation: CancellationToken,
    ) -> Result<()> {
        let confirmed = tokio::select! {
            biased;
            _ = cancellation.cancelled() => return Err(RuntimeError::OperationCancelled),
            confirmed = self.pg_session_rpc(id, SessionAction::Activate, true) => confirmed?,
        };
        if confirmed.stage != SessionStage::Active {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        self.pg_session_publications(id).await?;
        let mut sessions = self.public_owner().sessions.gate.lock().unwrap();
        if sessions.fenced || sessions.blocked {
            return Err(RuntimeError::Closed);
        }
        let entry = sessions.entries.get_mut(&id.nonce).unwrap();
        if entry.effect_generation != generation
            || entry.stage != SessionStage::Reserved
            || entry.cancellation.is_cancelled()
        {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        entry.stage = SessionStage::Receiving;
        Ok(())
    }

    pub(super) fn pg_quarantine_session(&self, id: &PgSessionId) {
        let mut sessions = self.public_owner().sessions.gate.lock().unwrap();
        if let Some(entry) = sessions.entries.get_mut(&id.nonce)
            && entry.id == *id
            && (entry.stage != SessionStage::Retired || !entry.remote_settled)
        {
            entry.stage = SessionStage::Quarantined;
            entry.cancellation.cancel();
        }
    }

    #[cfg(all(test, feature = "testing"))]
    pub(super) async fn pg_retire_session(&self, id: &PgSessionId) -> Result<()> {
        self.pg_start_retirement(id)?
            .wait("lost PostgreSQL two-ended retirement owner")
            .await
    }

    pub(super) async fn pg_remove_native_session(&self, id: &PgSessionId) -> Result<()> {
        self.pg_start_retirement(id)?;
        self.pg_retire_local(id).await
    }

    pub(super) fn pg_retire_native_now(&self, id: &PgSessionId) -> Result<()> {
        self.pg_start_retirement(id).map(|_| ())
    }

    fn pg_start_retirement(&self, id: &PgSessionId) -> Result<CloseOwner> {
        let mut sessions = self.public_owner().sessions.gate.lock().unwrap();
        let entry = sessions
            .entries
            .get_mut(&id.nonce)
            .ok_or(RuntimeError::AuthorityNotAdmitted)?;
        if entry.id != *id {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        if let Some(operation) = &entry.remote_retirement {
            let retry = operation
                .result
                .borrow()
                .as_ref()
                .is_some_and(|result| result.is_err())
                && operation.task.settled_successfully();
            if !retry {
                return Ok(operation.clone());
            }
        }
        if entry.stage == SessionStage::Retired && entry.remote_settled {
            return entry
                .retirement
                .clone()
                .ok_or(RuntimeError::AuthorityNotAdmitted);
        }
        entry.stage = SessionStage::Retiring;
        entry.effect_generation = entry.effect_generation.saturating_add(1);
        entry.cancellation.cancel();
        let driver = self
            .public_owner()
            .driver
            .get()
            .and_then(Weak::upgrade)
            .ok_or(RuntimeError::Closed)?;
        let retired = id.clone();
        let (sender, result) = tokio::sync::watch::channel(None);
        let task = tokio::spawn(async move {
            let result = driver.pg_retire_two_ended(&retired).await;
            if result.is_err() {
                driver.pg_quarantine_session(&retired);
            }
            sender.send_replace(Some(result.map_err(|error| error.to_string())));
        });
        let operation = CloseOwner {
            result,
            task: crate::owned_task::PgTask::new(task),
        };
        entry.remote_retirement = Some(operation.clone());
        Ok(operation)
    }

    async fn pg_retire_two_ended(&self, id: &PgSessionId) -> Result<()> {
        let local = self.pg_retire_local(id).await;
        if self.public_owner().sessions.gate.lock().unwrap().entries[&id.nonce].remote_settled {
            self.public_owner()
                .sessions
                .gate
                .lock()
                .unwrap()
                .entries
                .get_mut(&id.nonce)
                .unwrap()
                .stage = if local.is_ok() {
                SessionStage::Retired
            } else {
                SessionStage::Quarantined
            };
            return local;
        }
        self.pg_quarantine_session(id);
        let source = self.public_owner().fence.local.identity == id.target.scope.storage.replica;
        for attempt in 0..2 {
            let action = if local.is_ok() {
                SessionAction::Retire
            } else {
                SessionAction::Cancel
            };
            let result = self.pg_session_rpc(id, action, source).await;
            if let Err(error) = &result {
                tracing::warn!(%error, attempt, "exact PostgreSQL peer retirement was not acknowledged");
            }
            if result.is_ok_and(|reply| reply.stage == SessionStage::Retired)
                || self.public_owner().sessions.gate.lock().unwrap().entries[&id.nonce]
                    .remote_settled
            {
                let mut sessions = self.public_owner().sessions.gate.lock().unwrap();
                let entry = sessions.entries.get_mut(&id.nonce).unwrap();
                entry.stage = if local.is_ok() {
                    SessionStage::Retired
                } else {
                    SessionStage::Quarantined
                };
                entry.remote_settled = true;
                return local;
            }
            if attempt == 0 {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
        Err(application_error(match local {
            Ok(()) => "PostgreSQL remote retirement unresolved; session quarantined".to_owned(),
            Err(error) => {
                format!("{error}; PostgreSQL remote retirement unresolved; session quarantined")
            }
        }))
    }

    async fn pg_retire_local(&self, id: &PgSessionId) -> Result<()> {
        id.validate()?;
        let known = self
            .public_owner()
            .sessions
            .gate
            .lock()
            .unwrap()
            .entries
            .get(&id.nonce)
            .is_some_and(|entry| entry.id == *id);
        if !known {
            self.pg_validate_retirement_identity(id).await?;
        }
        let local = &self.public_owner().fence.local;
        let endpoint = if id.source.scope.storage.replica == local.identity {
            &id.source
        } else {
            &id.target
        };
        if endpoint.scope.storage.replica != local.identity
            || endpoint.scope.process_session_id != local.process_session_id
            || endpoint.replication_address != local.replication_address
        {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        let operation = {
            let mut sessions = self.public_owner().sessions.gate.lock().unwrap();
            let entry = PgSessions::record(&mut sessions, id, SessionStage::Retiring)?;
            if let Some(operation) = &entry.retirement {
                operation.clone()
            } else {
                entry.stage = SessionStage::Retiring;
                entry.effect_generation = entry.effect_generation.saturating_add(1);
                entry.cancellation.cancel();
                let mut joins = entry.joins.clone();
                if let Some(reservation) = &entry.reservation {
                    joins.push(reservation.task.clone());
                }
                let driver = self
                    .public_owner()
                    .driver
                    .get()
                    .and_then(Weak::upgrade)
                    .ok_or(RuntimeError::Closed)?;
                let retired = id.clone();
                let (sender, result) = tokio::sync::watch::channel(None);
                let task = tokio::spawn(async move {
                    let outcome = driver.pg_finish_retirement(&retired, joins).await;
                    sender.send_replace(Some(outcome.map_err(|error| error.to_string())));
                });
                let operation = CloseOwner {
                    task: crate::owned_task::PgTask::new(task),
                    result,
                };
                entry.retirement = Some(operation.clone());
                operation
            }
        };
        let mut observed = operation.result;
        loop {
            let result = observed.borrow().clone();
            if let Some(result) = result {
                operation.task.await.map_err(application_error)?;
                return result.map_err(application_error);
            }
            if observed.changed().await.is_err() {
                operation.task.await.map_err(application_error)?;
                return Err(application_error(
                    "lost PostgreSQL session retirement owner",
                ));
            }
        }
    }

    async fn pg_validate_retirement_identity(&self, id: &PgSessionId) -> Result<()> {
        let local = &self.public_owner().fence.local;
        let expected = if id.source.scope.storage.replica == local.identity {
            &id.source
        } else {
            &id.target
        };
        let matches = |current: &ReplicaPublication| {
            current.scope.storage == expected.scope.storage
                && current.scope.creation_id == expected.scope.creation_id
                && current.scope.process_session_id == expected.scope.process_session_id
                && current.scope.history == expected.scope.history
                && current.replication_address == expected.replication_address
        };
        #[cfg(all(test, feature = "testing"))]
        if let Some((source, target)) = self
            .public_owner()
            .session_publications
            .lock()
            .unwrap()
            .as_ref()
        {
            let current = if source.scope.storage.replica == local.identity {
                source
            } else {
                target
            };
            return if matches(current) {
                Ok(())
            } else {
                Err(RuntimeError::AuthorityNotAdmitted)
            };
        }
        let host = self
            .public_owner()
            .host
            .get()
            .ok_or(RuntimeError::AuthorityNotAdmitted)?;
        let current = host
            .local_publication()
            .await?
            .ok_or(RuntimeError::AuthorityNotAdmitted)?;
        // Cancellation may follow source withdrawal; it is not fresh establishment.
        if !matches(&current) {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        Ok(())
    }

    async fn pg_finish_retirement(
        &self,
        id: &PgSessionId,
        joins: Vec<crate::owned_task::PgTask>,
    ) -> Result<()> {
        let mut failures = Vec::new();
        if let Err(error) = self.pg_cleanup_native_session(id).await {
            failures.push(error.to_string());
        }
        for task in joins {
            if let Err(error) = task.await {
                failures.push(error.to_string());
            }
        }
        if let Some(error) = self
            .public_owner()
            .sessions
            .gate
            .lock()
            .unwrap()
            .entries
            .get(&id.nonce)
            .and_then(|entry| entry.native_failure.clone())
        {
            failures.push(error);
        }
        let result = if failures.is_empty() {
            Ok(())
        } else {
            Err(failures.join("; "))
        };
        let mut sessions = self.public_owner().sessions.gate.lock().unwrap();
        let entry = sessions.entries.get_mut(&id.nonce).unwrap();
        entry.stage = if result.is_ok() {
            SessionStage::Retired
        } else {
            SessionStage::Quarantined
        };
        entry.cleanup = Some(result.clone());
        if result.is_ok() && sessions.receiver == Some(id.nonce) {
            sessions.receiver = None;
        }
        result.map_err(application_error)
    }

    pub(super) async fn pg_join_sessions(&self) -> Result<()> {
        let ids = self
            .public_owner()
            .sessions
            .gate
            .lock()
            .unwrap()
            .entries
            .values()
            .map(|entry| entry.id.clone())
            .collect::<Vec<_>>();
        let mut failures = Vec::new();
        let mut started = Vec::new();
        for id in ids {
            match self.pg_start_retirement(&id) {
                Ok(operation) => started.push(operation),
                Err(error) => failures.push(error.to_string()),
            }
        }
        for operation in started {
            if let Err(error) = operation
                .wait("lost PostgreSQL retained session retirement")
                .await
            {
                failures.push(error.to_string());
            }
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(application_error(failures.join("; ")))
        }
    }

    pub(super) async fn pg_session_request(&self, request: SessionRequest) -> Result<SessionReply> {
        let SessionRequest {
            session: id,
            action,
        } = request;
        id.validate()?;
        if matches!(action, SessionAction::Retire | SessionAction::Cancel) {
            #[cfg(all(test, feature = "testing"))]
            if self
                .public_owner()
                .retirement_unavailable
                .load(std::sync::atomic::Ordering::SeqCst)
            {
                return Err(RuntimeError::OperationCancelled);
            }
            self.pg_retire_local(&id).await?;
            #[cfg(all(test, feature = "testing"))]
            {
                let gate = self
                    .public_owner()
                    .session_retirement_gate
                    .lock()
                    .unwrap()
                    .take();
                if let Some(gate) = gate {
                    gate.entered.notify_one();
                    gate.release.notified().await;
                }
            }
            if action == SessionAction::Retire {
                self.public_owner()
                    .sessions
                    .gate
                    .lock()
                    .unwrap()
                    .entries
                    .get_mut(&id.nonce)
                    .unwrap()
                    .remote_settled = true;
            }
        } else {
            #[cfg(all(test, feature = "testing"))]
            if matches!(action, SessionAction::Activate | SessionAction::Confirm) {
                let gate = self
                    .public_owner()
                    .session_confirmation_gate
                    .lock()
                    .unwrap()
                    .take();
                if let Some(gate) = gate {
                    gate.entered.notify_one();
                    gate.release.notified().await;
                }
            }
            self.pg_session_publications(&id).await?;
            let local_source =
                id.source.scope.storage.replica == self.public_owner().fence.local.identity;
            if local_source && matches!(action, SessionAction::Activate | SessionAction::Confirm) {
                let (generation, _) = self.public_owner().sessions.lease(&id)?;
                self.pg_native_lineage_live(&id, generation).await?;
                self.pg_session_publications(&id).await?;
            }
            match action {
                SessionAction::Activate | SessionAction::Confirm if local_source => {
                    let mut sessions = self.public_owner().sessions.gate.lock().unwrap();
                    if sessions.fenced || sessions.blocked {
                        return Err(RuntimeError::Closed);
                    }
                    let entry = sessions
                        .entries
                        .get_mut(&id.nonce)
                        .ok_or(RuntimeError::AuthorityNotAdmitted)?;
                    if entry.id != id
                        || !matches!(entry.stage, SessionStage::Proposed | SessionStage::Active)
                        || (action == SessionAction::Confirm && entry.stage != SessionStage::Active)
                    {
                        return Err(RuntimeError::AuthorityNotAdmitted);
                    }
                    entry.stage = SessionStage::Active;
                }
                SessionAction::Reserve if !local_source => {
                    self.pg_reserve_session(&id).await?;
                }
                SessionAction::Complete if !local_source => {
                    let (generation, _) = self.public_owner().sessions.lease(&id)?;
                    let confirmed = self.pg_session_rpc(&id, SessionAction::Confirm, true).await;
                    if !confirmed
                        .as_ref()
                        .is_ok_and(|reply| reply.stage == SessionStage::Active)
                    {
                        self.pg_quarantine_session(&id);
                        return Err(RuntimeError::AuthorityNotAdmitted);
                    }
                    self.pg_session_publications(&id).await?;
                    let mut sessions = self.public_owner().sessions.gate.lock().unwrap();
                    if sessions.fenced || sessions.blocked {
                        return Err(RuntimeError::Closed);
                    }
                    let entry = sessions.entries.get_mut(&id.nonce).unwrap();
                    if entry.effect_generation != generation
                        || !matches!(
                            entry.stage,
                            SessionStage::Receiving | SessionStage::Completed
                        )
                        || entry.cancellation.is_cancelled()
                    {
                        return Err(RuntimeError::AuthorityNotAdmitted);
                    }
                    entry.stage = SessionStage::Completed;
                }
                _ => return Err(RuntimeError::AuthorityNotAdmitted),
            }
        }
        let sessions = self.public_owner().sessions.gate.lock().unwrap();
        let entry = sessions
            .entries
            .get(&id.nonce)
            .ok_or(RuntimeError::AuthorityNotAdmitted)?;
        let responder =
            if id.source.scope.storage.replica == self.public_owner().fence.local.identity {
                id.source.clone()
            } else {
                id.target.clone()
            };
        Ok(SessionReply {
            session: id,
            responder,
            action,
            stage: if matches!(action, SessionAction::Retire | SessionAction::Cancel)
                && entry.cleanup.as_ref().is_some_and(|result| result.is_ok())
            {
                SessionStage::Retired
            } else {
                entry.stage
            },
        })
    }
}
