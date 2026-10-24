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
            },
        );
        Ok(state.entries.get_mut(&id.nonce).unwrap())
    }

    fn lease(&self, id: &PgSessionId) -> Result<(u64, CancellationToken)> {
        let state = self.gate.lock().unwrap();
        let entry = state
            .entries
            .get(&id.nonce)
            .ok_or(RuntimeError::AuthorityNotAdmitted)?;
        if state.fenced
            || state.blocked
            || &entry.id != id
            || !entry.stage.live()
            || entry.cancellation.is_cancelled()
        {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        Ok((entry.effect_generation, entry.cancellation.clone()))
    }

    fn revalidate(&self, id: &PgSessionId, generation: u64) -> Result<()> {
        let (current, _) = self.lease(id)?;
        if current != generation {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        Ok(())
    }
}

impl PgReplicator {
    pub(super) async fn pg_prepare_build_session(
        &self,
        replica: &ReplicaInformation,
    ) -> Result<()> {
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
        self.pg_establish_session(&id).await
    }

    async fn pg_session_publications(&self, id: &PgSessionId) -> Result<()> {
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
                && !entry.remote_settled
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

    fn pg_quarantine_session(&self, id: &PgSessionId) {
        let mut sessions = self.public_owner().sessions.gate.lock().unwrap();
        if let Some(entry) = sessions.entries.get_mut(&id.nonce)
            && entry.id == *id
            && (entry.stage != SessionStage::Retired || !entry.remote_settled)
        {
            entry.stage = SessionStage::Quarantined;
            entry.cancellation.cancel();
        }
    }

    pub(super) async fn pg_retire_session(&self, id: &PgSessionId) -> Result<()> {
        self.pg_start_retirement(id)?
            .wait("lost PostgreSQL two-ended retirement owner")
            .await
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
        self.pg_retire_local(id).await?;
        if self.public_owner().sessions.gate.lock().unwrap().entries[&id.nonce].remote_settled {
            self.public_owner()
                .sessions
                .gate
                .lock()
                .unwrap()
                .entries
                .get_mut(&id.nonce)
                .unwrap()
                .stage = SessionStage::Retired;
            return Ok(());
        }
        self.pg_quarantine_session(id);
        let source = self.public_owner().fence.local.identity == id.target.scope.storage.replica;
        for attempt in 0..2 {
            let result = self.pg_session_rpc(id, SessionAction::Retire, source).await;
            if result.is_ok_and(|reply| reply.stage == SessionStage::Retired)
                || self.public_owner().sessions.gate.lock().unwrap().entries[&id.nonce]
                    .remote_settled
            {
                let mut sessions = self.public_owner().sessions.gate.lock().unwrap();
                let entry = sessions.entries.get_mut(&id.nonce).unwrap();
                entry.stage = SessionStage::Retired;
                entry.remote_settled = true;
                return Ok(());
            }
            if attempt == 0 {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
        Err(application_error(
            "PostgreSQL remote retirement unresolved; session quarantined",
        ))
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
        for task in joins {
            if let Err(error) = task.await {
                failures.push(error.to_string());
            }
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
                Ok(_) => started.push(id),
                Err(error) => failures.push(error.to_string()),
            }
        }
        for id in started {
            if let Err(error) = self.pg_retire_session(&id).await {
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
        if action == SessionAction::Retire {
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
            self.public_owner()
                .sessions
                .gate
                .lock()
                .unwrap()
                .entries
                .get_mut(&id.nonce)
                .unwrap()
                .remote_settled = true;
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
            stage: entry.stage,
        })
    }
}
