//! Creation-owned transfer identity and admission. Native routing is not authority.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::Poll;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::protocol::public_operations::{
    RemoteReplicaSelection, ReplicaPublication, ReplicaPublicationStage,
};
use crate::protocol::types::{
    Epoch, HistoryContext, ProcessSessionId, ReplicaIdentity, ReplicaStorageIncarnationContext,
    ReplicatorCreationId, ResourceUid,
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
}

impl Binding {
    pub(super) fn new(source: ReplicaPublication, target: ReplicaPublication) -> Result<Self> {
        let binding = Self {
            nonce: uuid::Uuid::new_v4().to_string(),
            source,
            target,
        };
        binding.validate()?;
        Ok(binding)
    }

    pub(super) fn validate(&self) -> Result<()> {
        if uuid::Uuid::parse_str(&self.nonce).is_err() {
            return Err(RuntimeError::AuthorityNotAdmitted);
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
        publication.stage == ReplicaPublicationStage::AchievedScope
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
}

#[derive(Default)]
struct State {
    closed: bool,
    entries: BTreeMap<String, Entry>,
    receiver: Option<String>,
}

#[derive(Default)]
pub(in crate::replicator::v1) struct Sessions(Mutex<State>);

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
    #[cfg(feature = "testing")]
    pub(super) fn inspect(&self) -> serde_json::Value {
        let state = self.0.lock().unwrap();
        serde_json::json!({
            "closed": state.closed,
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
            },
        );
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
        let mut state = self.0.lock().unwrap();
        if !state.entries.contains_key(&binding.nonce) {
            if state.entries.len() >= MAX_SESSIONS {
                return Err(RuntimeError::AuthorityNotAdmitted);
            }
            binding.validate()?;
            state.entries.insert(
                binding.nonce.clone(),
                Entry {
                    binding: binding.clone(),
                    phase: Phase::Retiring,
                    admitted: Arc::new(AtomicBool::new(false)),
                },
            );
        }
        let entry = state
            .entries
            .get_mut(&binding.nonce)
            .filter(|entry| entry.binding == *binding)
            .ok_or(RuntimeError::AuthorityNotAdmitted)?;
        if entry.phase != Phase::Retired {
            entry.phase = Phase::Retiring;
        }
        entry.admitted.store(false, Ordering::Release);
        Ok(())
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
        // A retired native receiver is never reinitialized in this creation.
        // Its reservation and all tombstones stay until the creation is dropped.
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

    fn retire_entries(state: &mut State) {
        for entry in state.entries.values_mut() {
            if entry.phase != Phase::Retired {
                entry.phase = Phase::Retiring;
            }
            entry.admitted.store(false, Ordering::Release);
        }
    }
}
