//! Creation-owned transfer identity and admission. Native routing is not authority.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::task::Poll;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::protocol::public_operations::{
    RemoteReplicaSelection, ReplicaPublication, ReplicaPublicationStage,
};
use crate::protocol::types::{
    Epoch, HistoryContext, ProcessSessionId, ReplicaIdentity, ReplicaRole,
    ReplicaStorageIncarnationContext, ReplicatorCreationId, ResourceUid,
};
use crate::{Result, RuntimeError};

const MAX_SESSIONS: usize = 256;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct Endpoint {
    pub(super) identity: ReplicaIdentity,
    pub(super) process_session_id: ProcessSessionId,
    pub(super) storage: ReplicaStorageIncarnationContext,
    pub(super) creation_id: ReplicatorCreationId,
    pub(super) address: String,
}

impl Endpoint {
    pub(super) fn publication(publication: &ReplicaPublication) -> Self {
        Self {
            identity: publication.scope.storage.replica.clone(),
            process_session_id: publication.scope.process_session_id.clone(),
            storage: publication.scope.storage.clone(),
            creation_id: publication.scope.creation_id.clone(),
            address: publication.replication_address.clone(),
        }
    }

    pub(super) fn validate(&self) -> Result<()> {
        self.storage
            .validate()
            .map_err(|error| RuntimeError::AuthorityMismatch(error.into()))?;
        if self.storage.replica != self.identity
            || self.identity.replica_id.value() <= 0
            || self.identity.instance_id.is_empty()
            || self.identity.agent_generation.is_empty()
            || self.process_session_id.is_empty()
            || self.creation_id.is_empty()
            || self.address.is_empty()
        {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        Ok(())
    }

    pub(super) fn incarnation(&self) -> Result<[u8; 16]> {
        self.validate()?;
        Ok(guid(
            b"kuberic-v1-engine-endpoint",
            &serde_json::to_vec(self)
                .map_err(|error| RuntimeError::InvalidReplication(error.to_string()))?,
        ))
    }
}

pub(super) fn partition(resource: &ResourceUid) -> [u8; 16] {
    guid(b"kuberic-v1-engine-partition", resource.as_str().as_bytes())
}

fn guid(domain: &[u8], bytes: &[u8]) -> [u8; 16] {
    let mut hash = Sha256::new();
    hash.update((domain.len() as u64).to_be_bytes());
    hash.update(domain);
    hash.update(bytes);
    let mut result = [0; 16];
    result.copy_from_slice(&hash.finalize()[..16]);
    result
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(in crate::replicator::v1) struct Binding {
    pub(super) nonce: String,
    pub(super) source: ReplicaPublication,
    pub(super) target: ReplicaPublication,
    pub(super) recovery: bool,
}

impl Binding {
    pub(super) fn admitted_reconfiguration(&self) -> bool {
        !self.recovery
            && self
                .source
                .scope
                .configuration
                .current
                .members
                .iter()
                .any(|member| {
                    member.identity == self.target.scope.storage.replica
                        && member.role == ReplicaRole::ActiveSecondary
                })
    }

    pub(super) fn new(source: ReplicaPublication, target: ReplicaPublication) -> Result<Self> {
        let binding = Self {
            nonce: uuid::Uuid::new_v4().to_string(),
            source,
            target,
            recovery: false,
        };
        binding.validate()?;
        Ok(binding)
    }

    pub(super) fn validate(&self) -> Result<()> {
        if uuid::Uuid::parse_str(&self.nonce).is_err() {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        if self.admitted_reconfiguration() {
            for publication in [&self.source, &self.target] {
                publication
                    .validate()
                    .map_err(|error| RuntimeError::AuthorityMismatch(error.into()))?;
            }
            let source = &self.source.scope;
            let target = &self.target.scope;
            let configuration = &source.configuration;
            let selected = |publication: &ReplicaPublication, role| {
                (publication.stage == ReplicaPublicationStage::DiscoveryPending
                    && publication.role == ReplicaRole::None)
                    || (publication.stage == ReplicaPublicationStage::AchievedScope
                        && publication.role == role)
            };
            if !selected(&self.source, ReplicaRole::Primary)
                || !selected(&self.target, ReplicaRole::ActiveSecondary)
                || source.storage.replica == target.storage.replica
                || source.history != target.history
                || source.epoch != target.epoch
                || configuration.current != target.configuration.current
                || configuration.previous != target.configuration.previous
                || configuration.current.primary_id != source.storage.replica.replica_id
                || !configuration.current.members.iter().any(|member| {
                    member.identity == source.storage.replica && member.role == ReplicaRole::Primary
                })
                || ![
                    (&self.source, &self.target, ReplicaRole::ActiveSecondary),
                    (&self.target, &self.source, ReplicaRole::Primary),
                ]
                .into_iter()
                .all(|(local, peer, role)| {
                    local
                        .scope
                        .configuration
                        .replicas
                        .iter()
                        .any(|description| {
                            description.identity == peer.scope.storage.replica
                                && description.storage_incarnation == peer.scope.storage
                                && description.process_session_id == peer.scope.process_session_id
                                && description.replication_address == peer.replication_address
                                && description.role == role
                        })
                })
            {
                return Err(RuntimeError::AuthorityNotAdmitted);
            }
            return Ok(());
        }
        if self.recovery {
            self.source
                .validate()
                .map_err(|e| RuntimeError::AuthorityMismatch(e.into()))?;
            self.target
                .validate()
                .map_err(|e| RuntimeError::AuthorityMismatch(e.into()))?;
            let source = &self.source.scope;
            let target = &self.target.scope;
            let configuration = &source.configuration;
            if self.source.stage != ReplicaPublicationStage::AchievedScope
                || self.source.role != ReplicaRole::Primary
                || !matches!(
                    (self.target.stage, self.target.role),
                    (ReplicaPublicationStage::DiscoveryPending, ReplicaRole::None)
                        | (
                            ReplicaPublicationStage::AchievedScope,
                            ReplicaRole::ActiveSecondary
                        )
                )
                || source.history != target.history
                || source.epoch != target.epoch
                || configuration.previous.is_some()
                || target.configuration.previous.is_some()
                || configuration.current != target.configuration.current
                || configuration.current.primary_id != source.storage.replica.replica_id
                || !configuration.current.members.iter().any(|m| {
                    m.identity == target.storage.replica && m.role == ReplicaRole::ActiveSecondary
                })
                || !configuration.replicas.iter().any(|peer| {
                    peer.identity == target.storage.replica
                        && peer.storage_incarnation == target.storage
                        && peer.process_session_id != target.process_session_id
                })
                || !target.configuration.replicas.iter().any(|peer| {
                    peer.identity == source.storage.replica
                        && peer.storage_incarnation == source.storage
                        && peer.process_session_id == source.process_session_id
                        && peer.replication_address == self.source.replication_address
                })
            {
                return Err(RuntimeError::AuthorityNotAdmitted);
            }
            return Ok(());
        }
        RemoteReplicaSelection {
            source: self.source.clone(),
            target: self.target.clone(),
            target_control_address: "engine-session".into(),
            generation: 1,
        }
        .validate()
        .map_err(|error| RuntimeError::AuthorityMismatch(error.into()))
    }

    pub(super) fn primary(&self) -> Endpoint {
        Endpoint::publication(&self.source)
    }

    pub(super) fn secondary(&self) -> Endpoint {
        Endpoint::publication(&self.target)
    }

    pub(super) fn history(&self) -> &HistoryContext {
        &self.source.scope.history
    }

    pub(super) fn epoch(&self) -> Epoch {
        self.source.scope.epoch
    }

    pub(super) fn exact_publication(&self, publication: &ReplicaPublication) -> bool {
        let origin = if Endpoint::publication(publication) == self.primary() {
            &self.source
        } else if Endpoint::publication(publication) == self.secondary() {
            &self.target
        } else {
            return false;
        };
        (publication.stage == ReplicaPublicationStage::AchievedScope
            || (self.recovery && publication == &self.target)
            || (self.admitted_reconfiguration() && publication == origin))
            && publication.scope == origin.scope
            && publication.role == origin.role
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) enum Control {
    Reserve,
    Activate,
    Confirm,
    Complete,
    Retire,
    Recover,
    Recovered,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct ControlEnvelope {
    pub(super) binding: Binding,
    pub(super) command: Control,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Proposed,
    Recovering,
    Reserved,
    Active,
    Completed,
    Retiring,
    Retired,
}

struct Entry {
    binding: Binding,
    phase: Phase,
    admitted: Arc<AtomicBool>,
    copy_completed: bool,
    predecessor: Option<String>,
    proposed: bool,
}

#[derive(Default)]
struct State {
    closed: bool,
    entries: BTreeMap<String, Entry>,
    receiver: Option<String>,
}

#[cfg(all(test, feature = "testing"))]
#[derive(Clone, Copy)]
pub(super) enum RemovalCut {
    GateRequested,
    Dispatch { replica: i64, applied: i64 },
}

#[cfg(all(test, feature = "testing"))]
type RemovalHook = Arc<dyn Fn(RemovalCut) + Send + Sync>;

#[derive(Default)]
pub(in crate::replicator::v1) struct Sessions(
    Mutex<State>,
    #[cfg(all(test, feature = "testing"))] pub(super) Mutex<Option<RemovalHook>>,
);

pub(super) struct IdleRemoval<'a> {
    state: MutexGuard<'a, State>,
    binding: &'a Binding,
}

impl IdleRemoval<'_> {
    pub(super) fn retire(mut self, native_idle: bool) -> Result<()> {
        if !native_idle
            && !self
                .state
                .entries
                .get(&self.binding.nonce)
                .is_some_and(|entry| {
                    entry.binding == *self.binding
                        && !self.binding.recovery
                        && !entry.copy_completed
                        && matches!(entry.phase, Phase::Proposed | Phase::Active)
                })
        {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        Sessions::retire_locked(&mut self.state, self.binding, || Ok(()))
    }
}

#[derive(Clone)]
pub(in crate::replicator::v1) struct Fence {
    admitted: Arc<AtomicBool>,
}

impl Fence {
    // Native owners may hold their own locks here. Only effect/retirement
    // admission takes the session gate; native checks never acquire it.
    pub(in crate::replicator::v1) fn check(&self) -> Result<()> {
        if self.admitted.load(Ordering::Acquire) {
            Ok(())
        } else {
            Err(RuntimeError::AuthorityNotAdmitted)
        }
    }

    pub(in crate::replicator::v1) fn stream(
        &self,
        mut stream: crate::application::OperationDataStream,
    ) -> crate::application::OperationDataStream {
        let fence = self.clone();
        let mut in_flight = false;
        Box::pin(futures::stream::poll_fn(move |cx| {
            if !in_flight {
                if fence.check().is_err() {
                    return Poll::Ready(Some(Err(RuntimeError::OperationCancelled)));
                }
                in_flight = true;
            }
            match stream.as_mut().poll_next(cx) {
                Poll::Pending => Poll::Pending,
                Poll::Ready(item) => {
                    in_flight = false;
                    if fence.check().is_err() {
                        Poll::Ready(Some(Err(RuntimeError::OperationCancelled)))
                    } else {
                        Poll::Ready(item)
                    }
                }
            }
        }))
    }
}

impl Sessions {
    #[cfg(all(test, feature = "testing"))]
    pub(super) fn removal_cut(&self, cut: RemovalCut) {
        let hook = self.1.lock().unwrap().clone();
        if let Some(hook) = hook {
            hook(cut);
        }
    }

    pub(super) fn completed(&self, binding: &Binding) -> bool {
        self.0
            .lock()
            .unwrap()
            .entries
            .get(&binding.nonce)
            .is_some_and(|entry| entry.binding == *binding && entry.phase == Phase::Completed)
    }

    pub(super) fn predecessor_completed(&self, binding: &Binding) -> bool {
        self.0
            .lock()
            .unwrap()
            .entries
            .get(&binding.nonce)
            .is_some_and(|entry| {
                entry.binding == *binding
                    && entry.copy_completed
                    && matches!(entry.phase, Phase::Completed | Phase::Retiring)
            })
    }
    #[cfg(feature = "testing")]
    pub(super) fn inspect(&self) -> serde_json::Value {
        let state = self.0.lock().unwrap();
        serde_json::json!({
            "closed": state.closed,
            "capacity": MAX_SESSIONS,
            "receiver": state.receiver,
            "entries": state.entries.values().map(|entry| serde_json::json!({
                "binding": entry.binding,
                "phase": format!("{:?}", entry.phase),
                "admitted": entry.admitted.load(Ordering::Acquire),
            })).collect::<Vec<_>>(),
        })
    }
    fn insert(&self, binding: &Binding, phase: Phase, receiver: bool) -> Result<()> {
        binding.validate()?;
        let mut state = self.0.lock().unwrap();
        if state.closed {
            return Err(RuntimeError::Closed);
        }
        if let Some(entry) = state.entries.get(&binding.nonce) {
            return if entry.binding == *binding
                && !matches!(entry.phase, Phase::Retiring | Phase::Retired)
            {
                Ok(())
            } else {
                Err(RuntimeError::AuthorityNotAdmitted)
            };
        }
        if state.entries.len() >= MAX_SESSIONS
            || (receiver && state.receiver.is_some())
            || state.entries.values().any(|entry| {
                entry.binding.secondary().identity.replica_id
                    == binding.secondary().identity.replica_id
                    && entry.phase != Phase::Retired
            })
        {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        state.entries.insert(
            binding.nonce.clone(),
            Entry {
                binding: binding.clone(),
                phase,
                admitted: Arc::new(AtomicBool::new(false)),
                copy_completed: false,
                predecessor: None,
                proposed: true,
            },
        );
        if receiver {
            state.receiver = Some(binding.nonce.clone());
        }
        Ok(())
    }

    #[cfg(test)]
    pub(super) fn propose(&self, binding: &Binding) -> Result<()> {
        self.insert(binding, Phase::Proposed, false)
    }

    pub(super) fn proposal(&self, binding: Binding) -> Result<Binding> {
        binding.validate()?;
        let mut state = self.0.lock().unwrap();
        if state.closed {
            return Err(RuntimeError::Closed);
        }
        if let Some(entry) = state.entries.values().find(|entry| {
            entry.binding.source.scope == binding.source.scope
                && entry.binding.target.scope == binding.target.scope
                && entry.binding.primary() == binding.primary()
                && entry.binding.secondary() == binding.secondary()
                && matches!(
                    entry.phase,
                    Phase::Proposed | Phase::Active | Phase::Completed
                )
        }) {
            return Ok(entry.binding.clone());
        }
        if state.entries.len() >= MAX_SESSIONS
            || state.entries.values().any(|entry| {
                entry.binding.secondary().identity.replica_id
                    == binding.secondary().identity.replica_id
                    && entry.phase != Phase::Retired
            })
        {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        state.entries.insert(
            binding.nonce.clone(),
            Entry {
                binding: binding.clone(),
                phase: Phase::Proposed,
                admitted: Arc::new(AtomicBool::new(false)),
                copy_completed: false,
                predecessor: None,
                proposed: true,
            },
        );
        Ok(binding)
    }

    pub(super) fn reserve_recovery(&self, binding: &Binding) -> Result<Binding> {
        binding.validate()?;
        let mut state = self.0.lock().unwrap();
        if !binding.recovery || state.closed {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        if let Some(entry) = state.entries.get(&binding.nonce) {
            return entry
                .predecessor
                .as_ref()
                .filter(|_| entry.binding == *binding)
                .and_then(|nonce| state.entries.get(nonce))
                .map(|entry| entry.binding.clone())
                .ok_or(RuntimeError::AuthorityNotAdmitted);
        }
        let old = state
            .entries
            .values()
            .find(|entry| {
                entry.binding.secondary().identity.replica_id
                    == binding.secondary().identity.replica_id
                    && entry.copy_completed
                    && matches!(entry.phase, Phase::Completed | Phase::Retiring)
            })
            .map(|entry| entry.binding.clone())
            .ok_or(RuntimeError::AuthorityNotAdmitted)?;
        let endpoint = old.secondary();
        if state.entries.len() >= MAX_SESSIONS
            || old.primary() != binding.primary()
            || old.history() != binding.history()
            || old.epoch() != binding.epoch()
            || endpoint.storage != binding.secondary().storage
            || endpoint.process_session_id == binding.secondary().process_session_id
            || endpoint.creation_id == binding.secondary().creation_id
            || !binding
                .source
                .scope
                .configuration
                .replicas
                .iter()
                .any(|peer| {
                    peer.identity == endpoint.identity
                        && peer.process_session_id == endpoint.process_session_id
                        && peer.replication_address == endpoint.address
                        && peer.storage_incarnation == endpoint.storage
                })
            || state.entries.values().any(|entry| {
                entry.binding.nonce != old.nonce
                    && entry.binding.secondary().identity.replica_id
                        == binding.secondary().identity.replica_id
                    && entry.phase != Phase::Retired
            })
        {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        // This bounded entry owns both the retained result and the only worker
        // allowed to replace this predecessor. Failed attempts are not evicted.
        state.entries.insert(
            binding.nonce.clone(),
            Entry {
                binding: binding.clone(),
                phase: Phase::Recovering,
                admitted: Arc::new(AtomicBool::new(false)),
                copy_completed: false,
                predecessor: Some(old.nonce.clone()),
                proposed: false,
            },
        );
        Ok(old)
    }

    pub(super) fn retained_recovery(&self, binding: &Binding) -> Result<()> {
        let state = self.0.lock().unwrap();
        state
            .entries
            .get(&binding.nonce)
            .filter(|entry| entry.binding == *binding && entry.predecessor.is_some())
            .map(|_| ())
            .ok_or(RuntimeError::AuthorityNotAdmitted)
    }

    fn pending_recovery(state: &State, old: &Binding, binding: &Binding) -> Result<()> {
        if state.closed
            || !state.entries.get(&binding.nonce).is_some_and(|entry| {
                entry.binding == *binding
                    && entry.phase == Phase::Recovering
                    && entry.predecessor.as_ref() == Some(&old.nonce)
            })
            || !state.entries.get(&old.nonce).is_some_and(|entry| {
                entry.binding == *old
                    && entry.copy_completed
                    && matches!(entry.phase, Phase::Completed | Phase::Retiring)
            })
        {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        Ok(())
    }

    pub(super) fn recovery_pending(&self, old: &Binding, binding: &Binding) -> Result<()> {
        Self::pending_recovery(&self.0.lock().unwrap(), old, binding)
    }

    pub(super) fn recovery_proposal(&self, old: &Binding, binding: Binding) -> Result<Binding> {
        let mut state = self.0.lock().unwrap();
        Self::pending_recovery(&state, old, &binding)?;
        if state.entries.get(&old.nonce).unwrap().phase != Phase::Retiring {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        state.entries.get_mut(&old.nonce).unwrap().phase = Phase::Retired;
        state.entries.get_mut(&binding.nonce).unwrap().phase = Phase::Proposed;
        state.entries.get_mut(&binding.nonce).unwrap().proposed = true;
        Ok(binding)
    }

    pub(super) fn reserve(&self, binding: &Binding) -> Result<()> {
        self.insert(binding, Phase::Reserved, true)
    }

    pub(super) fn activate_source(&self, binding: &Binding) -> Result<()> {
        self.transition(binding, Phase::Proposed, Phase::Active)
    }

    pub(super) fn activate_target(&self, binding: &Binding) -> Result<()> {
        self.transition(binding, Phase::Reserved, Phase::Active)
    }

    pub(super) fn complete(&self, binding: &Binding) -> Result<()> {
        self.transition(binding, Phase::Active, Phase::Completed)
    }

    fn transition(&self, binding: &Binding, from: Phase, to: Phase) -> Result<()> {
        let mut state = self.0.lock().unwrap();
        if state.closed {
            return Err(RuntimeError::Closed);
        }
        let entry = state
            .entries
            .get_mut(&binding.nonce)
            .filter(|entry| entry.binding == *binding)
            .ok_or(RuntimeError::AuthorityNotAdmitted)?;
        if entry.phase != from && entry.phase != to {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        entry.phase = to;
        entry.copy_completed |= to == Phase::Completed;
        entry.admitted.store(true, Ordering::Release);
        Ok(())
    }

    pub(super) fn fence(&self, binding: &Binding) -> Fence {
        let state = self.0.lock().unwrap();
        Fence {
            admitted: state
                .entries
                .get(&binding.nonce)
                .filter(|entry| entry.binding == *binding)
                .map(|entry| entry.admitted.clone())
                .unwrap_or_else(|| Arc::new(AtomicBool::new(false))),
        }
    }

    /// The same gate serializes confirmation, local effects and retirement.
    pub(super) fn effect<T>(&self, binding: &Binding, effect: impl FnOnce() -> T) -> Result<T> {
        let state = self.0.lock().unwrap();
        let entry = state
            .entries
            .get(&binding.nonce)
            .filter(|entry| {
                !state.closed
                    && entry.binding == *binding
                    && matches!(entry.phase, Phase::Active | Phase::Completed)
            })
            .ok_or(RuntimeError::AuthorityNotAdmitted)?;
        let _entry = entry;
        Ok(effect())
    }

    pub(super) fn active(&self, binding: &Binding) -> Result<()> {
        self.effect(binding, || ())
    }

    pub(super) fn binding_for(&self, replica: i64) -> Option<Binding> {
        self.0
            .lock()
            .unwrap()
            .entries
            .values()
            .find(|entry| {
                entry.binding.secondary().identity.replica_id.value() == replica
                    && entry.phase != Phase::Retired
                    && entry.proposed
            })
            .map(|entry| entry.binding.clone())
    }

    pub(super) fn inbound(&self) -> Option<Binding> {
        let state = self.0.lock().unwrap();
        state
            .receiver
            .as_ref()
            .and_then(|nonce| state.entries.get(nonce))
            .map(|entry| entry.binding.clone())
    }

    pub(super) fn retire(&self, binding: &Binding) -> Result<()> {
        self.retire_with(binding, || Ok(()))
    }

    pub(super) fn retire_with<T>(
        &self,
        binding: &Binding,
        take: impl FnOnce() -> Result<T>,
    ) -> Result<T> {
        let mut state = self.0.lock().unwrap();
        Self::retire_locked(&mut state, binding, take)
    }

    // Acquire before native selection, just like ingress effect admission.
    // The consuming retirement callback never reacquires the session gate.
    pub(super) fn idle_removal<'a>(&'a self, binding: &'a Binding) -> IdleRemoval<'a> {
        #[cfg(all(test, feature = "testing"))]
        self.removal_cut(RemovalCut::GateRequested);
        IdleRemoval {
            state: self.0.lock().unwrap(),
            binding,
        }
    }

    fn retire_locked<T>(
        state: &mut State,
        binding: &Binding,
        take: impl FnOnce() -> Result<T>,
    ) -> Result<T> {
        if state
            .entries
            .get(&binding.nonce)
            .is_some_and(|entry| entry.binding != *binding)
        {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        if !state.entries.contains_key(&binding.nonce) {
            if state.entries.len() >= MAX_SESSIONS {
                return Err(RuntimeError::AuthorityNotAdmitted);
            }
            binding.validate()?;
        }
        let taken = take()?;
        let entry = state
            .entries
            .entry(binding.nonce.clone())
            .or_insert_with(|| Entry {
                binding: binding.clone(),
                phase: Phase::Retiring,
                admitted: Arc::new(AtomicBool::new(false)),
                copy_completed: false,
                predecessor: None,
                proposed: true,
            });
        if entry.phase != Phase::Retired {
            entry.phase = Phase::Retiring;
        }
        entry.admitted.store(false, Ordering::Release);
        Ok(taken)
    }

    /// Only an actual target join permits this transition, never an RPC timeout.
    pub(super) fn settled(&self, binding: &Binding) -> Result<()> {
        self.transition_retired(binding)
    }

    fn transition_retired(&self, binding: &Binding) -> Result<()> {
        let mut state = self.0.lock().unwrap();
        let entry = state
            .entries
            .get_mut(&binding.nonce)
            .filter(|entry| entry.binding == *binding)
            .ok_or(RuntimeError::AuthorityNotAdmitted)?;
        if !matches!(entry.phase, Phase::Retiring | Phase::Retired) {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        entry.phase = Phase::Retired;
        // The exact binding and its fence stay retired. Only a settled native
        // role transition may release the receiver slot for a different binding.
        Ok(())
    }

    pub(super) fn abort(&self) {
        let mut state = self.0.lock().unwrap();
        state.closed = true;
        Self::retire_entries(&mut state);
    }

    pub(super) fn retire_all(&self) {
        Self::retire_entries(&mut self.0.lock().unwrap());
    }

    pub(super) fn retiring(&self) -> Vec<Binding> {
        self.0
            .lock()
            .unwrap()
            .entries
            .values()
            .filter(|entry| entry.phase == Phase::Retiring)
            .map(|entry| entry.binding.clone())
            .collect()
    }

    pub(super) fn retiring_receiver(&self, binding: &Binding) -> bool {
        let state = self.0.lock().unwrap();
        !state.closed
            && state.receiver.as_ref() == Some(&binding.nonce)
            && state
                .entries
                .get(&binding.nonce)
                .is_some_and(|entry| entry.binding == *binding && entry.phase == Phase::Retiring)
    }

    pub(super) fn role_receiver_settled(&self) -> Result<()> {
        let mut state = self.0.lock().unwrap();
        if let Some(receiver) = &state.receiver {
            let entry = state
                .entries
                .get(receiver)
                .ok_or(RuntimeError::AuthorityNotAdmitted)?;
            if entry.phase == Phase::Retired {
                state.receiver = None;
            }
        }
        Ok(())
    }

    fn retire_entries(state: &mut State) {
        for entry in state.entries.values_mut() {
            if entry.phase != Phase::Retired {
                entry.phase = Phase::Retiring;
            }
            entry.admitted.store(false, Ordering::Release);
        }
    }
}
